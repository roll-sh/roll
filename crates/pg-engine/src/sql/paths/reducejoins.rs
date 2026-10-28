//! reduce_outer_joins (prepjointree.c): an outer join whose nullable side a strict qual
//! above it reads can't produce the null-extended rows that qual would reject, so it is
//! an inner join (a full join: one-sided); a left join whose own clauses are strict for
//! a column an upper `IS NULL` forces to null keeps only its null-extended rows, so it
//! is an anti join (and the `IS NULL` goes).

use super::*;
use crate::sql::binder::optimize::join;
use crate::sql::expr::BoolTest;
use std::collections::BTreeSet;

type Cols = BTreeSet<usize>;

/// Columns an expression's value is NULL whenever one of them is (a strict expression of
/// them).
fn value_cols(e: &Expr, out: &mut Cols) {
    match e {
        Expr::Col(c, _) => {
            out.insert(*c);
        }
        Expr::Call(d, args, _) if d.name == crate::sql::binder::INDETERMINATE_HASH => {
            args.iter().take(1).for_each(|a| value_cols(a, out))
        }
        Expr::Call(d, args, _) if d.strict => args.iter().for_each(|a| value_cols(a, out)),
        Expr::Cast(x, ..) | Expr::Relabel(x, _) | Expr::Collate(x, ..) => value_cols(x, out),
        _ => {}
    }
}

/// find_nonnullable_vars at column level: the columns that must be non-null for `e` to be
/// true (at the top level) or non-null (below it).
fn nonnull_cols(e: &Expr, top: bool) -> Cols {
    let meet = |args: &[Expr], top: bool| {
        let mut it = args.iter().map(|a| nonnull_cols(a, top));
        let first = it.next().unwrap_or_default();
        it.fold(first, |m, x| m.intersection(&x).copied().collect())
    };
    match e {
        Expr::Col(c, _) => Cols::from([*c]),
        Expr::And(args) if top => args.iter().flat_map(|a| nonnull_cols(a, true)).collect(),
        Expr::And(args) => meet(args, false),
        Expr::Or(args) => meet(args, top),
        Expr::Not(x) => nonnull_cols(x, false),
        Expr::Call(d, args, _) if d.strict && d.name != crate::sql::binder::INDETERMINATE_HASH => {
            args.iter().flat_map(|a| nonnull_cols(a, false)).collect()
        }
        Expr::Cast(x, ..) | Expr::Relabel(x, _) | Expr::Collate(x, ..) => nonnull_cols(x, top),
        Expr::IsNull(x, true) if top && !is_row(&x.ty()) => nonnull_cols(x, false),
        Expr::BoolTest(x, BoolTest::IsTrue | BoolTest::IsFalse | BoolTest::IsNotUnknown)
            if top =>
        {
            nonnull_cols(x, false)
        }
        // (is_strict_saop: ALL over an empty array is true whatever the left side, ANY false;
        // so ANY at the top level, where false is as good as NULL, else an array known to have
        // elements: `NOT (x < ANY ('{}'))` is true for a NULL x.)
        Expr::ArrayOp { left, right, op, all }
            if op.strict && matches!(&**right, Expr::Const(..) | Expr::Array(..)) && ((!*all && top) || saop_nonempty(right)) =>
        {
            nonnull_cols(left, false)
        }
        Expr::InList(x, items, _) if !items.is_empty() && items.iter().all(|(_, d)| d.strict) => {
            nonnull_cols(x, false)
        }
        // A SubPlan's test expression (the sub-link's left side compared with its columns):
        // ANY at the top level, a row comparison anywhere, are as strict as it is.
        Expr::Subquery(sq)
            if sq.test_op.is_none()
                && sq.left.len() == sq.ops.len()
                && ((top && sq.kind == crate::sql::expr::SubqueryKind::Any)
                    || sq.kind == crate::sql::expr::SubqueryKind::RowCompare) =>
        {
            let per: Vec<Cols> = sq
                .left
                .iter()
                .zip(&sq.ops)
                .map(|(l, op)| if op.strict { nonnull_cols(l, false) } else { Cols::new() })
                .collect();
            match per.len() {
                0 => Cols::new(),
                1 => per.into_iter().next().unwrap(),
                // (Several columns: their comparisons ANDed, `=`; `<>` ORed; an ordering
                // comparison is a RowCompareExpr, which proves nothing.)
                _ if sq.ops.iter().all(|o| o.name == "=") => {
                    if top || sq.kind == crate::sql::expr::SubqueryKind::Any {
                        per.into_iter().flatten().collect()
                    } else {
                        let mut it = per.into_iter();
                        let first = it.next().unwrap_or_default();
                        it.fold(first, |m, x| m.intersection(&x).copied().collect())
                    }
                }
                _ if sq.ops.iter().all(|o| o.name == "<>") => {
                    let mut it = per.into_iter();
                    let first = it.next().unwrap_or_default();
                    it.fold(first, |m, x| m.intersection(&x).copied().collect())
                }
                _ => Cols::new(),
            }
        }
        _ => Cols::new(),
    }
}

