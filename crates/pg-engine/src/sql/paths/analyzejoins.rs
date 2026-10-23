//! Join removal (analyzejoins.c's remove_useless_joins): a left join whose inner side
//! can match at most one row per outer row, and whose columns nothing above the join
//! reads, returns just its outer rows: the join is the outer side alone.

use super::*;
use crate::sql::binder::plan_width;

/// An expression with the casts that don't change equality stripped (RelabelType and the
/// integer / float widenings a cross-type `=` reads through).
fn bare(e: &Expr) -> &Expr {
    match e {
        Expr::Relabel(x, _) | Expr::Collate(x, _, _) => bare(x),
        Expr::Cast(x, from, to, false)
            if matches!(
                (from, to),
                (DataType::Int2, DataType::Int4 | DataType::Int8)
                    | (DataType::Int4, DataType::Int8)
                    | (DataType::Float4, DataType::Float8)
                    | (
                        DataType::Varchar(_) | DataType::Name | DataType::Text,
                        DataType::Text | DataType::Varchar(_)
                    )
            ) =>
        {
            bare(x)
        }
        e => e,
    }
}

/// The types of a plan's columns, as far as the plan says (else Unknown).
pub(crate) fn col_types(p: &Plan) -> Vec<DataType> {
    let w = plan_width(p);
    let mut out = match p {
        Plan::Scan(s) => s.table.columns.iter().map(|c| c.ty.clone()).collect(),
        Plan::Values { rows, .. } => rows
            .first()
            .map(|r| r.iter().map(Expr::ty).collect())
            .unwrap_or_default(),
        Plan::Project { exprs, .. } => exprs.iter().map(Expr::ty).collect(),
        Plan::Agg { groups, aggs, .. } => groups
            .iter()
            .map(Expr::ty)
            .chain(aggs.iter().map(|a| a.ty.clone()))
            .collect(),
        Plan::Join {
            kind, left, right, ..
        } => {
            let mut v = col_types(left);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                v.extend(col_types(right));
            }
            v
        }
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => col_types(input),
        _ => Vec::new(),
    };
    out.resize(w, DataType::Unknown);
    out
}

/// `a = b` usable to prove uniqueness (a mergejoinable equality): its two sides.
fn equality(e: &Expr) -> Option<(&Expr, &Expr)> {
    match e {
        Expr::Call(d, args, _)
            if d.name == "="
                && args.len() == 2
                && !e.is_volatile()
                && !d.coll.as_ref().is_some_and(|c| !c.deterministic) =>
        {
            Some((&args[0], &args[1]))
        }
        _ => None,
    }
}

/// A candidate for proving uniqueness: an expression an equality fixes (over the
/// relation's row), and the restriction clause it comes from when it is one (`col =
/// constant`), not a join clause.
type Cand = (Expr, Option<Expr>);

/// The `col = constant` restrictions among `pred`'s conjuncts.
fn restrictions(pred: &Expr, out: &mut Vec<Cand>) {
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    for q in parts {
        if let Some((a, b)) = equality(&q) {
            for (x, y) in [(a, b), (b, a)] {
                if !x.columns().is_empty() && y.columns().is_empty() && pseudo_constant(y) {
                    out.push((x.clone(), Some(q.clone())));
                    break;
                }
            }
        }
    }
}

/// relation_has_unique_index_ext: the first unique index of `t` (in get_relation_info's
/// order: newest first) whose every key `cands` fix: the restriction clauses that
/// matched its columns. Partial and deferrable indexes don't count.
fn unique_index_ext(ctx: &ExecCtx, t: &Arc<Table>, cands: &[Cand]) -> Option<Vec<Expr>> {
    if cands.is_empty() {
        return None;
    }
    let mut bound = crate::sql::binder::bound_indexes(t, ctx).ok()?;
    bound.sort_by_key(|b| std::cmp::Reverse(b.index.oid));
    'ix: for b in &bound {
        let ix = &b.index;
        if !(ix.unique || ix.primary)
            || !ix.valid
            || b.predicate.is_some()
            || b.constraint.as_ref().is_some_and(|c| c.deferral.0)
        {
            continue;
        }
        let mut extra = Vec::new();
        for k in b.keys.iter().take(ix.columns.len()) {
            match cands.iter().find(|(e, _)| bare(e).same_as(bare(k))) {
                Some((_, Some(c))) => extra.push(c.clone()),
                Some(_) => {}
                None => continue 'ix,
            }
        }
        return Some(extra);
    }
    None
}

