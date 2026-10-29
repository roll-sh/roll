//! pull_up_simple_subquery's substitution of a FROM subquery's computed outputs
//! (prepjointree.c's pullup_replace_vars_callback, Postgres 18): a subquery over one table
//! is the table itself in the join tree, its select list's expressions evaluated where the
//! query reads them (above the joins), not in the table's scan. Only an output an outer
//! join could null and that wouldn't go NULL with the table's row (a PlaceHolderVar) stays
//! computed in the scan: one without the table's columns, or not strict, or a LATERAL
//! reference to a relation the outer join doesn't null alike.

use super::*;

/// A join tree's leaf scans with their computed outputs taken out: the tree as it becomes,
/// and per column of the tree's row as it was, the expression over the new row it is.
pub(super) struct Lifted {
    pub plan: Plan,
    pub map: Vec<Expr>,
    /// Empty relations (FROM-less subqueries, one-row VALUES) went from the join tree.
    pub results: bool,
}

fn and_all(mut items: Vec<Expr>) -> Option<Expr> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

fn nonstrict(e: &Expr) -> bool {
    crate::sql::binder::optimize::contain_nonstrict(e)
}

/// An expression that may move up the plan: no sub-select, aggregate, set-returning or
/// volatile call, and no reference to a query further out than the join's LATERAL values.
fn movable(e: &Expr) -> bool {
    let mut ok = !e.is_volatile();
    // (A CASE's operand placeholder inside its own WHEN tests moves with the CASE.)
    let e = &e
        .clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Case {
                    operand: Some(o),
                    whens,
                    else_,
                    ty,
                } => Expr::Case {
                    operand: Some(o),
                    whens: whens
                        .into_iter()
                        .map(|(c, r)| {
                            let c = c
                                .transform(&mut |y| {
                                    Ok(match y {
                                        Expr::CaseTest(t) => Expr::Const(Value::Null, t),
                                        y => y,
                                    })
                                })
                                .unwrap();
                            (c, r)
                        })
                        .collect(),
                    else_,
                    ty,
                },
                x => x,
            })
        })
        .unwrap();
    e.walk(&mut |x| match x {
        // (A call of a function that isn't volatile is a function call: it moves.)
        Expr::Subquery(s) if s.call.is_some() && matches!(s.func, Some('i' | 's')) => {}
        Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Grouping(_) | Expr::Srf(..) => {
            ok = false
        }
        Expr::Outer(d, ..) if *d != 1 => ok = false,
        Expr::Param(..) | Expr::Default(_) | Expr::CaseTest(_) => ok = false,
        _ => {}
    });
    ok
}

/// An output of a FROM-less subquery no outer join nulls that may move up: `movable`, or
/// with sub-selects (is_simple_subquery takes them, nothing volatile in them:
/// contain_volatile_functions looks into sub-selects), each place reading it computing it
/// (pullup_replace_vars).
fn movable_fromless(e: &Expr) -> bool {
    if movable(e) {
        return true;
    }
    let mut ok = !crate::sql::explain::expr_volatile_deep(e);
    e.walk(&mut |x| match x {
        Expr::Subquery(s) => ok &= s.call.is_none(),
        Expr::Agg(..) | Expr::Window(..) | Expr::Grouping(_) | Expr::Srf(..) => ok = false,
        Expr::Outer(d, ..) if *d != 1 => ok = false,
        Expr::Param(..) | Expr::Default(_) | Expr::CaseTest(_) => ok = false,
        _ => {}
    });
    ok
}

/// The LATERAL values (`Outer(1, i)`) an expression reads.
fn outer_refs(e: &Expr) -> Vec<usize> {
    let mut out = Vec::new();
    e.walk(&mut |x| {
        if let Expr::Outer(1, i, _) = x {
            out.push(*i);
        }
    });
    out
}

/// `e` (over the old row) over the new row; a column that stays a column keeps its type.
pub(super) fn subst(e: &Expr, map: &[Expr]) -> Expr {
    e.clone().rewrite(&mut |x| match x {
        Expr::Col(i, t) => match map.get(i) {
            Some(Expr::Col(j, _)) => Expr::Col(*j, t),
            Some(m) => m.clone(),
            None => Expr::Col(i, t),
        },
        x => x,
    })
}

fn subst_outer(e: Expr, params: &[Expr]) -> Expr {
    e.rewrite(&mut |x| match x {
        Expr::Outer(1, i, t) => params.get(i).cloned().unwrap_or(Expr::Outer(1, i, t)),
        x => x,
    })
}

/// Whether the plan reads any value of an enclosing level (a LATERAL join's parameters
/// included).
fn reads_outer(p: &Plan) -> bool {
    // (A nested loop passing its inner side parameters: the inner side's `Outer(1, …)`
    // are those, values of its outer side; the level's own come in through them.)
    if let Plan::Join {
        left, right, params, cond, hash, ..
    } = p
        && !params.is_empty()
    {
        let mut deeper = false;
        let mut exprs = Vec::new();
        let mut stack: Vec<&Plan> = vec![right];
        while let Some(q) = stack.pop() {
            exprs.clear();
            crate::sql::explain::node_exprs(q, &mut exprs);
            if let Plan::Scan(sp) = q {
                exprs.extend(sp.written.iter());
                exprs.extend(sp.phv_quals.iter());
            }
            for e in &exprs {
                e.walk(&mut |x| deeper |= matches!(x, Expr::Outer(d, ..) if *d != 1));
            }
            stack.extend(crate::sql::explain::plan_inputs(q).into_iter().map(|a| &**a));
        }
        return deeper
            || params.iter().chain(cond.iter()).any(|e| e.has_outer_refs())
            || hash.as_ref().is_some_and(|(l, r)| l.iter().chain(r).any(|e| e.has_outer_refs()))
            || reads_outer(left);
    }
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(p, &mut exprs);
    if let Plan::Scan(sp) = p {
        exprs.extend(sp.written.iter());
        exprs.extend(sp.phv_quals.iter());
        exprs.extend(sp.current_of.iter());
    }
    if exprs.iter().any(|e| e.has_outer_refs()) {
        return true;
    }
    // (A sub-select without parameters of its own, a planner-made MIN / MAX InitPlan's,
    // reads the level's outer values as its own `Outer(1, …)`.)
    let mut framed = false;
    for e in &exprs {
        e.walk(&mut |x| {
            if let Expr::Subquery(s) = x
                && s.params.is_empty()
            {
                framed |= reads_outer(&s.plan);
            }
        });
    }
    framed || crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_outer(c))
}

/// A FROM subquery over one table scan: its select list and the scan.
fn subquery_leaf(p: &Plan) -> Option<(&ScanPlan, &Vec<Expr>)> {
    let Plan::Project { input, exprs } = p else {
        return None;
    };
    let Plan::Scan(sp) = &**input else {
        return None;
    };
    // (The subquery planned its scan for itself: the group plans it again.)
    if virtual_proj(p).is_some()
        || sp.sample.is_some()
        || sp.current_of.is_some()
        || sp.table.partkey.is_some()
        || sp.mark.is_some()
        || sp.limit.is_some()
        || sp.filter.as_ref().is_some_and(|f| !movable_filter(f))
    {
        return None;
    }
    Some((sp, exprs))
}

/// is_standard_join_alias_expression: a column (or a PlaceHolderVar's), under implicit
/// coercions and COALESCE.
fn standard_alias(e: &Expr) -> bool {
    match e {
        Expr::Col(..) => true,
        Expr::Cast(x, ..) | Expr::Relabel(x, _) => standard_alias(x),
        Expr::Coalesce(args, _) => args.iter().all(standard_alias),
        _ => false,
    }
}

/// A FROM subquery (or view) over a join, or a filter over its outputs (a WHERE clause
/// pushed down to it): its join tree is part of the group (pull_up_simple_subquery), its
/// outputs evaluated where they're read. One an outer join above nulls that wouldn't go
/// NULL with the subquery's row (a PlaceHolderVar to evaluate at a join, which the plan has
/// no place for here) keeps the subquery a leaf, unless reduce_outer_joins made those joins
/// inner.
fn flatten_join_subquery(p: &Plan, nulled: u32, anulled: u32, cx: &mut Cx) -> Option<(Plan, Vec<Expr>, Vec<(u32, u64)>)> {
    // pull_up_sublinks runs before pull_up_subqueries, and a pulled-up subquery's semi
    // joins join the query's join tree: a subquery whose join tree has them is pulled up
    // as its level had it before planning (which may have unique-ified them already).
    if let Some((b, semi)) = bound_form(p, &cx.bound, cx.unbind) {
        let saved = std::mem::replace(&mut cx.semi, semi);
        let r = flatten_tree(&b, nulled, anulled, cx, semi);
        cx.semi = saved;
        if r.is_some() {
            return r;
        }
    }
    flatten_tree(p, nulled, anulled, cx, false)
}

/// `p` with the restrictions as written (`ScanPlan::written`) of its semi / anti joins'
/// right sides that read the query: the correlated quals pull_up_sublinks made the joins'
/// clauses, which the scan keeps in its written order but no longer checks.
fn semi_written_dropped(p: &Plan) -> Plan {
    fn strip(p: &Plan) -> Plan {
        match p {
            Plan::Scan(sp) => {
                let mut sp = sp.clone();
                sp.written.retain(|w| !w.has_outer_refs());
                Plan::Scan(sp)
            }
            Plan::Join { .. } | Plan::Filter { .. } | Plan::Project { .. } => {
                let mut q = p.clone();
                for c in crate::sql::explain::plan_inputs_mut(&mut q) {
                    *c = Arc::new(strip(c));
                }
                q
            }
            p => p.clone(),
        }
    }
    let mut q = p.clone();
    if let Plan::Join { kind, right, .. } = &mut q
        && matches!(kind, JoinKind::Semi | JoinKind::Anti)
    {
        *right = Arc::new(strip(right));
    }
    for c in crate::sql::explain::plan_inputs_mut(&mut q) {
        if matches!(&**c, Plan::Join { .. } | Plan::Filter { .. }) {
            *c = Arc::new(semi_written_dropped(c));
        }
    }
    q
}

/// A FROM subquery (`p`, perhaps under a clause pushed down onto it) with the join tree its
/// level had before planning in place of the planned one (`ExecCtx::bound_trees`); whether
/// that tree has semi / anti joins. Trees without them only when `unbind`.
fn bound_form(p: &Plan, bound: &[crate::sql::paths::BoundTree], unbind: bool) -> Option<(Plan, bool)> {
    fn tree_of(p: &Plan, bound: &[crate::sql::paths::BoundTree], unbind: bool) -> Option<(Arc<Plan>, bool)> {
        let mut glue: Vec<&Vec<Expr>> = Vec::new();
        let mut join = p;
        while let Plan::Project { input, exprs } = join {
            glue.push(exprs);
            join = input;
        }
        let Plan::Join { meta, .. } = join else {
            return None;
        };
        let b = bound.iter().find(|b| meta.bound != 0 && b.id == meta.bound && (b.semi || unbind))?;
        let glue_ok = glue.len() == b.glue.len()
            && glue
                .iter()
                .zip(&b.glue)
                .all(|(a, b)| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_as(y)));
        if !glue_ok || crate::sql::paths::join_tree_key(p).as_ref() != Some(&b.key) {
            return None;
        }
        if meta.bound_pushed.is_empty() {
            return Some((b.tree.clone(), b.semi));
        }
        // (The clauses pushed into the planned join since: over the tree's row, each column
        // of the join's one the projections pass through.)
        let mut row: Vec<Expr> = (0..crate::sql::binder::plan_width(join)).map(|i| Expr::Col(i, DataType::Unknown)).collect();
        for g in glue.iter().rev() {
            row = g.iter().map(|e| subst(e, &row)).collect();
        }
        let mut preds = Vec::new();
        for q in &meta.bound_pushed {
            let mut ok = true;
            let q = q
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(c, t) => match row.iter().position(|e| matches!(e, Expr::Col(d, _) if *d == c)) {
                            Some(i) => Expr::Col(i, t),
                            None => {
                                ok = false;
                                Expr::Col(c, t)
                            }
                        },
                        x => x,
                    })
                })
                .unwrap();
            if !ok {
                return None;
            }
            preds.push(q);
        }
        let tree = Plan::Filter {
            input: b.tree.clone(),
            pred: and_all(preds)?,
        };
        Some((Arc::new(tree), b.semi))
    }
    fn go(p: &Plan, bound: &[crate::sql::paths::BoundTree], unbind: bool) -> Option<(Plan, bool)> {
        let Plan::Project { input, exprs } = p else {
            return None;
        };
        let (input, semi) = match tree_of(input, bound, unbind) {
            Some(tree) => tree,
            None => {
                let (q, semi) = go(input, bound, unbind)?;
                (Arc::new(q), semi)
            }
        };
        Some((
            Plan::Project {
                input,
                exprs: exprs.clone(),
            },
            semi,
        ))
    }
    if bound.is_empty() {
        return None;
    }
    match p {
        Plan::Filter { input, pred } => {
            let (q, semi) = go(input, bound, unbind)?;
            Some((
                Plan::Filter {
                    input: Arc::new(q),
                    pred: pred.clone(),
                },
                semi,
            ))
        }
        p => go(p, bound, unbind),
    }
}