/// An array operand known to have elements (is_strict_saop's test for ALL).
pub(crate) fn saop_nonempty(e: &Expr) -> bool {
    match e {
        Expr::Const(crate::types::Value::Array(a), _) => !a.items.is_empty(),
        Expr::Array(items, _) => !items.is_empty(),
        _ => false,
    }
}

/// Whether column `c` of `p` goes NULL with the rows it comes from: a column, or a strict
/// expression of one (a constant a subquery outputs is a PlaceHolderVar, which a strict
/// clause over doesn't make its rel non-nullable).
fn var_like(p: &Plan, c: usize) -> bool {
    match p {
        Plan::Project { input, exprs } => match exprs.get(c) {
            Some(Expr::Col(j, _)) => var_like(input, *j),
            Some(e) => {
                let mut v = Cols::new();
                value_cols(e, &mut v);
                if v.is_empty() {
                    // A PlaceHolderVar over one rel behaves like its Var (its syntactic
                    // scope a singleton: a table, a pulled-up VALUES row or FROM-less
                    // select), whatever it reads.
                    single_rel(input)
                } else {
                    v.iter().any(|j| var_like(input, *j))
                }
            }
            None => false,
        },
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => var_like(input, c),
        Plan::Join {
            left, right, nleft, ..
        } => {
            if c < *nleft {
                var_like(left, c)
            } else {
                var_like(right, c - nleft)
            }
        }
        _ => true,
    }
}

/// A plan over one relation (no join below its filters and projections).
fn single_rel(p: &Plan) -> bool {
    match p {
        // (A join remove_useless_joins took out: its inner side's columns NULL constants
        // over the outer side. Its scope had both relations.)
        Plan::Project { exprs, .. } if exprs.iter().any(|e| matches!(e, Expr::Const(Value::Null, _))) => false,
        Plan::Filter { input, .. } | Plan::Project { input, .. } => single_rel(input),
        Plan::Join { .. } => false,
        _ => true,
    }
}

/// The boolean columns `e` (a qual, at the top level) needs TRUE: itself, or its ANDed
/// arms.
fn truth_cols(e: &Expr) -> Cols {
    match e {
        Expr::Col(c, DataType::Bool) => Cols::from([*c]),
        Expr::And(args) => args.iter().flat_map(truth_cols).collect(),
        _ => Cols::new(),
    }
}

/// Whether column `c` of `p` being non-null (TRUE, when `top`) means some row under `p`
/// isn't null-extended: a column of it, or a PlaceHolderVar whose contents force one
/// (find_nonnullable_rels looks into a PlaceHolderVar's expression).
fn nonnull_reaches(p: &Plan, c: usize, top: bool) -> bool {
    if var_like(p, c) {
        return true;
    }
    match p {
        Plan::Project { input, exprs } => match exprs.get(c) {
            Some(Expr::Col(j, _)) => nonnull_reaches(input, *j, top),
            Some(e) => {
                let truth = if top { truth_cols(e) } else { Cols::new() };
                nonnull_cols(e, top)
                    .into_iter()
                    .any(|j| nonnull_reaches(input, j, truth.contains(&j)))
            }
            None => false,
        },
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => nonnull_reaches(input, c, top),
        Plan::Join {
            left, right, nleft, ..
        } => {
            if c < *nleft {
                nonnull_reaches(left, c, top)
            } else {
                nonnull_reaches(right, c - nleft, top)
            }
        }
        _ => false,
    }
}