/// Whether `p` has at most one row per value of `keys` (over its row): rel_is_distinct_for.
/// `base`: `p` is still a base relation (a pulled-up subquery's projections and filters
/// over a table); a subquery that isn't (it has a LIMIT, an ORDER BY, ...) is distinct
/// only by its DISTINCT, GROUP BY or set operation (query_is_distinct_for).
fn distinct_for(ctx: &ExecCtx, p: &Plan, keys: &[Cand], base: bool) -> bool {
    let covers = |n: usize| {
        (0..n).all(|c| keys.iter().any(|(k, _)| matches!(bare(k), Expr::Col(j, _) if *j == c)))
    };
    match p {
        Plan::Barrier { input, .. } | Plan::Context { input, .. } => distinct_for(ctx, input, keys, base),
        Plan::Project { input, exprs } => {
            if exprs.iter().any(|e| matches!(e, Expr::Srf(..))) {
                return false;
            }
            let subst = |k: &Expr| {
                k.clone()
                    .transform(&mut |e| {
                        Ok(match e {
                            Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            e => e,
                        })
                    })
                    .unwrap()
            };
            let mapped: Vec<Cand> = keys
                .iter()
                .map(|(k, c)| (subst(k), c.as_ref().map(subst)))
                .collect();
            distinct_for(ctx, input, &mapped, base)
        }
        Plan::Filter { input, pred } => {
            let mut keys = keys.to_vec();
            restrictions(pred, &mut keys);
            distinct_for(ctx, input, &keys, base)
        }
        Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
            distinct_for(ctx, input, keys, false)
        }
        Plan::Scan(s) => {
            if !base || s.sample.is_some() {
                return false;
            }
            let mut keys = keys.to_vec();
            if let Some(f) = &s.filter {
                restrictions(f, &mut keys);
            }
            unique_index_ext(ctx, &s.table, &keys).is_some()
        }
        // A partitioned table: its own unique indexes (its columns are the Append's).
        Plan::Append {
            inputs,
            inh: Some(_),
            ..
        } if base => {
            let parent = inputs.iter().find_map(|i| {
                let mut q: &Plan = i;
                loop {
                    match q {
                        Plan::Project { input, .. } | Plan::Filter { input, .. } => q = input,
                        Plan::Scan(s) => {
                            return s.part.as_ref().and_then(|l| l.last()).map(|l| l.parent.clone());
                        }
                        _ => return None,
                    }
                }
            });
            let Some(parent) = parent else {
                return false;
            };
            if parent.columns.len() != plan_width(p) {
                return false;
            }
            unique_index_ext(ctx, &parent, keys).is_some()
        }
        Plan::Agg {
            groups,
            sets: None,
            ..
        } => groups.is_empty() || covers(groups.len()),
        Plan::Distinct { input, on: None, .. } => covers(plan_width(input)),
        Plan::Distinct {
            on: Some(on), ..
        } => on
            .iter()
            .all(|e| keys.iter().any(|(k, _)| bare(k).same_as(bare(e)))),
        Plan::SetOp {
            all: false, left, ..
        } => covers(plan_width(left)),
        _ => false,
    }
}

impl Planner<'_, '_> {
    /// remove_useless_joins over the plan (whose consumers need `needed`), until no more
    /// go (a removed join's clauses no longer need the columns of the joins below it).
    pub(super) fn remove_useless_joins(&self, p: &mut Plan, needed: &[bool]) {
        for _ in 0..16 {
            if !self.remove_joins(p, needed) {
                break;
            }
        }
    }

    /// One pass: inner joins first, so an outer join over them sees what is left. Whether
    /// a join went.
    fn remove_joins(&self, p: &mut Plan, needed: &[bool]) -> bool {
        let changed = std::cell::Cell::new(false);
        let needs = child_needs(p, needed);
        let mut needs = needs.into_iter();
        let mut visit = |c: &mut Arc<Plan>| {
            let n = needs.next().unwrap_or_else(|| vec![true; plan_width(c)]);
            if self.remove_joins(take(c), &n) {
                changed.set(true);
            }
        };
        match p {
            Plan::With { ctes, body, .. } => {
                for (_, c) in ctes.iter_mut() {
                    let cw = plan_width(c);
                    if self.remove_joins(take(c), &vec![true; cw]) {
                        changed.set(true);
                    }
                }
                visit(body);
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
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => visit(input),
            Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
                visit(left);
                visit(right);
            }
            Plan::Append { inputs, .. } => {
                for i in inputs.iter_mut() {
                    visit(i);
                }
            }
            Plan::Recursive {
                base, recursive, ..
            } => {
                visit(base);
                visit(recursive);
            }
            Plan::Insert(i) => visit(&mut i.source),
            Plan::Update(u) => visit(&mut u.source),
            Plan::Delete(d) => visit(&mut d.source),
            Plan::Merge(m) => visit(&mut m.source),
            _ => {}
        }
        if let Some(out) = self.removed_join(p, needed) {
            *p = out;
            self.removed.set(self.removed.get() + 1);
            changed.set(true);
        }
        changed.get()
    }