/// `flatten_join_subquery` of `p` as it is; `semi`: a join tree as bound, whose semi / anti
/// joins may be the group's.
fn flatten_tree(p: &Plan, nulled: u32, anulled: u32, cx: &mut Cx, semi: bool) -> Option<(Plan, Vec<Expr>, Vec<(u32, u64)>)> {
    let (proj, above) = match p {
        Plan::Filter { input, pred } if matches!(&**input, Plan::Project { .. }) && clauses_movable(pred) => {
            (&**input, Some(pred))
        }
        p => (p, None),
    };
    let Plan::Project { input, exprs } = proj else {
        return None;
    };
    // (Projections over another one, over the join: composed. A planned subquery's
    // join can sit under several: its columns put back in order, a removed join's
    // columns as constants.)
    let mut chain: Vec<&Vec<Expr>> = vec![exprs];
    let mut below = input;
    while let Plan::Project { input: i2, exprs: e2 } = &**below
        && crate::sql::plan::phv_projection(below).is_none()
        && virtual_proj(below).is_none()
        && e2.iter().all(|e| matches!(e, Expr::Col(..)) || movable(e))
    {
        chain.push(e2);
        below = i2;
    }
    let composed: Vec<Expr>;
    let (input, exprs) = if chain.len() > 1 && join_subquery(below, semi) {
        let mut acc = chain.pop().unwrap().clone();
        while let Some(upper) = chain.pop() {
            acc = upper.iter().map(|e| subst(e, &acc)).collect();
        }
        composed = acc;
        (below, &composed)
    } else {
        (input, exprs)
    };
    // An output an outer join above nulls (as written: pull-up comes before
    // reduce_outer_joins) that wouldn't go NULL with the subquery's rows: a
    // PlaceHolderVar, evaluated at the lowest join of the relations it reads (or of all
    // the subquery's, reading none).
    let _ = anulled;
    // (The last `alias_cols` outputs are JOIN USING merged columns (the binder's
    // `materialize_using`): add_nullingrels_if_needed wraps one that isn't a standard join
    // alias expression, columns under implicit coercions and COALESCE.)
    let alias_from = {
        let j = match &**input {
            Plan::Filter { input, .. } => &**input,
            j => j,
        };
        match j {
            Plan::Join { meta, .. } => exprs.len().saturating_sub(meta.alias_cols),
            _ => exprs.len(),
        }
    };
    // (Inside a LATERAL subquery pulled up (`Cx::frame`), its lateral references are the
    // query's Vars: pullup_replace_vars_callback wraps a bare one whose relation isn't under
    // the same lowest nulling outer join, and an expression unless it reads a Var of the
    // subquery's (or of such a relation) and is strict.)
    let frame = cx.frame.clone();
    let same_level = |i: usize| frame.as_ref().is_some_and(|f| f.get(i).is_some_and(|n| nulled & !n.0 == 0));
    let needs_phv_at = |i: usize, e: &Expr| {
        if i >= alias_from {
            return nulled != 0 && !standard_alias(e);
        }
        if frame.is_some() && !outer_refs(e).is_empty() && !matches!(e, Expr::Col(..)) {
            let nullable = !e.columns().is_empty() || outer_refs(e).into_iter().any(same_level);
            return nulled != 0 && (!nullable || nonstrict(e));
        }
        !matches!(e, Expr::Col(..)) && nulled != 0 && (e.columns().is_empty() || nonstrict(e))
    };
    let beyond = |q: &Plan| {
        let mut r = (false, false);
        frame_refs(q, 1, &mut r);
        r.1
    };
    if !join_subquery(input, semi)
        || virtual_proj(proj).is_some()
        || if frame.is_some() {
            beyond(input)
        } else if semi {
            reads_outer(&semi_written_dropped(input))
        } else {
            reads_outer(input)
        }
        || !exprs.iter().enumerate().all(|(i, e)| {
            matches!(e, Expr::Col(..))
                || (movable(e) || (nulled == 0 || frame.is_none() && needs_phv_at(i, e)) && movable_fromless(e))
                    && (outer_refs(e).is_empty() || frame.is_some() && plain_frame_refs(e))
        })
        || frame.is_none() && exprs.iter().enumerate().any(|(i, e)| needs_phv_at(i, e) && e.has_outer_refs())
    {
        return None;
    }
    // (Its sub-select outputs, no outer join nulling them, computed where read: a copy each.)
    note_moved(exprs, &vec![true; exprs.len()], cx);
    let (join, pred) = match &**input {
        Plan::Filter { input, pred } => (&**input, Some(pred)),
        j => (j, None),
    };
    // (A LATERAL subquery's scans' clauses over its lateral references: its WHERE's.)
    let (hoisted_join, hoisted) = if frame.is_some() { hoist_frame_quals(join) } else { (join.clone(), Vec::new()) };
    if frame.is_some() && frame_quals_left(&hoisted_join) {
        return None;
    }
    let join = &hoisted_join;
    // (Its own subqueries are pulled up into it first, as pull_up_simple_subquery does:
    // PlaceHolderVars for its own outer joins; its outputs are wrapped above for the
    // outer joins over it.)
    let (j2, mj, mut nj) = lift(join, 0, 0, None, cx);
    for x in nj.iter_mut() {
        x.0 |= nulled;
    }
    let j2 = unplanned(j2);
    cx.lifted = true;
    // The outputs over the new row as computed where read (`direct`), and as the query
    // reads them (PlaceHolderVars as columns past the join's).
    let direct: Vec<Expr> = exprs.iter().map(|e| subst(e, &mj)).collect();
    let w2 = crate::sql::binder::plan_width(&j2);
    let mut phvs: Vec<Expr> = Vec::new();
    let map: Vec<Expr> = exprs
        .iter()
        .zip(&direct)
        .enumerate()
        .map(|(i, (e, d))| {
            // (Judged on the output as its own pulled-up subqueries left it: a column
            // of one that became its constant needs one too.)
            if needs_phv_at(i, d) {
                phvs.push(d.clone());
                Expr::Col(w2 + phvs.len() - 1, e.ty())
            } else {
                d.clone()
            }
        })
        .collect();
    let mut preds = Vec::new();
    if let Some(f) = pred {
        conjuncts(subst(f, &mj), &mut preds);
    }
    for q in &hoisted {
        preds.push(subst(q, &mj));
    }
    // (A clause pushed down onto the subquery from above reads its outputs as the query
    // does: over the PlaceHolderVars, above the projection computing them.)
    let mut above_preds = Vec::new();
    if let Some(f) = above {
        if phvs.is_empty() {
            conjuncts(subst(f, &map), &mut preds);
        } else {
            conjuncts(subst(f, &map), &mut above_preds);
        }
    }
    let plan = match and_all(preds) {
        Some(f) => Plan::Filter {
            input: Arc::new(j2),
            pred: f,
        },
        None => j2,
    };
    if phvs.is_empty() {
        return Some((plan, map, nj));
    }
    let types = super::super::analyzejoins::col_types(&plan);
    let mut out: Vec<Expr> = (0..w2)
        .map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown)))
        .collect();
    let rels = nj.iter().fold(0u64, |m, x| m | x.1);
    nj.extend(std::iter::repeat_n((nulled, rels), phvs.len()));
    out.extend(phvs);
    let plan = Plan::Project {
        input: Arc::new(crate::sql::plan::mark_phv_join(plan)),
        exprs: out,
    };
    let plan = match and_all(above_preds) {
        Some(f) => Plan::Filter {
            input: Arc::new(plan),
            pred: f,
        },
        None => plan,
    };
    Some((plan, map, nj))
}

/// A join tree's scans as the group will plan them again (the subquery planned them for
/// itself).
pub(super) fn unplanned(p: Plan) -> Plan {
    match p {
        Plan::Scan(mut sp) if sp.mark.is_none() && sp.current_of.is_none() => {
            sp.access = Access::Unplanned;
            sp.index = None;
            sp.reverse = false;
            Plan::Scan(sp)
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
        } => Plan::Join {
            kind,
            left: Arc::new(unplanned(Arc::unwrap_or_clone(left))),
            right: Arc::new(unplanned(Arc::unwrap_or_clone(right))),
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        },
        Plan::Filter { input, pred } => Plan::Filter {
            input: Arc::new(unplanned(Arc::unwrap_or_clone(input))),
            pred,
        },
        p => p,
    }
}

/// Clauses that may become the group's: each reads some column, none volatile or a
/// sub-select.
fn clauses_movable(f: &Expr) -> bool {
    let mut parts = Vec::new();
    conjuncts(f.clone(), &mut parts);
    super::outerjoins::absorbable(f) && parts.iter().all(|q| !q.columns().is_empty() && movable(q))
}

/// A subquery's plan below its select list that is a join tree the group can take in: a
/// plain join (or its WHERE over one, each clause reading the subquery's columns).
fn join_subquery(p: &Plan, semi: bool) -> bool {
    let (j, pred) = match p {
        Plan::Filter { input, pred } => (&**input, Some(pred)),
        j => (j, None),
    };
    let Plan::Join { kind, meta, .. } = j else {
        return false;
    };
    // (A semi / anti join: in a join tree as bound, `bound_form`.)
    if !(matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full)
        || semi && matches!(kind, JoinKind::Semi | JoinKind::Anti))
        || meta.from_call.is_some()
        || meta.phv_eval
    {
        return false;
    }
    pred.is_none_or(clauses_movable)
}

/// A scan filter that doesn't stop the scan from being a plain relation of the join tree.
fn movable_filter(f: &Expr) -> bool {
    let mut ok = true;
    f.walk(&mut |x| {
        if let Expr::Outer(d, ..) = x
            && *d != 1
        {
            ok = false
        }
    });
    ok
}

struct Cx {
    /// Outer joins seen so far (each a bit).
    next: u32,
    lifted: bool,
    /// Something lifted can't be placed: nothing is lifted.
    bad: bool,
    /// Relations (FROM items) seen so far (each a bit).
    nrel: u32,
    /// An empty relation was pulled up or removed.
    results: bool,
    /// The plan `lift` just returned is a join merged into a projection over one side
    /// (remove_useless_result_rtes): its PlaceHolderVars are a lower join's, nulled at the
    /// join above.
    merged: bool,
    /// The sub-selects of the outputs moved up (`copy_moved_subselects`).
    moved: Vec<usize>,
    /// FROM subqueries' join trees with semi / anti joins as bound (`ExecCtx::bound_trees`):
    /// per planned tree, the tree to pull up instead.
    bound: Vec<crate::sql::paths::BoundTree>,
    /// Pulling up such a tree: its semi / anti joins are the group's.
    semi: bool,
    /// Lifting a LATERAL join's left side: its FROM subqueries are pulled up as their levels
    /// had them before planning (`ExecCtx::bound_trees`), the LATERAL item reading their
    /// outputs then part of their join problem, as pull_up_subqueries has it.
    unbind: bool,
    /// Lifting inside a LATERAL subquery pulled up (`JoinMeta::lateral_sub`): per value of
    /// its join (its `Outer(1, k)`), the outer joins nulling the relation it reads and that
    /// relation, as `lift`'s `lateral` has them. Its values are lateral references then,
    /// Vars of the query above (pullup_replace_vars_callback's LATERAL cases).
    frame: Option<Vec<(u32, u64)>>,
}

/// The `Outer` levels plan `p` reads, `depth` frames in from the one asked about (1: the
/// LATERAL join whose right side `p` is): (reads that frame, reads one past it). A LATERAL
/// join's right side and a sub-select with parameters are a frame further in.
fn frame_refs(p: &Plan, depth: usize, out: &mut (bool, bool)) {
    let mut es = Vec::new();
    crate::sql::explain::node_exprs(p, &mut es);
    if let Plan::Scan(sp) = p {
        es.extend(sp.written.iter());
        es.extend(sp.phv_quals.iter());
        es.extend(sp.current_of.iter());
        // (Planned conditions came from the filter or a join: as reading anything.)
        if !matches!(sp.access, Access::Unplanned | Access::Seq) || sp.index.is_some() {
            out.1 = true;
        }
    }
    for e in es {
        expr_frame_refs(e, depth, out);
    }
    match p {
        Plan::Join { left, right, params, .. } if !params.is_empty() => {
            frame_refs(left, depth, out);
            frame_refs(right, depth + 1, out);
        }
        Plan::With { .. } => out.1 = true,
        _ => {
            for c in crate::sql::explain::plan_inputs(p) {
                frame_refs(c, depth, out);
            }
        }
    }
}

fn expr_frame_refs(e: &Expr, depth: usize, out: &mut (bool, bool)) {
    e.walk(&mut |x| match x {
        Expr::Outer(l, ..) if *l == depth => out.0 = true,
        Expr::Outer(l, ..) if *l > depth => out.1 = true,
        Expr::Subquery(s) => frame_refs(&s.plan, depth + usize::from(!s.params.is_empty()), out),
        _ => {}
    });
}