fn is_row(t: &DataType) -> bool {
    matches!(t, DataType::Record | DataType::Row(_))
}

/// find_forced_null_var: the column `x IS NULL` (or `x IS UNKNOWN`) tests.
fn forced_null_col(e: &Expr) -> Option<usize> {
    match e {
        Expr::IsNull(x, false) | Expr::BoolTest(x, BoolTest::IsUnknown) => match &**x {
            Expr::Col(c, t) if !is_row(t) => Some(*c),
            _ => None,
        },
        _ => None,
    }
}

/// find_forced_null_vars: over a clause's top-level conjuncts.
fn forced_null_cols(parts: &[Expr]) -> Cols {
    parts.iter().filter_map(forced_null_col).collect()
}

fn clause_nonnull(parts: &[Expr]) -> Cols {
    parts.iter().flat_map(|p| nonnull_cols(p, true)).collect()
}

/// The columns a WHERE's conjuncts are strict for (an outer join below whose nullable side
/// has one of them is reduced).
pub(crate) fn strict_cols(parts: &[Expr]) -> Cols {
    clause_nonnull(parts)
}

fn range(s: &Cols, lo: usize, hi: usize) -> Cols {
    s.range(lo..hi).map(|c| c - lo).collect()
}

/// What reducing a subtree did: whether anything changed, and its output columns an
/// anti join made always NULL.
#[derive(Default)]
struct Done {
    changed: bool,
    nulled: Cols,
}

impl Planner<'_, '_> {
    pub(super) fn reduce_outer_joins(&self, p: &mut Plan) {
        self.reduce(p, &Cols::new(), &Cols::new(), &Cols::new());
    }