    /// join_is_removable: the join at `p` as its outer side alone (the inner side's
    /// columns NULL), when it is a left join nothing above reads the inner side of and the
    /// inner side has at most one match per outer row.
    fn removed_join(&self, p: &Plan, needed: &[bool]) -> Option<Plan> {
        let Plan::Join {
            kind,
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            ..
        } = p
        else {
            return None;
        };
        let outer_left = match kind {
            JoinKind::Left => true,
            JoinKind::Right => false,
            _ => return None,
        };
        if !params.is_empty() {
            return None;
        }
        let nl = *nleft;
        let width = plan_width(p);
        let (outer, inner) = if outer_left { (left, right) } else { (right, left) };
        let inner_cols = if outer_left { nl..width } else { 0..nl };
        if needed
            .iter()
            .enumerate()
            .any(|(i, &x)| x && inner_cols.contains(&i))
        {
            return None;
        }
        if crate::sql::explain::plan_volatile(inner) {
            return None;
        }
        let shift = -(inner_cols.start as isize);
        let in_inner = |e: &Expr| {
            let cs = e.columns();
            !cs.is_empty() && cs.iter().all(|c| inner_cols.contains(c))
        };
        let in_outer = |e: &Expr| e.columns().iter().all(|c| !inner_cols.contains(c));
        // The inner side's expressions the join clauses equate to outer ones (or to
        // constants).
        let mut keys: Vec<Cand> = Vec::new();
        if let Some(c) = cond {
            let mut parts = Vec::new();
            conjuncts(c.clone(), &mut parts);
            for q in &parts {
                if let Some((a, b)) = equality(q) {
                    for (x, y) in [(a, b), (b, a)] {
                        if in_inner(x) && in_outer(y) && !y.is_volatile() {
                            keys.push((x.clone().shift_cols(shift), None));
                        }
                    }
                }
            }
        }
        if let Some((lk, rk)) = hash {
            for (a, b) in lk.iter().zip(rk) {
                let k = if outer_left { b } else { a };
                if matches!(k, Expr::Call(d, ..) if d.name == crate::sql::binder::INDETERMINATE_HASH)
                {
                    continue;
                }
                keys.push((k.clone(), None));
            }
        }
        if !distinct_for(self.ctx, inner, &keys, true) {
            return None;
        }
        let types = col_types(p);
        let ty = |i: usize| types.get(i).cloned().unwrap_or(DataType::Unknown);
        let exprs = (0..width)
            .map(|i| {
                if inner_cols.contains(&i) {
                    Expr::Const(Value::Null, ty(i))
                } else if outer_left {
                    Expr::Col(i, ty(i))
                } else {
                    Expr::Col(i - nl, ty(i))
                }
            })
            .collect();
        Some(Plan::Project {
            input: outer.clone(),
            exprs,
        })
    }
}

/// The same qual, or the same equality commuted.
fn same_qual(a: &Expr, b: &Expr) -> bool {
    if a.same_as(b) {
        return true;
    }
    match (a, b) {
        (Expr::Call(d1, x, _), Expr::Call(d2, y, _))
            if d1.name == "=" && d2.name == "=" && x.len() == 2 && y.len() == 2 =>
        {
            x[0].same_as(&y[1]) && x[1].same_as(&y[0])
        }
        _ => false,
    }
}

/// `constant = column` as `column = constant` (an equivalence class's generated clause),
/// when the operator takes its operands either way round.
fn var_first(e: &Expr) -> Option<Expr> {
    let Expr::Call(d, args, t) = e else { return None };
    if d.name != "=" || args.len() != 2 {
        return None;
    }
    let colish = |x: &Expr| !x.columns().is_empty();
    if !colish(&args[0])
        && colish(&args[1])
        && args[0].ty().without_typmod() == args[1].ty().without_typmod()
    {
        return Some(Expr::Call(
            d.clone(),
            vec![args[1].clone(), args[0].clone()],
            t.clone(),
        ));
    }
    None
}

/// replace_selfjoin_qual: `X = X` (a strict btree equality, X not volatile) is
/// `X IS NOT NULL`.
fn replace_selfjoin_qual(q: Expr) -> Expr {
    if let Some((a, b)) = equality(&q)
        && a.same_as(b)
        && !a.is_volatile()
    {
        return Expr::IsNull(Box::new(a.clone()), true);
    }
    q
}