/// Whether an expression reads the frame `Outer(1, …)` stands for only as plain lateral
/// references (`Outer(1, k)` nodes of its own, none inside a sub-select's plan or its
/// parameters), and nothing further out.
fn plain_frame_refs(e: &Expr) -> bool {
    let mut ok = true;
    e.walk(&mut |x| match x {
        Expr::Outer(l, ..) if *l > 1 => ok = false,
        Expr::Subquery(s) => {
            let mut r = (false, false);
            expr_frame_refs(&Expr::Subquery(s.clone()), 1, &mut r);
            ok &= !r.0 && !r.1;
        }
        _ => {}
    });
    ok
}

/// A scan's clause inside a LATERAL subquery pulled up that the join tree checks: one
/// reading the subquery's lateral references plainly, nothing volatile, no sub-select.
fn frame_clause(e: &Expr) -> bool {
    let mut reads = false;
    let mut fine = !e.is_volatile();
    e.walk(&mut |x| match x {
        Expr::Outer(1, ..) => reads = true,
        Expr::Outer(..) | Expr::Subquery(_) | Expr::Param(..) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) => fine = false,
        _ => {}
    });
    reads && fine
}

/// A LATERAL subquery's inner join tree with its scans' clauses over the lateral
/// references taken out, over the tree's row: they are WHERE clauses of the query the
/// subquery is pulled up into (distribute_qual_to_rels over the Vars they read), join
/// clauses of the group.
fn hoist_frame_quals(p: &Plan) -> (Plan, Vec<Expr>) {
    match p {
        Plan::Join { kind: JoinKind::Inner, left, right, cond, hash, params, nleft, nright, meta } if params.is_empty() => {
            let (l, lq) = hoist_frame_quals(left);
            let (r, rq) = hoist_frame_quals(right);
            let mut q = lq;
            q.extend(rq.into_iter().map(|e| e.shift_cols(*nleft as isize)));
            (
                Plan::Join {
                    kind: JoinKind::Inner,
                    left: Arc::new(l),
                    right: Arc::new(r),
                    cond: cond.clone(),
                    hash: hash.clone(),
                    params: Vec::new(),
                    nleft: *nleft,
                    nright: *nright,
                    meta: meta.clone(),
                },
                q,
            )
        }
        // (A LATERAL join of its own: its left side's scans are the subquery's relations as
        // any; its right side reads the join's values, a frame further in.)
        Plan::Join { kind: JoinKind::Inner, left, right, cond, hash, params, nleft, nright, meta } => {
            let (l, q) = hoist_frame_quals(left);
            (
                Plan::Join {
                    kind: JoinKind::Inner,
                    left: Arc::new(l),
                    right: right.clone(),
                    cond: cond.clone(),
                    hash: hash.clone(),
                    params: params.clone(),
                    nleft: *nleft,
                    nright: *nright,
                    meta: meta.clone(),
                },
                q,
            )
        }
        Plan::Filter { input, pred } if matches!(&**input, Plan::Join { .. }) => {
            let (i, mut q) = hoist_frame_quals(input);
            // (Over a LATERAL join of its own, which takes no clause pushed into its sides:
            // the WHERE's clauses over the references are the query's as any.)
            let mut keep = vec![pred.clone()];
            if matches!(&**input, Plan::Join { params, .. } if !params.is_empty()) {
                let mut parts = Vec::new();
                conjuncts(pred.clone(), &mut parts);
                let (out, rest): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(frame_clause);
                q.extend(out);
                keep = rest;
            }
            match and_all(keep) {
                Some(pred) => (
                    Plan::Filter {
                        input: Arc::new(i),
                        pred,
                    },
                    q,
                ),
                None => (i, q),
            }
        }
        Plan::Scan(sp)
            if sp.sample.is_none()
                && sp.current_of.is_none()
                && sp.mark.is_none()
                && sp.limit.is_none()
                && sp.filter.as_ref().is_some_and(|f| {
                    let mut parts = Vec::new();
                    conjuncts(f.clone(), &mut parts);
                    parts.iter().any(frame_clause)
                }) =>
        {
            let mut sp = sp.clone();
            let mut parts = Vec::new();
            conjuncts(sp.filter.take().unwrap(), &mut parts);
            let (out, keep): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(frame_clause);
            sp.filter = and_all(keep);
            sp.written.retain(|w| !out.iter().any(|o| o.same_as(w)));
            sp.phv_quals.retain(|w| !out.iter().any(|o| o.same_as(w)));
            sp.access = Access::Unplanned;
            sp.index = None;
            sp.reverse = false;
            (Plan::Scan(sp), out)
        }
        // (A FROM subquery of its own, pulled up into it: its scans' clauses over the
        // projection's row, which must return the columns they read.)
        Plan::Project { input, exprs } => {
            let (i, q) = hoist_frame_quals(input);
            if q.is_empty() {
                return (p.clone(), Vec::new());
            }
            let pos = |c: usize| exprs.iter().position(|e| matches!(e, Expr::Col(j, _) if *j == c));
            if !q.iter().all(|e| e.columns().iter().all(|&c| pos(c).is_some())) {
                return (p.clone(), Vec::new());
            }
            let out = q
                .into_iter()
                .map(|e| {
                    e.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) => Expr::Col(pos(c).unwrap_or(c), t),
                            x => x,
                        })
                    })
                    .expect("a column map")
                })
                .collect();
            (Plan::Project { input: Arc::new(i), exprs: exprs.clone() }, out)
        }
        p => (p.clone(), Vec::new()),
    }
}

/// A scan of the tree still checking a clause over the LATERAL references (one
/// `hoist_frame_quals` couldn't take out): the tree can't be pulled up without them.
fn frame_quals_left(p: &Plan) -> bool {
    match p {
        Plan::Scan(sp) => sp.filter.as_ref().is_some_and(|f| {
            let mut parts = Vec::new();
            conjuncts(f.clone(), &mut parts);
            parts.iter().any(|e| {
                let mut r = (false, false);
                expr_frame_refs(e, 1, &mut r);
                r.0
            })
        }),
        Plan::Join { params, left, .. } if !params.is_empty() => frame_quals_left(left),
        p => crate::sql::explain::plan_inputs(p).into_iter().any(|c| frame_quals_left(c)),
    }
}

/// is_simple_subquery for a LATERAL subquery whose FROM is a join (the binder's plan of
/// it, `Outer(1, k)` its lateral references): a select list (no set-returning, aggregate,
/// window or volatile call: a grouping, DISTINCT, ORDER BY, LIMIT or locking clause is a
/// node of its own over the select list) and maybe its WHERE over a plain join tree, the
/// references reading only the query right above, none in or under an outer join of its
/// own (jointree_contains_lateral_outer_refs), none inside a sub-select.
pub(crate) fn lateral_pullable(p: &Plan, collapses: bool) -> bool {
    // (`SELECT *` of the join: no projection of its own, `mark_deferred_lateral` gives it
    // one.)
    let mut x = p;
    while let Plan::Project { input, exprs } = x {
        if virtual_proj(x).is_some() || !exprs.iter().all(|e| movable_fromless(e) && plain_frame_refs(e)) {
            return false;
        }
        x = input;
    }
    let mut r = (false, false);
    frame_refs(p, 1, &mut r);
    if !r.0 || r.1 {
        return false;
    }
    // The join tree, its own subqueries pulled up into it: its quals may read the
    // references only outside outer joins (`restricted` in or under one); a subquery's
    // select list may (its outputs, PlaceHolderVars there).
    fn qual(e: &Expr, restricted: bool) -> bool {
        let mut r = (false, false);
        expr_frame_refs(e, 1, &mut r);
        plain_frame_refs(e) && !(restricted && r.0)
    }
    fn tree(p: &Plan, top: bool, restricted: bool, collapses: bool) -> bool {
        match p {
            Plan::Filter { input, pred } if !top || matches!(&**input, Plan::Join { .. } | Plan::Filter { .. }) => {
                qual(pred, restricted) && tree(input, false, restricted, collapses)
            }
            Plan::Join { kind, left, right, cond, hash, params, meta, .. } => {
                if !matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full)
                    || meta.from_call.is_some()
                    || meta.phv_eval
                {
                    return false;
                }
                // (A LATERAL item of its own: taken when the subquery's FROM items join the
                // list it is pulled up into (`collapses`, deconstruct_recurse's
                // from_collapse_limit), its relations then items of the group above with the
                // LATERAL item among them (join 783). Else Postgres plans them as a sub-list,
                // a join relation of their own first, as the engine plans the subquery that
                // isn't pulled up (join 355 under from_collapse_limit 2).)
                if !params.is_empty() && !collapses {
                    return false;
                }
                let restricted = restricted || meta.written_kind.unwrap_or(*kind) != JoinKind::Inner;
                cond.as_ref().is_none_or(|c| qual(c, restricted))
                    && hash.as_ref().is_none_or(|(l, r)| l.iter().chain(r).all(|e| qual(e, restricted)))
                    && tree(left, false, restricted, collapses)
                    && tree(right, false, restricted, collapses)
            }
            _ if top => false,
            // (A subquery of its own, pulled up into it first: its select list may read
            // them, its quals are the tree's.)
            Plan::Project { input, exprs } => exprs.iter().all(plain_frame_refs) && tree(input, false, restricted, collapses),
            // (A FROM item: a scan's filter reading the references is a qual of the join
            // tree; anything else reading them a LATERAL item of the group.)
            Plan::Scan(sp) => sp.filter.as_ref().is_none_or(|f| qual(f, restricted)),
            p => {
                let mut r = (false, false);
                frame_refs(p, 1, &mut r);
                !r.1 && !(restricted && r.0)
            }
        }
    }
    tree(x, true, false, collapses)
}