    /// `nonnull`: the columns of `p`'s row the quals above need non-null; `truth`: those
    /// of them they need TRUE; `forced`: those an `IS NULL` above forces null.
    fn reduce(&self, p: &mut Plan, nonnull: &Cols, forced: &Cols, truth: &Cols) -> Done {
        let none = Cols::new();
        match p {
            Plan::Filter { input, pred } => {
                let mut parts = Vec::new();
                conjuncts(pred.clone(), &mut parts);
                let mut nn = clause_nonnull(&parts);
                nn.extend(nonnull);
                let mut fnull = forced_null_cols(&parts);
                fnull.extend(forced);
                let mut tr: Cols = parts.iter().flat_map(truth_cols).collect();
                tr.extend(truth);
                let d = self.reduce(take(input), &nn, &fnull, &tr);
                if !d.changed {
                    return d;
                }
                // check_redundant_nullability_qual: `x IS NULL` over an anti join's
                // nullable side is true.
                parts.retain(|q| forced_null_col(q).is_none_or(|c| !d.nulled.contains(&c)));
                let inner = std::mem::replace(take(input), Plan::One);
                *p = crate::sql::binder::optimize::add_filter_phv(&self.cat, inner, Expr::And(parts));
                d
            }
            Plan::Project { input, exprs } => {
                // A pulled-up subquery's columns: strict expressions of its row.
                let mut nn = Cols::new();
                let mut tr = Cols::new();
                for c in nonnull {
                    if let Some(e) = exprs.get(*c) {
                        value_cols(e, &mut nn);
                        // (A PlaceHolderVar's contents: as the qual would read them.)
                        if truth.contains(c) {
                            nn.extend(nonnull_cols(e, true));
                            tr.extend(truth_cols(e));
                        }
                    }
                }
                let fnull: Cols = forced
                    .iter()
                    .filter_map(|c| match exprs.get(*c) {
                        Some(Expr::Col(i, _)) => Some(*i),
                        _ => None,
                    })
                    .collect();
                let d = self.reduce(take(input), &nn, &fnull, &tr);
                let nulled = exprs
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| matches!(e, Expr::Col(i, _) if d.nulled.contains(i)))
                    .map(|(o, _)| o)
                    .collect();
                Done {
                    changed: d.changed,
                    nulled,
                }
            }
            Plan::Sort { input, .. } | Plan::LockRows { input, .. } | Plan::Barrier { input, .. } | Plan::Context { input, .. } => {
                self.reduce(take(input), nonnull, forced, truth)
            }
            Plan::Join { .. } => self.reduce_join(p, nonnull, forced, truth),
            Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. } => {
                let d = self.reduce(take(input), &none, &none, &none);
                Done {
                    changed: d.changed,
                    nulled: Cols::new(),
                }
            }
            Plan::With { ctes, body, .. } => {
                for (_, c) in ctes.iter_mut() {
                    self.reduce(take(c), &none, &none, &none);
                }
                let d = self.reduce(take(body), nonnull, forced, truth);
                Done {
                    changed: d.changed,
                    nulled: Cols::new(),
                }
            }
            Plan::SetOp { left, right, .. } => {
                self.reduce(take(left), &none, &none, &none);
                self.reduce(take(right), &none, &none, &none);
                Done::default()
            }
            Plan::Append { inputs, .. } => {
                for i in inputs.iter_mut() {
                    self.reduce(take(i), &none, &none, &none);
                }
                Done::default()
            }
            Plan::Recursive {
                base, recursive, ..
            } => {
                self.reduce(take(base), &none, &none, &none);
                self.reduce(take(recursive), &none, &none, &none);
                Done::default()
            }
            Plan::Insert(i) => {
                self.reduce(take(&mut i.source), &none, &none, &none);
                Done::default()
            }
            Plan::Update(u) => {
                self.reduce(take(&mut u.source), &none, &none, &none);
                Done::default()
            }
            Plan::Delete(d) => {
                self.reduce(take(&mut d.source), &none, &none, &none);
                Done::default()
            }
            Plan::Merge(m) => {
                self.reduce(take(&mut m.source), &none, &none, &none);
                Done::default()
            }
            _ => Done::default(),
        }
    }

    fn reduce_join(&self, p: &mut Plan, nonnull: &Cols, forced: &Cols, truth: &Cols) -> Done {
        let Plan::Join {
            kind,
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
            unreachable!()
        };
        let (nl, nr) = (*nleft, *nright);
        let old = *kind;
        let lcols = |s: &Cols| s.iter().any(|c| *c < nl && nonnull_reaches(left, *c, truth.contains(c)));
        let rcols = |s: &Cols| {
            s.iter()
                .any(|c| (nl..nl + nr).contains(c) && nonnull_reaches(right, c - nl, truth.contains(c)))
        };
        let mut k = old;
        match old {
            JoinKind::Left if rcols(nonnull) => k = JoinKind::Inner,
            JoinKind::Right if lcols(nonnull) => k = JoinKind::Inner,
            JoinKind::Full => {
                k = match (lcols(nonnull), rcols(nonnull)) {
                    (true, true) => JoinKind::Inner,
                    (true, false) => JoinKind::Left,
                    (false, true) => JoinKind::Right,
                    (false, false) => JoinKind::Full,
                }
            }
            _ => {}
        }
        // The join's own clauses: its condition, hash keys, and the ON clauses pushed into
        // its nullable side.
        let mut parts = Vec::new();
        if let Some(c) = cond {
            conjuncts(c.clone(), &mut parts);
        }
        let mut local = clause_nonnull(&parts);
        if let Some((lk, rk)) = hash {
            for e in lk.iter() {
                value_cols(e, &mut local);
            }
            let mut rc = Cols::new();
            rk.iter().for_each(|e| value_cols(e, &mut rc));
            local.extend(rc.into_iter().map(|c| c + nl));
        }
        let mut local_null = forced_null_cols(&parts);
        let local_truth: Cols = parts.iter().flat_map(truth_cols).collect();
        let pushed_nonnull = |side: &Plan, off: usize, pushed: &[usize]| -> Cols {
            let mut fs = Vec::new();
            match side {
                Plan::Filter { pred, .. } => conjuncts(pred.clone(), &mut fs),
                Plan::Scan(s) => {
                    if let Some(f) = &s.filter {
                        conjuncts(f.clone(), &mut fs)
                    }
                }
                _ => {}
            }
            clause_nonnull(&fs)
                .into_iter()
                .map(|c| c + off)
                .filter(|c| pushed.contains(c))
                .collect()
        };
        match old {
            JoinKind::Left => local.extend(pushed_nonnull(right, nl, &meta.pushed)),
            JoinKind::Right => local.extend(pushed_nonnull(left, 0, &meta.pushed)),
            _ => {}
        }
        // A left join whose clauses are strict for a column of its right side an upper
        // `IS NULL` forces null: its matched rows all fail that test.
        let anti = k == JoinKind::Left
            && params.is_empty()
            && local
                .intersection(forced)
                .any(|c| (nl..nl + nr).contains(c));
        // What each side is told (a semi join is inner here; constraints never pass
        // through a full join).
        let (lpass, rpass) = match k {
            JoinKind::Inner | JoinKind::Semi => {
                let mut nn = local.clone();
                nn.extend(nonnull);
                local_null.extend(forced);
                let mut tr = local_truth;
                tr.extend(truth);
                ((nn.clone(), local_null.clone(), tr.clone()), (nn, local_null, tr))
            }
            JoinKind::Left | JoinKind::Anti => (
                (nonnull.clone(), forced.clone(), truth.clone()),
                (local.clone(), local_null, local_truth),
            ),
            JoinKind::Right => (
                (local.clone(), local_null, local_truth),
                (nonnull.clone(), forced.clone(), truth.clone()),
            ),
            _ => (
                (Cols::new(), Cols::new(), Cols::new()),
                (Cols::new(), Cols::new(), Cols::new()),
            ),
        };
        let dl = self.reduce(
            take(left),
            &range(&lpass.0, 0, nl),
            &range(&lpass.1, 0, nl),
            &range(&lpass.2, 0, nl),
        );
        let dr = self.reduce(
            take(right),
            &range(&rpass.0, nl, nl + nr),
            &range(&rpass.1, nl, nl + nr),
            &range(&rpass.2, nl, nl + nr),
        );
        let mut nulled = dl.nulled;
        if !matches!(k, JoinKind::Semi | JoinKind::Anti) {
            nulled.extend(dr.nulled.iter().map(|c| c + nl));
        }
        let mut changed = dl.changed || dr.changed;
        if k != old {
            changed = true;
            // Rebuild it as the new kind: ON clauses on a side that is no longer the
            // preserved one go down into it.
            let l = std::mem::replace(take(left), Plan::One);
            let r = std::mem::replace(take(right), Plan::One);
            let mut m = std::mem::take(meta);
            m.written_kind = m.written_kind.or(Some(old));
            let old_hash = hash.take();
            let mut j = join(&self.cat, k, l, r, cond.take(), params.clone(), nl, nr);
            if let Plan::Join { meta: jm, hash: h, .. } = &mut j {
                m.pushed = std::mem::take(&mut jm.pushed);
                *jm = m;
                if let Some((hl, hr)) = old_hash {
                    match h {
                        Some((l2, r2)) => {
                            l2.extend(hl);
                            r2.extend(hr);
                        }
                        None => *h = Some((hl, hr)),
                    }
                }
            }
            *p = j;
        }
        if anti {
            changed = true;
            let types = super::analyzejoins::col_types(p);
            let Plan::Join { kind, meta, .. } = p else {
                unreachable!()
            };
            *kind = JoinKind::Anti;
            meta.pushed.clear();
            let exprs = (0..nl + nr)
                .map(|i| {
                    let t = types.get(i).cloned().unwrap_or(DataType::Unknown);
                    if i < nl {
                        Expr::Col(i, t)
                    } else {
                        Expr::Const(Value::Null, t)
                    }
                })
                .collect();
            let j = std::mem::replace(p, Plan::One);
            *p = Plan::Project {
                input: Arc::new(j),
                exprs,
            };
            nulled.extend(nl..nl + nr);
        }
        Done { changed, nulled }
    }
}