impl Planner<'_, '_> {
    /// remove_useless_self_joins over a join group's leaves: of two scans of one table
    /// equated on a unique key (or fixed by the same constants), the earlier goes and its
    /// quals move to the later one, its columns read from there (`alias`). A semi join
    /// whose sides are such a pair becomes the one scan.
    /// With outer joins in the group, `sides` are their (left, right) items (the two
    /// scans must be on the same side of each) and `meta` goes along with `clauses`;
    /// returns the merges made, (removed, kept), in order.
    pub(super) fn self_joins(
        &self,
        kind: &mut JoinKind,
        leaves: &mut Vec<(Plan, usize)>,
        clauses: &mut Vec<(Expr, bool)>,
        alias: &mut [usize],
        sides: &mut [(u32, u32)],
        meta: &mut Vec<(Option<usize>, u32, u32)>,
    ) -> Vec<(usize, usize)> {
        let mut merges = Vec::new();
        let on = {
            let s = self.ctx.sess.settings.borrow();
            s.get_raw("enable_self_join_elimination")
                .is_none_or(|v| v != "off" && v != "false")
        };
        if !on
            || !(matches!(*kind, JoinKind::Inner)
                || (*kind == JoinKind::Semi && leaves.len() == 2))
        {
            return merges;
        }
        let mut r = 0;
        while r < leaves.len() {
            let same_side = |k: usize| {
                sides.iter().all(|(l, rr)| {
                    (l >> r & 1 == l >> k & 1) && (rr >> r & 1 == rr >> k & 1)
                })
            };
            let found = (r + 1..leaves.len())
                .find(|&k| same_side(k) && self.self_join_pair(leaves, clauses, r, k));
            match found {
                Some(k) => {
                    self.merge_self_join(leaves, clauses, alias, r, k, meta);
                    let remap = |m: u32| {
                        let m = if m >> r & 1 == 1 { (m | 1 << k) & !(1 << r) } else { m };
                        (m & ((1u32 << r) - 1)) | ((m >> (r + 1)) << r)
                    };
                    for (l, rr) in sides.iter_mut() {
                        *l = remap(*l);
                        *rr = remap(*rr);
                    }
                    for m in meta.iter_mut() {
                        m.1 = remap(m.1);
                        m.2 = remap(m.2);
                    }
                    merges.push((r, k));
                    self.removed.set(self.removed.get() + 1);
                    if *kind == JoinKind::Semi {
                        *kind = JoinKind::Inner;
                        for c in clauses.iter_mut() {
                            c.1 = false;
                        }
                    }
                }
                None => r += 1,
            }
        }
        merges
    }