/// `nulled`: the outer joins above `p` (in the group) that null its rows, as written;
/// `anulled`: those that still do after reduce_outer_joins. `lateral`: the
/// LATERAL values of the join `p` is the right side of (over that join's left row as it
/// becomes), with the outer joins nulling each one's relation; None when `p` isn't such a
/// side. Returns the new plan, the map, and per new column the outer joins nulling it and
/// the relation it comes from (a bit).
fn lift(
    p: &Plan,
    nulled: u32,
    anulled: u32,
    lateral: Option<(&[Expr], &[(u32, u64)])>,
    cx: &mut Cx,
) -> (Plan, Vec<Expr>, Vec<(u32, u64)>) {
    let rel = 1u64 << cx.nrel.min(63);
    let identity = |p: &Plan, cx: &mut Cx| {
        cx.nrel += 1;
        let types = super::super::analyzejoins::col_types(p);
        let w = types.len();
        (
            p.clone(),
            types.into_iter().enumerate().map(|(i, t)| Expr::Col(i, t)).collect::<Vec<_>>(),
            vec![(nulled, rel); w],
        )
    };
    match p {
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
        } if matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full)
            && meta.from_call.is_none()
            && cx.next < 30 =>
        {
            let bit = 1u32 << cx.next;
            cx.next += 1;
            // (As written: Postgres pulls subqueries up before reducing outer joins.)
            let wk = meta.written_kind.unwrap_or(*kind);
            let lnull = nulled | if matches!(wk, JoinKind::Right | JoinKind::Full) { bit } else { 0 };
            let rnull = nulled | if matches!(wk, JoinKind::Left | JoinKind::Full) { bit } else { 0 };
            let alnull = anulled | if matches!(kind, JoinKind::Right | JoinKind::Full) { bit } else { 0 };
            let arnull = anulled | if matches!(kind, JoinKind::Left | JoinKind::Full) { bit } else { 0 };
            let unbind = cx.unbind;
            cx.unbind |= !params.is_empty();
            let (l2, ml, nl) = lift(left, lnull, alnull, None, cx);
            cx.unbind = unbind;
            let lmerged = std::mem::take(&mut cx.merged);
            let nleft2 = crate::sql::binder::plan_width(&l2);
            let params2: Vec<Expr> = params.iter().map(|e| subst(e, &ml)).collect();
            let pnull: Vec<(u32, u64)> = params2
                .iter()
                .map(|e| {
                    e.columns().iter().fold((0u32, 0u64), |m, &c| {
                        let (n, r) = nl.get(c).copied().unwrap_or((0, 0));
                        (m.0 | n, m.1 | r)
                    })
                })
                .collect();
            // (A side the hash keys read: its lifted values must stay over that side.)
            let lat = (!params.is_empty()).then_some((&params2[..], &pnull[..]));
            // A LATERAL subquery over a join the binder left unplanned (`JoinMeta::
            // lateral_held`): pulled up as any (pull_up_simple_subquery), its lateral
            // references this join's values. Not one any more (the planner simplified it), or
            // not pullable after all: left as it is, to be planned apart
            // (`Planner::settle_laterals`).
            let held = meta.lateral_held && !params.is_empty();
            let mut flat = false;
            // (One whose lateral references all went (outputs nothing reads, nulled by
            // drop_unused_outputs) is a subquery as any other: planned apart and pulled up as
            // the binder's plan of one, `settle_laterals`.)
            let still_lateral = {
                let mut r = (false, false);
                frame_refs(right, 1, &mut r);
                r.0
            };
            let (r2, mr, nr) = if held {
                let saved = cx.frame.replace(pnull.clone());
                let r = if crate::sql::plan::deferred_lateral(right) && still_lateral {
                    flatten_join_subquery(right, rnull, arnull, cx)
                } else {
                    None
                };
                cx.frame = saved;
                match r {
                    Some(r) => {
                        flat = true;
                        r
                    }
                    None => {
                        let bit = 1u64 << cx.nrel.min(63);
                        cx.nrel += 1;
                        let types = super::super::analyzejoins::col_types(right);
                        let w = types.len();
                        (
                            (**right).clone(),
                            types.into_iter().enumerate().map(|(i, t)| Expr::Col(i, t)).collect(),
                            vec![(rnull, bit); w],
                        )
                    }
                }
            } else {
                lift(right, rnull, arnull, lat, cx)
            };
            let kept_held = held && !flat;
            let rmerged = std::mem::take(&mut cx.merged) || kept_held;
            let nright2 = crate::sql::binder::plan_width(&r2);
            // The right side's columns over the combined row (its LATERAL values put in).
            let mr: Vec<Expr> = mr
                .into_iter()
                .map(|e| subst_outer(e.shift_cols(nleft2 as isize), &params2))
                .collect();
            let mut map = ml.clone();
            map.extend(mr.iter().cloned());
            // (A subquery right below this join: its outputs in this join's own clause are
            // read before any join nulls them, no PlaceHolderVars there, as
            // pullup_replace_vars has it for Vars without varnullingrels.)
            let direct = |e: Expr, side: &Plan, off: usize| -> Expr {
                if std::ptr::eq(side, &l2) && lmerged || std::ptr::eq(side, &r2) && rmerged {
                    return e;
                }
                // (A subquery over a join: its PlaceHolderVars' expressions over its row.)
                let side = match side {
                    Plan::Filter { input, .. } if crate::sql::plan::phv_projection(input).is_some() => &**input,
                    s => s,
                };
                let inline: Vec<Option<Expr>> = match crate::sql::plan::phv_projection(side) {
                    Some((_, pe, n)) => (0..n).map(|_| None).chain(pe.iter().cloned().map(Some)).collect(),
                    // (A subquery over one table: an output it computes, when it reads only
                    // columns the subquery returns.)
                    // (A FROM-less relation's values.)
                    None if one_row(side).is_some() => one_row(side).unwrap().into_iter().map(Some).collect(),
                    None => match side {
                        Plan::Project { input, exprs } if matches!(&**input, Plan::Scan(_)) => exprs
                            .iter()
                            .map(|x| {
                                if matches!(x, Expr::Col(..)) {
                                    return None;
                                }
                                let mut ok = true;
                                let y = x
                                    .clone()
                                    .transform(&mut |z| {
                                        Ok(match z {
                                            Expr::Col(c, t) => match exprs.iter().position(|w| matches!(w, Expr::Col(d, _) if *d == c)) {
                                                Some(k) => Expr::Col(k, t),
                                                None => {
                                                    ok = false;
                                                    Expr::Col(c, t)
                                                }
                                            },
                                            z => z,
                                        })
                                    })
                                    .unwrap();
                                ok.then_some(y)
                            })
                            .collect(),
                        _ => Vec::new(),
                    },
                };
                if inline.iter().all(Option::is_none) {
                    return e;
                }
                // (A LATERAL subquery pulled up: its expressions' lateral references are
                // this join's values.)
                let lat_side = flat && std::ptr::eq(side, &r2);
                let e = e
                    .transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(j, t) if j >= off => match inline.get(j - off) {
                                Some(Some(y)) if lat_side => subst_outer(y.clone().shift_cols(off as isize), &params2),
                                Some(Some(y)) => y.clone().shift_cols(off as isize),
                                _ => Expr::Col(j, t),
                            },
                            x => x,
                        })
                    })
                    .unwrap();
                // (`c = c` of a constant: true, as eval_const_expressions folds it.)
                let mut parts = Vec::new();
                conjuncts(e, &mut parts);
                parts.retain(|q| {
                    !matches!(q, Expr::Call(d, a, _) if d.name == "=" && a.len() == 2
                        && matches!(&a[0], Expr::Const(v, _) if !matches!(v, Value::Null))
                        && a[0].same_as(&a[1]))
                });
                and_all(parts).unwrap_or(Expr::Const(Value::Bool(true), DataType::Bool))
            };
            // (Deeper down too: a pulled-up subquery's constant output read in this join's own
            // clause through joins that don't null it (as written) has no varnullingrels
            // there either, so pullup_replace_vars_callback puts the constant itself, no
            // PlaceHolderVar: `t1.f1 = '***'::text` over `b1 left join (...)`.)
            let deep = |e: Expr, side: &Plan, off: usize, merged: bool| -> Expr {
                if merged {
                    return e;
                }
                let w = crate::sql::binder::plan_width(side);
                e.transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(j, t) if j >= off && j < off + w => match unnulled_const(side, j - off) {
                            Some(k) => k,
                            None => Expr::Col(j, t),
                        },
                        x => x,
                    })
                })
                .unwrap()
            };
            // (When that puts a constant in, the group is planned over the clause with it. A
            // LATERAL value stays a PlaceHolderVar: pullup_replace_vars_callback wraps a
            // lateral reference to outside the subquery.)
            let put_in = |c: &Expr, d: &Expr| !d.same_as(c) && outer_refs(d).len() == outer_refs(c).len();
            let cond2 = cond.as_ref().map(|c| {
                let c = subst(c, &map);
                let d = direct(direct(c.clone(), &l2, 0), &r2, nleft2);
                let d = deep(deep(d, &l2, 0, lmerged), &r2, nleft2, rmerged);
                if put_in(&c, &d) {
                    cx.lifted = true;
                }
                d
            });
            let hash2 = hash.as_ref().map(|(lk, rk)| {
                let mut deepened = false;
                let mut keep = |e: Expr, side: &Plan, off: usize, merged: bool| {
                    let d = deep(direct(e.clone(), side, off), side, off, merged);
                    deepened |= put_in(&e, &d);
                    d
                };
                let rk: Vec<Expr> = rk.iter().map(|e| keep(subst(e, &mr), &r2, nleft2, rmerged)).collect();
                let lk: Vec<Expr> = lk.iter().map(|e| keep(subst(e, &ml), &l2, 0, lmerged)).collect();
                if deepened {
                    cx.lifted = true;
                }
                // (A right key now reading the left side: not a hash key any more.)
                if rk.iter().any(|e| e.columns().iter().any(|&c| c < nleft2)) {
                    cx.bad = true;
                }
                (
                    lk,
                    rk.into_iter()
                        .map(|e| if cx.bad { e } else { e.shift_cols(-(nleft2 as isize)) })
                        .collect::<Vec<_>>(),
                )
            });
            // A LATERAL subquery's WHERE clauses reading the left side are the join's
            // (deconstruct_jointree: they come before its ON clause).
            let (r2, moved) = if !params.is_empty() && !kept_held && matches!(kind, JoinKind::Inner | JoinKind::Left) {
                lateral_quals(r2, &params2, &pnull, nleft2)
            } else {
                (r2, Vec::new())
            };
            if !moved.is_empty() {
                cx.lifted = true;
            }
            let cond2 = match (moved.is_empty(), cond2) {
                (true, c) => c,
                (false, c) => {
                    let mut all = moved;
                    if let Some(c) = c {
                        conjuncts(c, &mut all);
                    }
                    and_all(all)
                }
            };
            // (A PlaceHolderVar of LATERAL values only the scan's clauses read: a
            // parameter of its own, the value the outer side computes.)
            let (r2, params2) = if !params.is_empty() && !kept_held && matches!(kind, JoinKind::Inner | JoinKind::Left) {
                hoist_phv_params(r2, params2)
            } else {
                (r2, params2)
            };
            let (r2, params2) = if kept_held {
                (r2, params2)
            } else if !params.is_empty()
                && !reads_outer(&r2)
                && !(matches!(r2, Plan::Barrier { .. }) && matches!(&**right, Plan::Barrier { .. }))
            {
                // (A LATERAL subquery planned apart, the LATERAL item as written, keeps its
                // references though the outputs reading them went (remove_unused_subquery_
                // outputs runs after find_lateral_references): its paths are still
                // parameterized by them.)
                (r2, Vec::new())
            } else if !params.is_empty() {
                let (r2, params2) = prune_params(r2, params2);
                order_lateral_scan(r2, params2)
            } else {
                (r2, params2)
            };
            let remap_cols = |cols: &[usize]| {
                let mut out = Vec::new();
                for &c in cols {
                    match map.get(c) {
                        Some(e) => {
                            for k in e.columns() {
                                if !out.contains(&k) {
                                    out.push(k);
                                }
                            }
                        }
                        None => out.push(c),
                    }
                }
                out
            };
            let mut meta2 = (**meta).clone();
            meta2.kept = remap_cols(&meta.kept);
            meta2.pushed = remap_cols(&meta.pushed);
            // (The equalities as written over the new row: a subquery's output its expression.)
            meta2.ec_written = meta.ec_written.iter().map(|e| subst(e, &map)).collect();
            // (Its relations the group's now; a LATERAL subquery pulled up into the right
            // side, whose values its relations still read.)
            meta2.lateral_sub = false;
            meta2.lateral_flat = flat && !params2.is_empty();
            meta2.lateral_held = kept_held;
            let mut nul = nl;
            nul.extend(nr);
            let _ = (nleft, nright);
            // remove_useless_result_rtes: a left join of an empty relation (a pulled up
            // FROM-less subquery or one-row VALUES) on true is its left side, the right
            // side's PlaceHolderVars evaluated there; one that computes nothing, whatever
            // its clause.
            // (A clause folded to true, `1 = 1` of two empty relations' values: none.)
            let cond2 = cond2.filter(|c| !matches!(c, Expr::Const(Value::Bool(true), _)));
            let fromless = |q: &Plan| match q {
                Plan::One => true,
                Plan::Project { input, .. } => matches!(&**input, Plan::One),
                _ => false,
            };
            if *kind == JoinKind::Left
                && matches!(wk, JoinKind::Left | JoinKind::Full)
                && fromless(&r2)
                && hash2.is_none()
                && params2.is_empty()
                && (cond2.is_none() || matches!(r2, Plan::One))
            {
                let plan = match (l2, r2) {
                    (l, Plan::One) => l,
                    (Plan::One, r) => r,
                    (l, Plan::Project { exprs, .. }) => {
                        let nl = crate::sql::binder::plan_width(&l);
                        let types = super::super::analyzejoins::col_types(&l);
                        let mut all: Vec<Expr> = (0..nl).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown))).collect();
                        all.extend(exprs);
                        cx.merged = true;
                        Plan::Project {
                            input: Arc::new(crate::sql::plan::mark_phv_eval(&l)),
                            exprs: all,
                        }
                    }
                    _ => unreachable!(),
                };
                return (plan, map, nul);
            }
            // (Its equalities with the empty relation's values, hash keys of the join:
            // plain clauses again, the values being constants of the other side.)
            let (hash2, cond2) = match hash2 {
                Some((lk, rk))
                    if (matches!(l2, Plan::One) || matches!(r2, Plan::One))
                        && *kind == JoinKind::Inner
                        && wk == JoinKind::Inner
                        && params2.is_empty() =>
                {
                    let mut eqs = Vec::new();
                    for (i, (a, b)) in lk.iter().zip(&rk).enumerate() {
                        let (a, bad, coll) = crate::sql::binder::hash_key_operand(a);
                        let (b, _, _) = crate::sql::binder::hash_key_operand(b);
                        let (a, b) = if meta.key_flipped.get(i) == Some(&true) { (b, a) } else { (a, b) };
                        if let Ok(r) = crate::sql::func::resolve_binop("=", &a.ty(), &b.ty()) {
                            let e = crate::sql::binder::eq_under(Expr::Call(r.def, vec![a, b], DataType::Bool), coll);
                            eqs.push(if bad { crate::sql::binder::indeterminate_eq(e) } else { e });
                        }
                    }
                    if eqs.len() == lk.len() {
                        let mut all = Vec::new();
                        if let Some(c) = cond2 {
                            conjuncts(c, &mut all);
                        }
                        all.extend(eqs);
                        (None, and_all(all))
                    } else {
                        (Some((lk, rk)), cond2)
                    }
                }
                // (A key the lifting made a constant (an output read in its own join's
                // clause): the keys are the join's clauses again, `c = c` of a constant
                // folded away.)
                Some((lk, rk))
                    if lk.iter().zip(&rk).any(|(a, b)| a.columns().is_empty() || b.columns().is_empty())
                        && params2.is_empty() =>
                {
                    let mut eqs = Vec::new();
                    for (i, (a, b)) in lk.iter().zip(&rk).enumerate() {
                        let (a, bad, coll) = crate::sql::binder::hash_key_operand(a);
                        let (b, _, _) = crate::sql::binder::hash_key_operand(b);
                        let b = b.shift_cols(nleft2 as isize);
                        let flip = meta.key_flipped.get(i) == Some(&true) || meta.hash_swapped.get(i) == Some(&true);
                        let (a, b) = if flip { (b, a) } else { (a, b) };
                        if a.columns().is_empty() && a.same_as(&b) && matches!(a, Expr::Const(ref v, _) if !matches!(v, Value::Null)) {
                            eqs.push(None);
                            continue;
                        }
                        if let Ok(r) = crate::sql::func::resolve_binop("=", &a.ty(), &b.ty()) {
                            let e = crate::sql::binder::eq_under(Expr::Call(r.def, vec![a, b], DataType::Bool), coll);
                            eqs.push(Some(if bad { crate::sql::binder::indeterminate_eq(e) } else { e }));
                        }
                    }
                    if eqs.len() == lk.len() {
                        let mut all = Vec::new();
                        if let Some(c) = cond2 {
                            conjuncts(c, &mut all);
                        }
                        all.extend(eqs.into_iter().flatten());
                        (None, and_all(all))
                    } else {
                        (Some((lk, rk)), cond2)
                    }
                }
                h => (h, cond2),
            };
            let cond2 = cond2.filter(|c| !matches!(c, Expr::Const(Value::Bool(true), _)));
            // remove_useless_result_rtes, by the join's kind after reduce_outer_joins: an
            // inner join with a FROM-less relation computing PlaceHolderVars is its other
            // side, the PlaceHolderVars evaluated there (remove_result_refs). (Its clauses
            // with the relation's values: plain clauses again, above.)
            if *kind == JoinKind::Inner
                && wk != JoinKind::Inner
                && params2.is_empty()
                && hash2.is_none()
                && (one_row(&l2).is_some_and(|e| !e.is_empty()) || one_row(&r2).is_some_and(|e| !e.is_empty()))
            {
                let (plan, map, nul) = merge_one_row(l2, r2, cond2, map, nul, nleft2);
                cx.merged = true;
                return (plan, map, nul);
            }
            // remove_useless_result_rtes: an inner join with an empty relation (a pulled
            // up one-row VALUES) is its other side, the join's clauses checked there.
            // (By its kind after reduce_outer_joins: an outer join made inner too, the
            // empty relation having no outputs a PlaceHolderVar could read.)
            if matches!(l2, Plan::One) && *kind == JoinKind::Inner && hash2.is_none() && params2.is_empty() {
                let plan = match cond2 {
                    Some(c) => Plan::Filter { input: Arc::new(r2), pred: c },
                    None => r2,
                };
                return (plan, map, nul);
            }
            if matches!(r2, Plan::One) && *kind == JoinKind::Inner && hash2.is_none() && params2.is_empty() {
                let plan = match (l2, cond2) {
                    (Plan::Join { kind: JoinKind::Inner, left, right, cond, hash, params, nleft, nright, meta }, Some(c)) => {
                        let mut all = Vec::new();
                        if let Some(c0) = cond {
                            conjuncts(c0, &mut all);
                        }
                        conjuncts(c, &mut all);
                        Plan::Join { kind: JoinKind::Inner, left, right, cond: and_all(all), hash, params, nleft, nright, meta }
                    }
                    (l, Some(c)) => Plan::Filter { input: Arc::new(l), pred: c },
                    (l, None) => l,
                };
                return (plan, map, nul);
            }
            // (An equivalence class with a value the lifting put in: `l = r` over an inner
            // join whose side has `r = k` gives the other side `l = k`, the join clause
            // implied by the two, the class's clauses each member's.)
            // (reduce_outer_joins comes first: an outer join it made inner is one too.)
            let (l2, r2, hash2, meta2) = if *kind == JoinKind::Inner && params2.is_empty() {
                ec_lifted_constants(l2, r2, hash2, meta2)
            } else {
                (l2, r2, hash2, meta2)
            };
            (
                Plan::Join {
                    kind: *kind,
                    left: Arc::new(l2),
                    right: Arc::new(r2),
                    cond: cond2,
                    hash: hash2,
                    params: params2,
                    nleft: nleft2,
                    nright: nright2,
                    meta: Box::new(meta2),
                },
                map,
                nul,
            )
        }
        // A semi / anti join of a subquery's join tree as bound (`flatten_join_subquery`):
        // pull_up_subqueries goes on into both its sides; its row is its left side's, its
        // right side's columns read by its clauses only (no outer join above nulls those).
        Plan::Join {
            kind: kind @ (JoinKind::Semi | JoinKind::Anti),
            left,
            right,
            cond,
            hash,
            params,
            nleft: _,
            nright: _,
            meta,
        } if cx.semi && params.is_empty() && meta.method.is_none() && meta.from_call.is_none() => {
            let (l2, ml, nl) = lift(left, nulled, anulled, None, cx);
            cx.merged = false;
            let nleft2 = crate::sql::binder::plan_width(&l2);
            let (r2, mr, _) = lift(right, 0, 0, None, cx);
            cx.merged = false;
            let nright2 = crate::sql::binder::plan_width(&r2);
            let mut map = ml.clone();
            map.extend(mr.iter().map(|e| e.clone().shift_cols(nleft2 as isize)));
            let cond2 = cond.as_ref().map(|c| subst(c, &map));
            let hash2 = hash.as_ref().map(|(lk, rk)| {
                let lk: Vec<Expr> = lk.iter().map(|e| subst(e, &ml)).collect();
                let rk: Vec<Expr> = rk.iter().map(|e| subst(e, &mr)).collect();
                (lk, rk)
            });
            // (A key that became a value of no column: not lifted.)
            if hash2
                .as_ref()
                .is_some_and(|(lk, rk)| lk.iter().chain(rk).any(|e| e.columns().is_empty()))
            {
                cx.bad = true;
            }
            let remap_cols = |cols: &[usize]| {
                let mut out = Vec::new();
                for &c in cols {
                    match map.get(c) {
                        Some(e) => {
                            for k in e.columns() {
                                if !out.contains(&k) {
                                    out.push(k);
                                }
                            }
                        }
                        None => out.push(c),
                    }
                }
                out
            };
            let mut meta2 = (**meta).clone();
            meta2.kept = remap_cols(&meta.kept);
            meta2.pushed = remap_cols(&meta.pushed);
            meta2.ec_written = meta.ec_written.iter().map(|e| subst(e, &map)).collect();
            (
                Plan::Join {
                    kind: *kind,
                    left: Arc::new(l2),
                    right: Arc::new(r2),
                    cond: cond2,
                    hash: hash2,
                    params: Vec::new(),
                    nleft: nleft2,
                    nright: nright2,
                    meta: Box::new(meta2),
                },
                ml,
                nl,
            )
        }
        // (Under an outer join that nulls it: an empty relation computing its values as
        // PlaceHolderVars, as a FROM-less select is.)
        Plan::Values { rows, .. }
            if rows.len() == 1
                && nulled != 0
                && rows[0].iter().all(movable)
                && rows[0].iter().all(|e| outer_refs(e).is_empty()) =>
        {
            let fromless = Plan::Project {
                input: Arc::new(Plan::One),
                exprs: rows[0].clone(),
            };
            lift(&fromless, nulled, anulled, lateral, cx)
        }
        // A one-row VALUES list (a FROM-less subquery's): pull_up_simple_values, an
        // empty relation (RTE_RESULT) whose values are computed where they're read,
        // when no outer join of the group nulls them.
        Plan::Values { rows, .. } if rows.len() == 1 && nulled == 0 && rows[0].iter().all(movable) => {
            if rows[0].iter().any(|e| !outer_refs(e).is_empty()) && lateral.is_none() {
                return identity(p, cx);
            }
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            (Plan::One, rows[0].clone(), Vec::new())
        }
        // (Projections over one: the FROM-less relation's values, composed.)
        Plan::Project { input, .. }
            if matches!(&**input, Plan::Project { .. }) && one_row(p).is_some_and(|e| !e.is_empty()) =>
        {
            let flat = Plan::Project {
                input: Arc::new(Plan::One),
                exprs: one_row(p).unwrap_or_default(),
            };
            lift(&flat, nulled, anulled, lateral, cx)
        }
        // A FROM-less subquery (an RTE_RESULT pulled up): its outputs computed where
        // they're read, those an outer join of the group nulls staying computed in it
        // (PlaceHolderVars evaluated at the empty relation).
        Plan::Project { input, exprs }
            if matches!(&**input, Plan::One)
                && !exprs.is_empty()
                && exprs.iter().all(|e| movable(e) || nulled == 0 && movable_fromless(e)) =>
        {
            if exprs.iter().any(|e| !outer_refs(e).is_empty()) && lateral.is_none() {
                return identity(p, cx);
            }
            let same_level = |i: &usize| {
                lateral.is_some_and(|(_, pn)| pn.get(*i).is_some_and(|n| nulled & !n.0 == 0))
            };
            let up: Vec<bool> = exprs
                .iter()
                .map(|e| {
                    if nulled == 0 {
                        return true;
                    }
                    if let Expr::Outer(1, i, _) = e {
                        return same_level(i);
                    }
                    outer_refs(e).iter().any(same_level) && !nonstrict(e)
                })
                .collect();
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            note_moved(exprs, &up, cx);
            let kept: Vec<Expr> = exprs.iter().zip(&up).filter(|(_, u)| !**u).map(|(e, _)| e.clone()).collect();
            let mut k = 0;
            let map: Vec<Expr> = exprs
                .iter()
                .zip(&up)
                .map(|(e, u)| {
                    if *u {
                        e.clone()
                    } else {
                        k += 1;
                        Expr::Col(k - 1, e.ty())
                    }
                })
                .collect();
            if kept.is_empty() {
                (Plan::One, map, Vec::new())
            } else {
                let w = kept.len();
                (
                    Plan::Project {
                        input: Arc::new(Plan::One),
                        exprs: kept,
                    },
                    map,
                    vec![(nulled, rel); w],
                )
            }
        }
        // (Its filter, the join clauses a LATERAL side took in, over the values: the
        // join's again, `lateral_quals`.)
        Plan::Filter { input, pred }
            if matches!(&**input, Plan::Values { rows, .. } if rows.len() == 1 && rows[0].iter().all(movable))
                && nulled == 0
                && lateral.is_some()
                && movable(pred) =>
        {
            let Plan::Values { rows, .. } = &**input else { unreachable!() };
            let row = rows[0].clone();
            let pred = subst(pred, &row);
            cx.lifted = true;
            cx.results = true;
            cx.nrel += 1;
            (
                Plan::Filter {
                    input: Arc::new(Plan::One),
                    pred,
                },
                row,
                Vec::new(),
            )
        }
        // A FROM subquery over one function scan (pull_up_simple_subquery): its computed
        // outputs evaluated where they're read, the function's row the group's relation
        // (a LATERAL function over them reads its expressions).
        Plan::Project { input, exprs }
            if matches!(&**input, Plan::FunctionScan { .. })
                && lateral.is_none()
                && nulled == 0
                && virtual_proj(p).is_none()
                && !reads_outer(input)
                && exprs.iter().any(|e| !matches!(e, Expr::Col(..)))
                && exprs.iter().all(|e| matches!(e, Expr::Col(..)) || movable(e) && outer_refs(e).is_empty()) =>
        {
            cx.lifted = true;
            cx.nrel += 1;
            let plan = (**input).clone();
            let w = crate::sql::binder::plan_width(&plan);
            (plan, exprs.clone(), vec![(nulled, rel); w])
        }
        _ => {
            if lateral.is_none()
                && let Some(r) = flatten_join_subquery(p, nulled, anulled, cx)
            {
                return r;
            }
            // A FROM subquery over a function call (a LATERAL one's select list reading the
            // left side, say): the function scan is the leaf, its outputs evaluated where
            // they're read, as for a table's; a LATERAL reference only its select list read
            // leaves the join without one (the scan doesn't read the left row).
            if let Plan::Project { input, exprs } = p
                && matches!(&**input, Plan::FunctionScan { .. })
                && exprs.iter().any(|e| !matches!(e, Expr::Col(..)))
                && exprs.iter().all(movable)
                && (lateral.is_some() || exprs.iter().all(|e| outer_refs(e).is_empty()))
            {
                let same_level = |i: &usize| {
                    lateral.is_some_and(|(_, pn)| pn.get(*i).is_some_and(|n| nulled & !n.0 == 0))
                };
                let up = exprs.iter().all(|e| {
                    if matches!(e, Expr::Col(..)) || nulled == 0 {
                        return true;
                    }
                    if let Expr::Outer(1, i, _) = e {
                        return same_level(i) || anulled == 0;
                    }
                    (!e.columns().is_empty() || outer_refs(e).iter().any(same_level)) && !nonstrict(e)
                });
                if up {
                    cx.lifted = true;
                    cx.nrel += 1;
                    let scan = (**input).clone();
                    let w = crate::sql::binder::plan_width(&scan);
                    return (scan, exprs.clone(), vec![(nulled, rel); w]);
                }
                // (A LATERAL one whose outputs are its columns and PlaceHolderVars evaluated
                // at its scan: the scan's reltarget, its columns first, then the
                // PlaceHolderVars (add_placeholders_to_base_rels).)
                let w = crate::sql::binder::plan_width(input);
                let kept: Vec<Expr> = exprs.iter().filter(|e| !matches!(e, Expr::Col(..))).cloned().collect();
                let ncol = exprs.iter().take_while(|e| matches!(e, Expr::Col(..))).count();
                if lateral.is_some() && !kept.is_empty() && ncol < exprs.len() - kept.len() {
                    cx.lifted = true;
                    cx.nrel += 1;
                    let types = super::super::analyzejoins::col_types(input);
                    let mut out: Vec<Expr> = (0..w).map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown))).collect();
                    out.extend(kept.iter().cloned());
                    let mut k = 0;
                    let map: Vec<Expr> = exprs
                        .iter()
                        .map(|e| match e {
                            Expr::Col(..) => e.clone(),
                            _ => {
                                k += 1;
                                Expr::Col(w + k - 1, e.ty())
                            }
                        })
                        .collect();
                    let plan = Plan::Project { input: input.clone(), exprs: out };
                    let pw = crate::sql::binder::plan_width(&plan);
                    return (plan, map, vec![(nulled, rel); pw]);
                }
            }
            // (A clause pushed down onto a single-table subquery from above, a WHERE over
            // its outputs: the subquery pulled up all the same, the clause over its outputs
            // as the query reads them, PlaceHolderVars included (one restricting the
            // relation the PlaceHolderVar is evaluated at, distribute_qual_to_rels).)
            if lateral.is_none()
                && let Plan::Filter { input, pred } = p
                && subquery_leaf(input).is_some()
                && clauses_movable(pred)
            {
                let before = std::mem::take(&mut cx.lifted);
                let (plan, map, nul) = lift(input, nulled, anulled, None, cx);
                let changed = cx.lifted;
                cx.lifted |= before;
                if !changed {
                    return (p.clone(), map, nul);
                }
                let pred = subst(pred, &map);
                let plan = match plan {
                    Plan::Scan(mut sp) => {
                        sp.filter = and_all(sp.filter.take().into_iter().chain([pred]).collect());
                        Plan::Scan(sp)
                    }
                    plan => Plan::Filter {
                        input: Arc::new(plan),
                        pred,
                    },
                };
                return (plan, map, nul);
            }
            let Some((sp, exprs)) = subquery_leaf(p) else {
                return identity(p, cx);
            };
            let mut sc = sp.clone();
            sc.access = Access::Unplanned;
            sc.index = None;
            sc.reverse = false;
            let scan = Plan::Scan(Box::new(sc));
            let sw = crate::sql::binder::plan_width(&scan);
            // Which outputs move up: those that need no PlaceHolderVar.
            let up: Vec<bool> = exprs
                .iter()
                .map(|e| {
                    // (A sub-select no outer join of the group nulls moves too:
                    // pullup_replace_vars puts a copy of it in each place reading it.)
                    if matches!(e, Expr::Col(..)) || !movable(e) && !(nulled == 0 && movable_fromless(e)) {
                        return false;
                    }
                    let refs = outer_refs(e);
                    if !refs.is_empty() && lateral.is_none() {
                        return false;
                    }
                    // Not nulled by an outer join of the group (as written): computed where
                    // it's read.
                    if nulled == 0 {
                        return true;
                    }
                    // The relations a LATERAL reference reads are under the same lowest
                    // nulling outer join as this one.
                    let same_level = |i: &usize| {
                        lateral.is_some_and(|(_, pn)| pn.get(*i).is_some_and(|n| nulled & !n.0 == 0))
                    };
                    // (A bare LATERAL reference no outer join nulls any more: the
                    // relation's Var itself, as the self-join removal sees it.)
                    if let Expr::Outer(1, i, _) = e {
                        return same_level(i) || anulled == 0;
                    }
                    let own = !e.columns().is_empty();
                    (own || refs.iter().any(same_level)) && !nonstrict(e)
                })
                .collect();
            note_moved(exprs, &up, cx);
            // (A subquery whose outputs are all columns: as it is, unless its WHERE
            // reads the LATERAL values, which become the join's. Nor one lifted already:
            // the relation's columns, then what stays computed.)
            let lateral_where = lateral.is_some() && sp.filter.as_ref().is_some_and(|f| !outer_refs(f).is_empty());
            let ncol = exprs.iter().take_while(|e| matches!(e, Expr::Col(..))).count();
            let canonical = ncol < exprs.len()
                && exprs[..ncol].windows(2).all(|w| matches!((&w[0], &w[1]), (Expr::Col(a, _), Expr::Col(b, _)) if a < b))
                && exprs[ncol..].iter().all(|e| !matches!(e, Expr::Col(..)))
                && !up.iter().any(|u| *u);
            // (A subquery selecting some of its table's columns is the table in the join
            // tree too, pulled up as any simple subquery: its paths the group's, the
            // parameterized ones included.)
            let picks = exprs.iter().all(|e| matches!(e, Expr::Col(..)))
                && lateral.is_none()
                && !(exprs.len() == sw && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)));
            if (exprs.iter().all(|e| matches!(e, Expr::Col(..))) && !picks || canonical) && !lateral_where {
                // (All the relation's columns, then its PlaceHolderVars: the scan is the join
                // group's relation, the PlaceHolderVars evaluated at it.)
                if canonical
                    && lateral.is_none()
                    && ncol == sw
                    && exprs[..ncol].iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
                {
                    let mut sc = sp.clone();
                    sc.phv_proj = true;
                    sc.access = Access::Unplanned;
                    sc.index = None;
                    sc.reverse = false;
                    let (_, map, nul) = identity(p, cx);
                    let plan = Plan::Project {
                        input: Arc::new(Plan::Scan(Box::new(sc))),
                        exprs: exprs.clone(),
                    };
                    return (plan, map, nul);
                }
                return identity(p, cx);
            }
            cx.lifted = true;
            cx.nrel += 1;
            let kept: Vec<Expr> = exprs
                .iter()
                .zip(&up)
                .filter(|(e, u)| !**u && !matches!(e, Expr::Col(..)))
                .map(|(e, _)| e.clone())
                .collect();
            // The scan's columns the projection returns (when it stays): those the query
            // reads (the outputs that are columns, the lifted expressions' columns) and
            // those of its clauses over the LATERAL values (join clauses), in order.
            let mut base: Vec<usize> = Vec::new();
            for (e, u) in exprs.iter().zip(&up) {
                if matches!(e, Expr::Col(..)) || *u {
                    base.extend(e.columns());
                }
            }
            if let Some(f) = &sp.filter {
                let mut parts = Vec::new();
                conjuncts(f.clone(), &mut parts);
                for q in parts.iter().filter(|q| !outer_refs(q).is_empty()) {
                    base.extend(q.columns());
                }
            }
            base.sort_unstable();
            base.dedup();
            // (Not a LATERAL item: the scan is the join group's relation, all its columns,
            // its PlaceHolderVars computed over it as the group evaluates them.)
            let phv_proj = !kept.is_empty() && lateral.is_none();
            if phv_proj {
                base = (0..sw).collect();
            }
            let at = |c: usize| if kept.is_empty() { c } else { base.iter().position(|&b| b == c).unwrap_or(c) };
            let to_out = |e: &Expr| -> Expr {
                e.clone()
                    .transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) => Expr::Col(at(c), t),
                            x => x,
                        })
                    })
                    .unwrap()
            };
            let nbase = if kept.is_empty() { sw } else { base.len() };
            let mut map = Vec::with_capacity(exprs.len());
            let mut k = 0;
            for (e, u) in exprs.iter().zip(&up) {
                map.push(match e {
                    Expr::Col(..) => to_out(e),
                    _ if *u => to_out(e),
                    _ => {
                        k += 1;
                        Expr::Col(nbase + k - 1, e.ty())
                    }
                });
            }
            let plan = if kept.is_empty() {
                scan
            } else {
                let mut out: Vec<Expr> = match &scan {
                    Plan::Scan(s) => base
                        .iter()
                        .map(|&i| Expr::Col(i, s.table.columns.get(i).map_or(DataType::Unknown, |c| c.ty.clone())))
                        .collect(),
                    _ => unreachable!(),
                };
                out.extend(kept);
                let scan = match scan {
                    Plan::Scan(mut s) if phv_proj => {
                        s.phv_proj = true;
                        Plan::Scan(s)
                    }
                    p => p,
                };
                Plan::Project {
                    input: Arc::new(scan),
                    exprs: out,
                }
            };
            let w = crate::sql::binder::plan_width(&plan);
            (plan, map, vec![(nulled, rel); w])
        }
    }
}