/// A FROM-less one-row relation's values: a pulled-up select without FROM (an RTE_RESULT,
/// its projections composed), or a left join on true of such relations (which
/// remove_useless_results_recurse reduces to its left side, the right side's values
/// evaluated there). None for anything else.
fn result_values(p: &Plan) -> Option<Vec<Expr>> {
    let ok = |e: &Expr| {
        let mut ok = !e.is_volatile() && !e.has_outer_refs();
        e.walk(&mut |x| {
            ok &= !matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Param(..))
        });
        ok
    };
    match p {
        Plan::One => Some(Vec::new()),
        Plan::Project { input, exprs } => {
            let below = result_values(input)?;
            let vals: Vec<Expr> = exprs.iter().map(|e| subst_cols(e, &below)).collect();
            vals.iter().all(ok).then_some(vals)
        }
        // (A subquery level kept for itself: its row.)
        Plan::Barrier { input, secure: false, .. } => result_values(input),
        // (Of two such relations, on true, after reduce_outer_joins: one row whichever side
        // it preserves; a full join stays a join.)
        Plan::Join {
            kind: JoinKind::Inner | JoinKind::Left | JoinKind::Right,
            left,
            right,
            cond: None,
            hash: None,
            params,
            ..
        } if params.is_empty() => {
            let mut vals = result_values(left)?;
            vals.extend(result_values(right)?);
            Some(vals)
        }
        _ => None,
    }
}