    /// The scans of leaves `r` and `k` read one table and the join can be proven to pair
    /// each row with itself (remove_self_joins_one_group's test).
    fn self_join_pair(&self, leaves: &[(Plan, usize)], clauses: &[(Expr, bool)], r: usize, k: usize) -> bool {
        let (Plan::Scan(rs), Plan::Scan(ks)) = (&leaves[r].0, &leaves[k].0) else {
            return false;
        };
        let plain = |s: &ScanPlan| {
            s.sample.is_none() && s.current_of.is_none() && s.part.is_none() && !s.with_key && !s.sys
        };
        if rs.table.oid != ks.table.oid
            || !plain(rs)
            || !plain(ks)
            || rs.mark.map(|m| m.0) != ks.mark.map(|m| m.0)
        {
            return false;
        }
        let (ro, ko) = (leaves[r].1, leaves[k].1);
        let n = rs.table.columns.len();
        let in_r = |e: &Expr| {
            let cs = e.columns();
            !cs.is_empty() && cs.iter().all(|&c| c >= ro && c < ro + n)
        };
        let in_k = |e: &Expr| {
            let cs = e.columns();
            !cs.is_empty() && cs.iter().all(|&c| c >= ko && c < ko + n)
        };
        let filter = |s: &ScanPlan, off: usize| {
            let mut v = Vec::new();
            if let Some(f) = &s.filter {
                conjuncts(f.clone(), &mut v);
            }
            v.into_iter()
                .map(|q| q.shift_cols(off as isize))
                .collect::<Vec<_>>()
        };
        // Equivalence classes (their members in the order they came in) of the
        // equalities: the removed scan's own, the join's, the kept scan's own.
        let mut ecs: Vec<Vec<Expr>> = Vec::new();
        let quals = filter(rs, ro)
            .into_iter()
            .chain(clauses.iter().map(|(e, _)| e.clone()))
            .chain(filter(ks, ko));
        for q in quals {
            let Some((a, b)) = equality(&q) else { continue };
            if a.columns().is_empty() || b.columns().is_empty() {
                continue;
            }
            let (a, b) = (bare(a).clone(), bare(b).clone());
            let pos = |e: &Expr, ecs: &[Vec<Expr>]| ecs.iter().position(|m| m.iter().any(|x| x.same_as(e)));
            match (pos(&a, &ecs), pos(&b, &ecs)) {
                (Some(i), Some(j)) if i != j => {
                    let (lo, hi) = (i.min(j), i.max(j));
                    let moved = ecs.remove(hi);
                    ecs[lo].extend(moved);
                }
                (Some(_), Some(_)) => {}
                (Some(i), None) => ecs[i].push(b),
                (None, Some(j)) => ecs[j].push(a),
                (None, None) => ecs.push(vec![a, b]),
            }
        }
        // generate_join_implied_equalities: per class over both scans, its first member
        // of each; the self-join ones equate an expression with itself.
        let mut cands: Vec<Cand> = Vec::new();
        let mut joined = false;
        for m in &ecs {
            let (Some(x), Some(y)) = (m.iter().find(|e| in_r(e)), m.iter().find(|e| in_k(e))) else {
                continue;
            };
            joined = true;
            let x = x.clone().shift_cols(-(ro as isize));
            let y = y.clone().shift_cols(-(ko as isize));
            if x.same_as(&y) {
                cands.push((y, None));
            }
        }
        if !joined {
            return false;
        }
        // The kept scan's restrictions count as well, when the removed one has them too.
        let (rq, kq) = (filter(rs, 0), filter(ks, 0));
        for q in &kq {
            restrictions(q, &mut cands);
        }
        let Some(extra) = unique_index_ext(self.ctx, &ks.table, &cands) else {
            return false;
        };
        extra.iter().all(|q| rq.iter().any(|x| same_qual(x, q)))
    }

    /// Merge leaf `r` into leaf `k` (remove_self_join_rel): `r`'s quals and the clauses
    /// now over `k` alone go to `k`'s scan, every reference to `r` reads `k`.
    fn merge_self_join(
        &self,
        leaves: &mut Vec<(Plan, usize)>,
        clauses: &mut Vec<(Expr, bool)>,
        alias: &mut [usize],
        r: usize,
        k: usize,
        meta: &mut Vec<(Option<usize>, u32, u32)>,
    ) {
        let (ro, ko) = (leaves[r].1, leaves[k].1);
        let old_meta = std::mem::take(meta);
        let n = plan_width(&leaves[r].0);
        let moved = |g: usize| if g >= ro && g < ro + n { g - ro + ko } else { g };
        for a in alias.iter_mut() {
            *a = moved(*a);
        }
        let mut quals: Vec<Expr> = Vec::new();
        let Plan::Scan(rs) = &leaves[r].0 else { unreachable!() };
        if let Some(f) = &rs.filter {
            conjuncts(f.clone(), &mut quals);
        }
        let mut kept = Vec::new();
        for (ci, (e, on)) in std::mem::take(clauses).into_iter().enumerate() {
            let m = old_meta.get(ci).copied();
            let e = e
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(g, t) => Expr::Col(moved(g), t),
                        x => x,
                    })
                })
                .unwrap();
            let cs = e.columns();
            if !cs.is_empty()
                && cs.iter().all(|&c| c >= ko && c < ko + n)
                && !e.is_volatile()
                && m.is_none_or(|m| m.0.is_none())
                && (m.is_some() || !on)
            {
                let q = replace_selfjoin_qual(e.shift_cols(-(ko as isize)));
                // (`col IS NOT NULL` of a NOT NULL column always holds.)
                if let Expr::IsNull(x, true) = &q
                    && let Expr::Col(c, _) = &**x
                    && let Plan::Scan(ks) = &leaves[k].0
                    && ks.table.columns.get(*c).is_some_and(|c| c.not_null)
                {
                    continue;
                }
                quals.push(q);
            } else {
                kept.push((e, on));
                meta.extend(m);
            }
        }
        *clauses = kept;
        leaves.remove(r);
        let k = if k > r { k - 1 } else { k };
        let Plan::Scan(ks) = &mut leaves[k].0 else { unreachable!() };
        let mut all = Vec::new();
        if let Some(f) = ks.filter.take() {
            conjuncts(f, &mut all);
        }
        for q in quals {
            match all.iter().position(|x| same_qual(x, &q)) {
                // (Two clauses equating a column with one constant: their class gives
                // back one `column = constant`.)
                Some(i) => {
                    if let Some(v) = var_first(&all[i]) {
                        all[i] = v;
                    }
                }
                None => all.push(q),
            }
        }
        // (Not add_filter: the equivalence classes were done with; quals that now
        // contradict each other stay quals.)
        crate::sql::binder::optimize::order_quals(&mut all);
        ks.filter = match all.len() {
            0 => None,
            1 => all.pop(),
            _ => Some(Expr::And(all)),
        };
        ks.access = Access::Unplanned;
        ks.index = None;
    }

    /// A join group self-join elimination left one leaf: that leaf planned alone, under the
    /// projection to the group's columns.
    pub(super) fn single_leaf(
        &self,
        p: &mut Plan,
        mut leaves: Vec<(Plan, usize)>,
        clauses: Vec<(Expr, bool)>,
        needed: &[bool],
        alias: &[usize],
        width: usize,
    ) -> Option<bool> {
        let (mut plan, off) = leaves.pop()?;
        let n = plan_width(&plan);
        let local: Vec<Expr> = clauses
            .into_iter()
            .map(|(e, _)| e.shift_cols(-(off as isize)))
            .collect();
        if !local.is_empty() {
            plan = crate::sql::binder::optimize::add_filter(&self.cat, plan, Expr::And(local));
        }
        let lneed: Vec<bool> = (0..n).map(|c| needed.get(off + c).copied().unwrap_or(false)).collect();
        self.walk(&mut plan, &lneed, None);
        let types = col_types(&plan);
        let exprs = (0..width)
            .map(|g| {
                let c = alias[g] - off;
                Expr::Col(c, types.get(c).cloned().unwrap_or(DataType::Unknown))
            })
            .collect();
        *p = Plan::Project {
            input: Arc::new(plan),
            exprs,
        };
        Some(false)
    }
}