/// Column `c` of a lifted side when it is a pulled-up subquery's constant output (a
/// PlaceHolderVar computed by its projection) that no outer join inside the side nulls, as
/// written: the constant. None for anything else.
fn unnulled_const(p: &Plan, c: usize) -> Option<Expr> {
    match p {
        Plan::Project { input, exprs } => match exprs.get(c)? {
            Expr::Col(i, _) => unnulled_const(input, *i),
            // (A subquery over one table, its PlaceHolderVars evaluated at its scan; not an
            // empty relation's values merged into its partner, which an outer join removed
            // with it nulled as written.)
            e if matches!(&**input, Plan::Scan(sp) if sp.phv_proj) => {
                let mut plain = e.columns().is_empty() && !e.is_volatile() && outer_refs(e).is_empty();
                e.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..) | Expr::Outer(..)));
                plain.then(|| e.clone())
            }
            _ => None,
        },
        Plan::Filter { input, .. } => unnulled_const(input, c),
        Plan::Join {
            kind: kind @ (JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full),
            left,
            right,
            nleft,
            meta,
            ..
        } => {
            let wk = meta.written_kind.unwrap_or(*kind);
            if c < *nleft {
                (!matches!(wk, JoinKind::Right | JoinKind::Full)).then(|| unnulled_const(left, c)).flatten()
            } else {
                (!matches!(wk, JoinKind::Left | JoinKind::Full)).then(|| unnulled_const(right, c - nleft)).flatten()
            }
        }
        _ => None,
    }
}