/// `e` over a row whose columns are `vals`.
fn subst_cols(e: &Expr, vals: &[Expr]) -> Expr {
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(c, t) => vals.get(c).cloned().unwrap_or(Expr::Col(c, t)),
                x => x,
            })
        })
        .unwrap()
}

impl Planner<'_, '_> {
    /// remove_useless_results_recurse (prepjointree.c) for a semi join, after
    /// reduce_outer_joins: one whose right side is a FROM-less relation (`result_values`)
    /// returns each left row or not, so it is its left side under the join's clauses, the
    /// right side's values (PlaceHolderVars) evaluated there (remove_result_refs).
    pub(super) fn remove_result_semijoins(&self, p: &mut Plan) {
        for c in crate::sql::explain::plan_inputs_mut(p) {
            self.remove_result_semijoins(take(c));
        }
        let Plan::Join {
            kind: JoinKind::Semi,
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            meta,
            ..
        } = &*p
        else {
            return;
        };
        if !params.is_empty() || meta.from_call.is_some() {
            return;
        }
        let Some(vals) = result_values(right) else { return };
        let nl = *nleft;
        let mut parts = Vec::new();
        if let Some(c) = cond {
            conjuncts(c.clone(), &mut parts);
        }
        // (Its hash keys: the join's clauses again.)
        if let Some((lk, rk)) = hash {
            for (i, (a, b)) in lk.iter().zip(rk).enumerate() {
                let (a, bad, coll) = crate::sql::binder::hash_key_operand(a);
                let (b, _, _) = crate::sql::binder::hash_key_operand(b);
                let b = b.shift_cols(nl as isize);
                let flip = meta.key_flipped.get(i) == Some(&true) || meta.hash_swapped.get(i) == Some(&true);
                let (a, b) = if flip { (b, a) } else { (a, b) };
                let Ok(r) = crate::sql::func::resolve_binop("=", &a.ty(), &b.ty()) else { return };
                let e = crate::sql::binder::eq_under(Expr::Call(r.def, vec![a, b], DataType::Bool), coll);
                parts.push(if bad { crate::sql::binder::indeterminate_eq(e) } else { e });
            }
        }
        let parts: Vec<Expr> = parts
            .into_iter()
            .map(|q| {
                q.transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(c, t) if c >= nl => vals.get(c - nl).cloned().unwrap_or(Expr::Col(c, t)),
                        x => x,
                    })
                })
                .unwrap()
            })
            .collect();
        let l = (**left).clone();
        *p = if parts.is_empty() {
            l
        } else if let Some(lv) = result_values(&l) {
            // (A FROM-less left side too: its quals, over its values, are the Result's
            // One-Time Filter (create_resultscan_plan), not folded.)
            let pred = subst_cols(&Expr::And(parts), &lv);
            Plan::Filter { input: Arc::new(l), pred }
        } else {
            crate::sql::binder::optimize::add_filter(&self.cat, l, Expr::And(parts))
        };
    }
}