/// The bare names of a plan's columns (as EXPLAIN would show them unqualified), when they
/// are plain columns.
fn plain_names(p: &Plan) -> Option<Vec<String>> {
    plain_cols(p).map(|v| v.into_iter().map(|(n, _)| n).collect())
}

/// `plain_names`, with the relation each column is read from.
fn plain_cols(p: &Plan) -> Option<Vec<(String, Option<String>)>> {
    match p {
        Plan::Scan(s) => {
            let rel = crate::sql::binder::optimize::scan_refname(s);
            Some(
                crate::sql::binder::optimize::scan_names(s)
                    .into_iter()
                    .map(|n| (n, Some(rel.clone())))
                    .collect(),
            )
        }
        Plan::Project { input, exprs } => {
            let inner = plain_cols(input)?;
            exprs
                .iter()
                .map(|e| match e {
                    Expr::Col(j, _) => inner.get(*j).cloned(),
                    _ => None,
                })
                .collect()
        }
        // (A CTE's columns by its reference's name.)
        Plan::CteScan { alias, cols, ncols, .. } if cols.len() == *ncols => Some(
            cols.iter()
                .map(|c| (crate::catalog::quote_ident(c), Some(alias.clone())))
                .collect(),
        ),
        Plan::Join {
            kind, left, right, ..
        } => {
            let mut v = plain_cols(left)?;
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                v.extend(plain_cols(right)?);
            }
            Some(v)
        }
        // (A subquery's columns by its alias.)
        Plan::Barrier { input, alias, cols, .. } => {
            if cols.len() == crate::sql::binder::plan_width(input) {
                Some(cols.iter().map(|c| (crate::catalog::quote_ident(c), Some(alias.clone()))).collect())
            } else {
                plain_cols(input)
            }
        }
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Context { input, .. } => plain_cols(input),
        // (Its members' columns are the parent's.)
        Plan::Append {
            inputs,
            inh: Some(n),
            ..
        } => inputs
            .first()
            .and_then(|i| plain_cols(i))
            .map(|v| v.into_iter().map(|(c, _)| (c, Some(n.clone()))).collect()),
        Plan::Empty { cols: Some(c), info, .. } => Some(
            c.iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), info.as_ref().and_then(|d| d.rels.get(i).cloned().flatten())))
                .collect(),
        ),
        _ => None,
    }
}