/// A FROM-less one-row relation's values (a pulled-up select without FROM or one-row
/// VALUES, its projections composed): None for anything else.
fn one_row(p: &Plan) -> Option<Vec<Expr>> {
    match p {
        Plan::One => Some(Vec::new()),
        Plan::Project { input, exprs } => {
            let below = one_row(input)?;
            Some(exprs.iter().map(|e| subst(e, &below)).collect())
        }
        _ => None,
    }
}

/// An inner join of a FROM-less relation (`one_row`) with another side: that side, the
/// FROM-less relation's values computed over it as PlaceHolderVars (after its columns),
/// the join's clause a filter above. The map and nulling info follow the columns.
fn merge_one_row(
    l2: Plan,
    r2: Plan,
    cond: Option<Expr>,
    map: Vec<Expr>,
    nul: Vec<(u32, u64)>,
    nleft: usize,
) -> (Plan, Vec<Expr>, Vec<(u32, u64)>) {
    let left_one = one_row(&l2).is_some_and(|e| !e.is_empty());
    let (vals, other) = if left_one { (one_row(&l2).unwrap(), r2) } else { (one_row(&r2).unwrap(), l2) };
    let w = crate::sql::binder::plan_width(&other);
    // (Old column → new: the other side's columns first, then the values.)
    let reloc = |c: usize| -> usize {
        if left_one {
            if c < nleft { w + c } else { c - nleft }
        } else {
            c
        }
    };
    let map: Vec<Expr> = map.into_iter().map(|e| e.remap_cols(&reloc)).collect();
    let nul = if left_one {
        let (l, r) = nul.split_at(nleft.min(nul.len()));
        let mut n = r.to_vec();
        n.extend_from_slice(l);
        n
    } else {
        nul
    };
    let types = super::super::analyzejoins::col_types(&other);
    let mut exprs: Vec<Expr> = (0..w)
        .map(|i| Expr::Col(i, types.get(i).cloned().unwrap_or(DataType::Unknown)))
        .collect();
    exprs.extend(vals);
    // (A join computes them for the projection above it; a scan stays a leaf of its own.)
    let plan = Plan::Project {
        input: Arc::new(match other {
            Plan::Scan(_) => other,
            o => crate::sql::plan::mark_phv_join(o),
        }),
        exprs,
    };
    let plan = match cond {
        Some(c) => Plan::Filter {
            input: Arc::new(plan),
            pred: c.remap_cols(&reloc),
        },
        None => plan,
    };
    (plan, map, nul)
}

/// The scan's restriction clauses that read the LATERAL values (`params`, over the left
/// row; `pinfo`: the relations each reads) taken out as clauses over the join's row: join
/// clauses, unless the relations they read are among those the scan's own LATERAL
/// references need (its parameterized path then checks them).
fn lateral_quals(r: Plan, params: &[Expr], pinfo: &[(u32, u64)], nleft: usize) -> (Plan, Vec<Expr>) {
    // A filter over another plan (a VALUES list): its clauses reading the LATERAL values
    // that read more than the plan below does.
    if let Plan::Filter { input, pred } = &r
        && !matches!(&**input, Plan::Scan(_) | Plan::Project { .. })
    {
        return filter_lateral_quals(input, pred, params, pinfo, nleft).unwrap_or((r, Vec::new()));
    }
    let (sp, kept) = match &r {
        Plan::Scan(sp) => (sp, &[][..]),
        Plan::Project { input, exprs } => match &**input {
            Plan::Scan(sp) => {
                let ncol = exprs.iter().take_while(|e| matches!(e, Expr::Col(..))).count();
                (sp, &exprs[ncol..])
            }
            _ => return (r, Vec::new()),
        },
        _ => return (r, Vec::new()),
    };
    // A scan column's place in the side's row.
    let pos = |c: usize| -> Option<usize> {
        match &r {
            Plan::Project { exprs, .. } => exprs.iter().position(|e| matches!(e, Expr::Col(j, _) if *j == c)),
            _ => Some(c),
        }
    };
    let Some(f) = &sp.filter else {
        return (r, Vec::new());
    };
    if sp.sample.is_some()
        || sp.current_of.is_some()
        || sp.table.partkey.is_some()
        || sp.limit.is_some()
        || sp.mark.is_some()
    {
        return (r, Vec::new());
    }
    let rels_of = |e: &Expr| outer_refs(e).iter().fold(0u64, |m, &i| m | pinfo.get(i).map_or(u64::MAX, |x| x.1));
    let fine = |e: &Expr| {
        let mut fine = !e.is_volatile();
        e.walk(&mut |x| match x {
            Expr::Subquery(_) | Expr::Param(..) => fine = false,
            Expr::Outer(d, ..) if *d != 1 => fine = false,
            _ => {}
        });
        fine
    };
    let mut parts = Vec::new();
    conjuncts(f.clone(), &mut parts);
    // The relations the scan must have from the left side anyway.
    let mut need = kept.iter().fold(0u64, |m, e| m | rels_of(e));
    for q in &parts {
        if !outer_refs(q).is_empty() && !fine(q) {
            need |= rels_of(q);
        }
    }
    let (out, keep): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| {
        !outer_refs(e).is_empty()
            && fine(e)
            && (need == 0 || rels_of(e) & !need != 0)
            && e.columns().iter().all(|&c| pos(c).is_some())
    });
    if out.is_empty() {
        return (r, Vec::new());
    }
    let mut sp2 = sp.clone();
    sp2.filter = and_all(keep);
    // (Planned for the LATERAL values: the group plans it again.)
    sp2.access = Access::Unplanned;
    sp2.index = None;
    sp2.reverse = false;
    sp2.written.retain(|w| !out.iter().any(|o| o.same_as(w)));
    let r2 = match r.clone() {
        Plan::Scan(_) => Plan::Scan(sp2),
        Plan::Project { exprs, .. } => Plan::Project {
            input: Arc::new(Plan::Scan(sp2)),
            exprs,
        },
        _ => unreachable!(),
    };
    // (Nothing gained when the side stays LATERAL for no reason of its own.)
    if need == 0 && reads_outer(&r2) {
        return (r, Vec::new());
    }
    let moved = out
        .into_iter()
        .map(|e| {
            let e = e
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(c, t) => Expr::Col(nleft + pos(c).unwrap_or(c), t),
                        x => x,
                    })
                })
                .unwrap();
            subst_outer(e, params)
        })
        .collect();
    (r2, moved)
}

/// `lateral_quals` for a filter over a plan other than a scan: the clauses reading the
/// LATERAL values of relations the plan's own references don't need, as join clauses.
fn filter_lateral_quals(
    input: &Arc<Plan>,
    pred: &Expr,
    params: &[Expr],
    pinfo: &[(u32, u64)],
    nleft: usize,
) -> Option<(Plan, Vec<Expr>)> {
    let rels_of = |e: &Expr| outer_refs(e).iter().fold(0u64, |m, &i| m | pinfo.get(i).map_or(u64::MAX, |x| x.1));
    let fine = |e: &Expr| {
        let mut fine = !e.is_volatile();
        e.walk(&mut |x| match x {
            Expr::Subquery(_) | Expr::Param(..) => fine = false,
            Expr::Outer(d, ..) if *d != 1 => fine = false,
            _ => {}
        });
        fine
    };
    let mut below = Vec::new();
    crate::sql::explain::node_exprs(input, &mut below);
    let need = below.iter().fold(0u64, |m, e| m | rels_of(e));
    if reads_outer_below(input) {
        return None;
    }
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    let (out, keep): (Vec<Expr>, Vec<Expr>) = parts
        .into_iter()
        .partition(|e| !outer_refs(e).is_empty() && fine(e) && (need == 0 || rels_of(e) & !need != 0));
    if out.is_empty() {
        return None;
    }
    let r2 = match and_all(keep) {
        Some(k) => Plan::Filter { input: input.clone(), pred: k },
        None => (**input).clone(),
    };
    let moved = out
        .into_iter()
        .map(|e| subst_outer(e.shift_cols(nleft as isize), params))
        .collect();
    Some((r2, moved))
}

/// Whether a plan's inputs (not the node itself) read the LATERAL values.
fn reads_outer_below(p: &Plan) -> bool {
    crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_outer(c))
}

/// A LATERAL scan's clauses over a pulled-up subquery's PlaceHolderVar that reads only the
/// LATERAL values (`phv_quals`): the PlaceHolderVar is a nestloop parameter of its own
/// (computed on the outer side, a Memoize's cache key), the clauses read it as one.
fn hoist_phv_params(r: Plan, mut params: Vec<Expr>) -> (Plan, Vec<Expr>) {
    // (Under the projection computing its PlaceHolderVars: the scan's clauses.)
    if let Plan::Project { input, exprs } = &r
        && matches!(&**input, Plan::Scan(_))
    {
        let (scan, params) = hoist_phv_params((**input).clone(), params);
        return (
            Plan::Project {
                input: Arc::new(scan),
                exprs: exprs.clone(),
            },
            params,
        );
    }
    let Plan::Scan(sp) = &r else {
        return (r, params);
    };
    if sp.phv_quals.is_empty() || !matches!(sp.access, Access::Unplanned) || sp.index.is_some() {
        return (r, params);
    }
    let mut changed = false;
    let mut hoist = |e: &Expr, params: &mut Vec<Expr>| -> Expr {
        e.clone()
            .transform(&mut |x| {
                if x.columns().is_empty()
                    && !matches!(x, Expr::Outer(..) | Expr::Const(..))
                    && !outer_refs(&x).is_empty()
                    && movable(&x)
                    && outer_refs(&x).iter().all(|i| *i < params.len())
                {
                    let v = subst_outer(x.clone(), params);
                    let ty = x.ty();
                    let k = match params.iter().position(|p| p.same_as(&v)) {
                        Some(k) => k,
                        None => {
                            params.push(v);
                            params.len() - 1
                        }
                    };
                    changed = true;
                    return Ok(Expr::Outer(1, k, ty));
                }
                Ok(x)
            })
            .unwrap()
    };
    let mut sp2 = (**sp).clone();
    let phv: Vec<Expr> = sp2.phv_quals.iter().map(|q| hoist(q, &mut params)).collect();
    let fix = |q: &Expr, params: &mut Vec<Expr>, hoist: &mut dyn FnMut(&Expr, &mut Vec<Expr>) -> Expr| {
        if sp.phv_quals.iter().any(|p| p.same_as(q)) { hoist(q, params) } else { q.clone() }
    };
    let mut parts = Vec::new();
    if let Some(f) = &sp2.filter {
        conjuncts(f.clone(), &mut parts);
    }
    let parts: Vec<Expr> = parts.iter().map(|q| fix(q, &mut params, &mut hoist)).collect();
    sp2.written = sp2.written.iter().map(|q| fix(q, &mut params, &mut hoist)).collect();
    sp2.filter = and_all(parts);
    sp2.phv_quals = phv;
    if !changed {
        return (r, params);
    }
    (Plan::Scan(Box::new(sp2)), params)
}

/// A LATERAL scan whose clauses over its PlaceHolderVars (`phv_quals`: restriction clauses
/// in Postgres, the PlaceHolderVar evaluated at the scan) sit with clauses reading the
/// LATERAL values directly (join clauses, its parameterized path's): the restrictions first,
/// as a scan's qual list has them, and the join clauses' values the first parameters
/// (a Memoize's cache keys: the parameterized path's clauses', then its lateral Vars).
fn order_lateral_scan(r: Plan, params: Vec<Expr>) -> (Plan, Vec<Expr>) {
    let sp = match &r {
        Plan::Scan(sp) => sp,
        Plan::Project { input, .. } => match &**input {
            Plan::Scan(sp) => sp,
            _ => return (r, params),
        },
        _ => return (r, params),
    };
    if sp.phv_quals.is_empty() || !matches!(sp.access, Access::Unplanned) || sp.index.is_some() {
        return (r, params);
    }
    let Some(f) = &sp.filter else { return (r, params) };
    let mut parts = Vec::new();
    conjuncts(f.clone(), &mut parts);
    let is_phv = |q: &Expr| sp.phv_quals.iter().any(|p| p.same_as(q));
    let (restr, joins): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|q| is_phv(q) || outer_refs(q).is_empty());
    if restr.is_empty() || joins.is_empty() || !restr.iter().any(is_phv) {
        return (r, params);
    }
    // (The join clauses' parameters first.)
    let mut order: Vec<usize> = Vec::new();
    for q in &joins {
        for k in outer_refs(q) {
            if k < params.len() && !order.contains(&k) {
                order.push(k);
            }
        }
    }
    for k in 0..params.len() {
        if !order.contains(&k) {
            order.push(k);
        }
    }
    let fix = |e: &Expr| -> Expr {
        e.clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, k, t) => Expr::Outer(1, order.iter().position(|&j| j == k).unwrap_or(k), t),
                    x => x,
                })
            })
            .unwrap()
    };
    let mut sp2 = (**sp).clone();
    let mut all = restr;
    all.extend(joins);
    sp2.filter = and_all(all.iter().map(&fix).collect());
    sp2.written = sp2.written.iter().map(&fix).collect();
    sp2.phv_quals = sp2.phv_quals.iter().map(&fix).collect();
    let params: Vec<Expr> = order.iter().map(|&k| params[k].clone()).collect();
    let r2 = match &r {
        Plan::Scan(_) => Plan::Scan(Box::new(sp2)),
        Plan::Project { exprs, .. } => Plan::Project {
            input: Arc::new(Plan::Scan(Box::new(sp2))),
            exprs: exprs.iter().map(&fix).collect(),
        },
        _ => unreachable!(),
    };
    (r2, params)
}