/// A plan known to return nothing (a dummy rel): a Result with One-Time Filter false
/// standing for it.
fn dummy(p: &Plan) -> Plan {
    // (A projection keeps its target list over the empty input: the dummy rel's Result
    // shows it.)
    if let Plan::Project { input, exprs } = p
        && !matches!(&**input, Plan::Empty { .. })
        && exprs.iter().any(|e| !matches!(e, Expr::Col(..)))
    {
        return Plan::Project {
            input: Arc::new(dummy(input)),
            exprs: exprs.clone(),
        };
    }
    // (A join's sides: their projections' expressions over one empty relation of all
    // their columns, as the dummy join rel's Result shows them.)
    if let Plan::Join { kind, .. } = p
        && !matches!(kind, JoinKind::Semi | JoinKind::Anti)
    {
        let (names, exprs) = dummy_outputs(p);
        if exprs.iter().any(|e| !matches!(e, Expr::Col(..))) {
            let ncols = names.len();
            let cols = names.into_iter().collect::<Option<Vec<_>>>().map(Arc::new);
            return Plan::Project {
                input: Arc::new(Plan::Empty { ncols, cols, info: None }),
                exprs,
            };
        }
    }
    let ncols = plan_width(p);
    let named = plain_cols(p).filter(|c| c.len() == ncols);
    let rels = named.as_ref().map_or_else(Vec::new, |v| v.iter().map(|(_, r)| r.clone()).collect());
    let cols = named.map(|v| Arc::new(v.into_iter().map(|(n, _)| n).collect()));
    // (The classes its quals made stay.)
    let ec = super::column_classes(p);
    let mut unplanned = Vec::new();
    unplanned_subqueries(p, &mut unplanned);
    Plan::Empty { ncols, cols, info: Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec, table: None, unplanned })) }
}

/// The FROM subqueries (not pulled up) a plan proven empty reads, never planned: each with
/// the count of the relations its query reads.
fn unplanned_subqueries(p: &Plan, out: &mut Vec<(String, usize)>) {
    fn relations(p: &Plan) -> usize {
        // (One proven empty already: the relations it was read from.)
        if let Plan::Empty { info: Some(d), .. } = p {
            let mut rels: Vec<&String> = d.rels.iter().flatten().collect();
            rels.sort();
            rels.dedup();
            return rels
                .iter()
                .map(|r| d.unplanned.iter().find(|u| u.0 == **r).map_or(1, |u| u.1))
                .sum();
        }
        let own = matches!(p, Plan::Scan(_) | Plan::SysScan { .. }) as usize;
        own + crate::sql::explain::plan_inputs(p).iter().map(|c| relations(c)).sum::<usize>()
    }
    match p {
        Plan::Barrier { input, alias, .. } if !alias.is_empty() => out.push((alias.clone(), relations(input))),
        Plan::Filter { input, .. } | Plan::Project { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
            unplanned_subqueries(input, out)
        }
        Plan::Join { left, right, .. } => {
            unplanned_subqueries(left, out);
            unplanned_subqueries(right, out);
        }
        _ => {}
    }
}

/// A plan's outputs as expressions over the relations below its projections and joins
/// (each column's name, when a plain column has one).
fn dummy_outputs(p: &Plan) -> (Vec<Option<String>>, Vec<Expr>) {
    match p {
        Plan::Project { input, exprs } => {
            let (names, inner) = dummy_outputs(input);
            let exprs = exprs.iter().map(|e| super::joinpath::lift_subst(e, &inner)).collect();
            (names, exprs)
        }
        Plan::Filter { input, .. } | Plan::Sort { input, .. } => dummy_outputs(input),
        Plan::Join { kind, left, right, .. } if !matches!(kind, JoinKind::Semi | JoinKind::Anti) => {
            let (mut names, mut exprs) = dummy_outputs(left);
            let (rn, re) = dummy_outputs(right);
            let off = names.len() as isize;
            names.extend(rn);
            exprs.extend(re.into_iter().map(|e| e.shift_cols(off)));
            (names, exprs)
        }
        _ => {
            let types = col_types(p);
            let names = plain_names(p).filter(|c| c.len() == types.len());
            (
                (0..types.len()).map(|i| names.as_ref().map(|n| n[i].clone())).collect(),
                types.into_iter().enumerate().map(|(i, t)| Expr::Col(i, t)).collect(),
            )
        }
    }
}

/// A relation made empty by a constant-false qual of its own (a pulled-up subquery's
/// `WHERE false`), which Postgres places at the outer join above as its Join Filter.
fn pseudo_false(p: &Plan) -> bool {
    match p {
        Plan::Filter { input, pred } => constant_false(&Some(pred.clone())) || pseudo_false(input),
        Plan::Project { input, .. } | Plan::Sort { input, .. } => pseudo_false(input),
        Plan::Join { kind: JoinKind::Inner, left, right, .. } => pseudo_false(left) || pseudo_false(right),
        Plan::Join { kind: JoinKind::Left, left, .. } => pseudo_false(left),
        // (A FROM-less query's `WHERE false`, folded already.)
        Plan::Empty { ncols: 0, .. } => true,
        _ => false,
    }
}