/// A LATERAL scan's parameters it still reads, renumbered (the left side's values the join
/// passes down are what its references need, no more).
fn prune_params(r: Plan, params: Vec<Expr>) -> (Plan, Vec<Expr>) {
    // (A VALUES list's: the ones its rows read.)
    let values = match &r {
        Plan::Values { .. } => true,
        Plan::Filter { input, .. } => matches!(&**input, Plan::Values { .. }),
        _ => false,
    };
    if values {
        let mut exprs = Vec::new();
        crate::sql::explain::node_exprs(&r, &mut exprs);
        if let Plan::Filter { input, .. } = &r {
            crate::sql::explain::node_exprs(input, &mut exprs);
        }
        let mut used = vec![false; params.len()];
        for e in &exprs {
            for i in outer_refs(e) {
                match used.get_mut(i) {
                    Some(u) => *u = true,
                    None => return (r, params),
                }
            }
        }
        if used.iter().all(|u| *u) {
            return (r, params);
        }
        let pos: Vec<Option<usize>> = used
            .iter()
            .scan(0usize, |n, &u| {
                Some(u.then(|| {
                    *n += 1;
                    *n - 1
                }))
            })
            .collect();
        let fix = |e: &Expr| -> Expr {
            e.clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, i, t) => Expr::Outer(1, pos[i].unwrap_or(i), t),
                        x => x,
                    })
                })
                .unwrap()
        };
        let fix_values = |v: &Plan| match v {
            Plan::Values { rows, ncols, rti } => Plan::Values {
                rows: rows.iter().map(|row| row.iter().map(&fix).collect()).collect(),
                ncols: *ncols,
                rti: *rti,
            },
            p => p.clone(),
        };
        let r2 = match &r {
            Plan::Filter { input, pred } => Plan::Filter {
                input: Arc::new(fix_values(input)),
                pred: fix(pred),
            },
            v => fix_values(v),
        };
        let params = params.into_iter().zip(&used).filter(|(_, u)| **u).map(|(p, _)| p).collect();
        return (r2, params);
    }
    let scan_only = match &r {
        Plan::Scan(_) => true,
        Plan::Project { input, .. } => matches!(&**input, Plan::Scan(_)),
        _ => false,
    };
    if !scan_only {
        return (r, params);
    }
    let mut used = vec![false; params.len()];
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(&r, &mut exprs);
    let scan = match &r {
        Plan::Project { input, .. } => &**input,
        p => p,
    };
    if !std::ptr::eq(scan, &r) {
        crate::sql::explain::node_exprs(scan, &mut exprs);
    }
    if let Plan::Scan(sp) = scan {
        // (A path planned over them reads them in its conditions too.)
        if !matches!(sp.access, Access::Unplanned) || sp.index.is_some() {
            return (r, params);
        }
        exprs.extend(sp.written.iter().chain(&sp.phv_quals).chain(&sp.current_of));
    }
    for e in &exprs {
        for i in outer_refs(e) {
            match used.get_mut(i) {
                Some(u) => *u = true,
                None => return (r, params),
            }
        }
    }
    if used.iter().all(|u| *u) {
        return (r, params);
    }
    let pos: Vec<Option<usize>> = used
        .iter()
        .scan(0usize, |n, &u| {
            Some(u.then(|| {
                *n += 1;
                *n - 1
            }))
        })
        .collect();
    let fix = |e: &Expr| -> Expr {
        e.clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Outer(1, i, t) => Expr::Outer(1, pos[i].unwrap_or(i), t),
                    x => x,
                })
            })
            .unwrap()
    };
    let fix_scan = |sp: &ScanPlan| {
        let mut sp = sp.clone();
        sp.filter = sp.filter.as_ref().map(fix);
        sp.written = sp.written.iter().map(&fix).collect();
        sp.phv_quals = sp.phv_quals.iter().map(&fix).collect();
        sp
    };
    let r2 = match &r {
        Plan::Scan(sp) => Plan::Scan(Box::new(fix_scan(sp))),
        Plan::Project { input, exprs } => match &**input {
            Plan::Scan(sp) => Plan::Project {
                input: Arc::new(Plan::Scan(Box::new(fix_scan(sp)))),
                exprs: exprs.iter().map(&fix).collect(),
            },
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let params = params.into_iter().zip(&used).filter(|(_, u)| **u).map(|(p, _)| p).collect();
    (r2, params)
}

/// The sub-selects of the outputs that move up (`up`), for `copy_moved_subselects`.
fn note_moved(exprs: &[Expr], up: &[bool], cx: &mut Cx) {
    for (e, _) in exprs.iter().zip(up).filter(|(_, u)| **u) {
        e.walk(&mut |x| {
            if let Expr::Subquery(s) = x
                && s.call.is_none()
            {
                cx.moved.push(s.id);
            }
        });
    }
}

/// pullup_replace_vars puts a copy of a pulled-up subquery's output in each place reading
/// it, so a sub-select there is planned once per place (make_subplan for each copy of the
/// SubLink): its own SubPlan, run where that place is evaluated. The sub-selects of the
/// outputs moved up (`moved`) met again after their first place, the group's target list
/// (`map`, preprocessed first) then its clauses, get an id and plan nodes of their own.
fn copy_moved_subselects(plan: &mut Plan, map: &mut [Expr], moved: &[usize]) {
    if moved.is_empty() {
        return;
    }
    let moved: std::collections::HashSet<usize> = moved.iter().copied().collect();
    let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // (`tlist`: the place is the target list's, else a qual's.)
    let mut fix = |e: &mut Expr, tlist: bool| {
        let mut any = false;
        e.walk(&mut |x| any |= matches!(x, Expr::Subquery(s) if moved.contains(&s.id)));
        if !any {
            return;
        }
        // (Each reference a copy: `x + x` reads two.)
        *e = e
            .clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Subquery(mut s) if moved.contains(&s.id) => {
                        if !seen.insert(s.id) {
                            crate::sql::paths::sublinkcopy::make_copy(&mut s);
                        }
                        s.tlist = tlist;
                        s.qual = !tlist;
                        Expr::Subquery(s)
                    }
                    x => x,
                })
            })
            .unwrap_or_else(|_| e.clone());
    };
    for e in map.iter_mut() {
        fix(e, true);
    }
    // (The join tree's quals in preprocess_qual_conditions' order: a join's inputs', then
    // its own.)
    fn go(p: &mut Plan, fix: &mut dyn FnMut(&mut Expr, bool)) {
        for c in crate::sql::explain::plan_inputs_mut(p) {
            go(Arc::make_mut(c), fix);
        }
        match p {
            Plan::Join { cond, hash, params, .. } => {
                params.iter_mut().for_each(|e| fix(e, true));
                if let Some((l, r)) = hash {
                    l.iter_mut().chain(r.iter_mut()).for_each(|e| fix(e, false));
                }
                if let Some(c) = cond {
                    fix(c, false);
                }
            }
            Plan::Filter { pred, .. } => fix(pred, false),
            Plan::Project { exprs, .. } => exprs.iter_mut().for_each(|e| fix(e, true)),
            _ => {}
        }
    }
    go(plan, &mut fix);
}

/// The join tree at `p` with its subqueries' liftable outputs taken out of their scans;
/// None when nothing moves.
pub(super) fn lift_group(p: &Plan, bound: &[crate::sql::paths::BoundTree]) -> Option<Lifted> {
    if !matches!(p, Plan::Join { .. }) {
        return None;
    }
    let mut cx = Cx {
        next: 0,
        lifted: false,
        bad: false,
        nrel: 0,
        results: false,
        merged: false,
        moved: Vec::new(),
        bound: bound.to_vec(),
        semi: false,
        unbind: false,
        frame: None,
    };
    let (mut plan, map, _) = lift(p, 0, 0, None, &mut cx);
    if !cx.lifted || cx.bad {
        return None;
    }
    let mut map = map;
    copy_moved_subselects(&mut plan, &mut map, &cx.moved);
    // (Columns read through the map keep their types.)
    let types = super::super::analyzejoins::col_types(p);
    let map = map
        .into_iter()
        .zip(types)
        .map(|(e, t)| match e {
            Expr::Col(j, DataType::Unknown) => Expr::Col(j, t),
            e => e,
        })
        .collect();
    Some(Lifted { plan, map, results: cx.results })
}

/// The `col = k` clauses (k a pseudo-constant a lifting put in: a value of no column) at the
/// top of a join side: the column and the value.
fn lifted_constants(p: &Plan) -> Vec<(usize, Expr)> {
    let pred = match p {
        Plan::Filter { input, pred } if matches!(&**input, Plan::Scan(_)) => Some(pred),
        // (A side that is an inner join (one join domain, reduce_outer_joins having made it
        // inner): its items' `x = k`, as its columns, the class being one across it.)
        Plan::Join {
            kind: JoinKind::Inner,
            left,
            right,
            nleft,
            params,
            cond,
            ..
        } if params.is_empty() => {
            let mut out = lifted_constants(left);
            out.extend(lifted_constants(right).into_iter().map(|(i, k)| (i + nleft, k)));
            // (And its own clauses equating a column with the value: a key the lifting
            // made `x = k`.)
            out.extend(const_clauses(cond.as_ref()));
            return out;
        }
        // (A projection over one computing PlaceHolderVars: its columns passed through.)
        Plan::Project { input, exprs } if crate::sql::plan::phv_projection(p).is_some() => {
            let below = lifted_constants(input);
            return exprs
                .iter()
                .enumerate()
                .filter_map(|(o, e)| match e {
                    Expr::Col(j, _) => below.iter().find(|(c, _)| c == j).map(|(_, k)| (o, k.clone())),
                    _ => None,
                })
                .collect();
        }
        _ => None,
    };
    const_clauses(pred)
}

/// The `col = k` clauses of `pred` (k a pseudo-constant of no column, not a plain constant).
fn const_clauses(pred: Option<&Expr>) -> Vec<(usize, Expr)> {
    let mut out = Vec::new();
    let Some(pred) = pred else { return out };
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    for q in parts {
        let Expr::Call(d, args, _) = &q else { continue };
        if d.name != "=" || args.len() != 2 || d.coll.as_ref().is_some_and(|c| !c.deterministic) {
            continue;
        }
        for (a, b) in [(&args[0], &args[1]), (&args[1], &args[0])] {
            if let Expr::Col(i, _) = a
                && b.columns().is_empty()
                && !matches!(b, Expr::Const(..))
                && movable(b)
                && crate::sql::binder::optimize::pseudo_constant(b)
            {
                out.push((*i, b.clone()));
            }
        }
    }
    out
}

/// `lifted_constants` across an inner join's hash keys (generate_base_implied_equalities_const
/// over a class the pulled-up value is a member of): each side gets `member = value`, the
/// written `value = member` turned round as the class generates it, and the key goes.
fn ec_lifted_constants(
    l: Plan,
    r: Plan,
    hash: Option<(Vec<Expr>, Vec<Expr>)>,
    mut meta: crate::sql::plan::JoinMeta,
) -> (Plan, Plan, Option<(Vec<Expr>, Vec<Expr>)>, crate::sql::plan::JoinMeta) {
    let Some((lk, rk)) = hash else { return (l, r, None, meta) };
    let (lc, rc) = (lifted_constants(&l), lifted_constants(&r));
    if lc.is_empty() && rc.is_empty() {
        return (l, r, Some((lk, rk)), meta);
    }
    let scan_side = |p: &Plan| match p {
        Plan::Scan(_) => true,
        Plan::Filter { input, .. } => matches!(&**input, Plan::Scan(_)),
        _ => false,
    };
    let eq_of = |a: &Expr, k: &Expr| -> Option<Expr> {
        let res = crate::sql::func::resolve_binop("=", &a.ty(), &k.ty()).ok()?;
        Some(Expr::Call(res.def, vec![a.clone(), k.clone()], DataType::Bool))
    };
    let (mut lpush, mut rpush) = (Vec::new(), Vec::new());
    let (mut lk2, mut rk2) = (Vec::new(), Vec::new());
    let (mut hs, mut kf) = (Vec::new(), Vec::new());
    let nl = crate::sql::binder::plan_width(&l);
    for (i, (a, b)) in lk.iter().zip(&rk).enumerate() {
        let k = match (a, b) {
            (Expr::Col(x, _), Expr::Col(y, _)) => rc
                .iter()
                .find(|(c, _)| c == y)
                .map(|(_, k)| (k.clone(), true))
                .or_else(|| lc.iter().find(|(c, _)| c == x).map(|(_, k)| (k.clone(), false))),
            _ => None,
        };
        match k.and_then(|(k, from_r)| Some((eq_of(a, &k)?, eq_of(b, &k)?, from_r))) {
            Some((le, re, from_r)) => {
                // (A join side holding the constant has its `member = value` in its item's
                // scan already.)
                if from_r || scan_side(&l) {
                    lpush.push(le);
                }
                if !from_r || scan_side(&r) {
                    rpush.push(re);
                }
                // (The clause the class absorbed still has its columns carried up to the
                // join: distribute_qual_to_rels' add_vars_to_targetlist.)
                if let (Expr::Col(x, _), Expr::Col(y, _)) = (a, b) {
                    for c in [*x, nl + *y] {
                        if !meta.kept.contains(&c) {
                            meta.kept.push(c);
                        }
                    }
                }
            }
            None => {
                lk2.push(a.clone());
                rk2.push(b.clone());
                hs.push(meta.hash_swapped.get(i).copied().unwrap_or(false));
                kf.push(meta.key_flipped.get(i).copied().unwrap_or(false));
            }
        }
    }
    if lk2.len() == lk.len() {
        return (l, r, Some((lk, rk)), meta);
    }
    // (A side's own `value = member` is the class's `member = value` now.)
    let regen = |p: Plan, push: Vec<Expr>| -> Plan {
        if push.is_empty() {
            return p;
        }
        let (input, pred) = match p {
            Plan::Filter { input, pred } => (input, Some(pred)),
            p => (Arc::new(p), None),
        };
        let mut parts = Vec::new();
        if let Some(pr) = pred {
            conjuncts(pr, &mut parts);
        }
        let mut out: Vec<Expr> = Vec::new();
        for q in parts {
            let same = push.iter().any(|e| match (e, &q) {
                (Expr::Call(_, ea, _), Expr::Call(d, qa, _)) => {
                    d.name == "=" && qa.len() == 2 && ea[0].same_as(&qa[1]) && ea[1].same_as(&qa[0])
                }
                _ => false,
            });
            if !same {
                out.push(q);
            }
        }
        for e in push {
            if !out.iter().any(|q| q.same_as(&e)) {
                out.push(e);
            }
        }
        Plan::Filter { input, pred: Expr::And(out) }
    };
    let l = regen(l, lpush);
    let r = regen(r, rpush);
    meta.hash_swapped = hs;
    meta.key_flipped = kf;
    let hash = (!lk2.is_empty()).then_some((lk2, rk2));
    (l, r, hash, meta)
}