/// A join or filter qual that is constant false or NULL (restriction_is_constant_false).
fn constant_false(e: &Option<Expr>) -> bool {
    let Some(e) = e else { return false };
    let mut parts = Vec::new();
    conjuncts(e.clone(), &mut parts);
    parts
        .iter()
        .any(|q| matches!(q, Expr::Const(v, _) if v.is_null() || *v == Value::Bool(false)))
}

impl Planner<'_, '_> {
    /// Joins proven empty (populate_joinrel_with_paths' dummy-rel rules), bottom-up: an
    /// inner or semi join with an empty side or a constant-false condition is empty, an
    /// outer join with an empty outer side is, and an outer join's constant-false
    /// condition empties its nullable side.
    pub(super) fn reduce_dummies(&self, p: &mut Plan) {
        let right_false = matches!(p, Plan::Join { kind: JoinKind::Left, right, params, .. } if params.is_empty() && pseudo_false(right));
        let kids: Vec<&mut Arc<Plan>> = match p {
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Agg { input, .. }
            | Plan::Window { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::Barrier { input, .. } | Plan::Context { input, .. } => vec![input],
            Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => vec![left, right],
            Plan::Append { inputs, .. } => inputs.iter_mut().collect(),
            Plan::With { ctes, body, .. } => {
                let mut v: Vec<&mut Arc<Plan>> = ctes.iter_mut().map(|(_, c)| c).collect();
                v.push(body);
                v
            }
            Plan::Insert(i) => vec![&mut i.source],
            Plan::Update(u) => vec![&mut u.source],
            Plan::Delete(d) => vec![&mut d.source],
            Plan::Merge(m) => vec![&mut m.source],
            _ => Vec::new(),
        };
        for k in kids {
            self.reduce_dummies(take(k));
        }
        // (A relation proven empty: through the projection of a subquery's select list,
        // and a filter or sort over it.)
        fn empty(x: &Plan) -> bool {
            match x {
                Plan::Empty { .. } => true,
                Plan::Project { input, .. } => empty(input),
                Plan::Filter { input, .. } | Plan::Sort { input, .. } => empty(input),
                // (A subquery whose final rel is dummy is (set_subquery_pathlist), a
                // set-returning projection over it too (is_dummy_rel), an OFFSET 0 no node.)
                Plan::Barrier { input, .. } | Plan::ProjectSet { input, .. } => empty(input),
                Plan::Limit { input, limit: None, offset, .. }
                    if offset.as_ref().is_none_or(|o| matches!(o, Expr::Const(Value::Int8(0), _))) =>
                {
                    empty(input)
                }
                _ => false,
            }
        }
        let replace = match p {
            Plan::Join {
                kind,
                left,
                right,
                cond,
                params,
                ..
            } => {
                let cf = constant_false(cond)
                    || cond.as_ref().is_some_and(|c| {
                        let mut parts = Vec::new();
                        conjuncts(c.clone(), &mut parts);
                        crate::sql::binder::partprune::const_conflict(&parts)
                    });
                match kind {
                    JoinKind::Inner | JoinKind::Semi => empty(left) || empty(right) || cf,
                    JoinKind::Left | JoinKind::Anti => {
                        if empty(left) {
                            true
                        } else {
                            if right_false && empty(right) && cond.is_none() {
                                *cond = Some(Expr::Const(Value::Bool(false), DataType::Bool));
                            }
                            if cf && params.is_empty() && !empty(right) {
                                let d = dummy(right);
                                *right = Arc::new(d);
                            }
                            false
                        }
                    }
                    JoinKind::Right => {
                        if empty(right) {
                            true
                        } else {
                            if cf && params.is_empty() && !empty(left) {
                                let d = dummy(left);
                                *left = Arc::new(d);
                            }
                            false
                        }
                    }
                    JoinKind::Full => empty(left) && empty(right),
                }
            }
            // (A subquery whose plan is proven empty: a dummy rel of its columns.)
            Plan::Barrier { input, .. } => !matches!(&**input, Plan::Empty { .. }) && empty(input),
            Plan::Filter { input, pred } => {
                empty(input)
                    || (constant_false(&Some(pred.clone()))
                        && !matches!(&**input, Plan::Agg { .. } | Plan::Window { .. }))
            }
            _ => false,
        };
        if replace {
            *p = dummy(p);
        }
    }
}
