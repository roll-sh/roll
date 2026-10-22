//! Outer joins inside a join group (deconstruct_jointree's SpecialJoinInfos,
//! make_outerjoininfo, join_is_legal): a tree of inner, left (right), semi and anti joins
//! is one group of FROM items; each non-inner join records which items must be on either
//! side of it (its minimal left and right sets), so the search may reorder joins the
//! outer-join identities allow and no others. A clause carries the items it needs joined
//! before it can be checked (its own, and for one that reads an outer join's nullable
//! side, that join's: Postgres 15's check_outerjoin_delay).

use super::*;

/// A non-inner join of the group (SpecialJoinInfo).
#[derive(Clone, Debug)]
pub(super) struct Sj {
    /// Left, Semi or Anti (a right join is a left one the other way round).
    pub kind: JoinKind,
    pub syn_l: u32,
    pub syn_r: u32,
    pub min_l: u32,
    pub min_r: u32,
    /// The join's condition is strict for some item of its left side.
    pub lhs_strict: bool,
}

/// A join node of the tree, bottom-up: its kind, its sides' items, its condition's
/// conjuncts (over the group's row) and the items joined by inner joins below it.
struct RawJoin {
    kind: JoinKind,
    l: u32,
    r: u32,
    conds: Vec<Expr>,
    inner: u32,
    /// Columns of ON conditions the binder already pushed into the nullable side.
    pushed: Vec<usize>,
}

/// What `gather_group` makes of a join tree.
pub(super) struct Group {
    pub leaves: Vec<(Plan, usize)>,
    /// The clauses, and per clause: the outer join it is the condition of (not pushed
    /// down), the items it needs, the items whose nulls it must see.
    pub clauses: Vec<(Expr, bool)>,
    pub meta: Vec<(Option<usize>, u32, u32)>,
    pub sjs: Vec<Sj>,
    /// The LATERAL items: each one's index, and the values (over the group's row) it reads
    /// from the items before it, as its `Outer(1, k)`.
    pub laterals: Vec<(usize, Vec<Expr>)>,
}

fn rebuild_eq(a: Expr, b: Expr, bad: bool) -> Option<Expr> {
    let ty = a.ty();
    // (A cross-type integer `=`: its own operator.)
    let eq = if ty.without_typmod() != b.ty().without_typmod() {
        crate::sql::func::resolve_binop("=", &ty, &b.ty()).ok()?.def
    } else {
        equality_for(&ty)?
    };
    let e = Expr::Call(eq, vec![a, b], DataType::Bool);
    Some(if bad {
        crate::sql::binder::indeterminate_eq(e)
    } else {
        e
    })
}

/// Phase one: the items (leaves) at their offsets in the group's row, and the joins
/// bottom-up. Semi / anti joins' right sides get offsets past the row (`virt`): their
/// columns don't come out. Returns (items, items joined by inner joins).
fn walk(
    p: &Plan,
    off: usize,
    virt: &mut usize,
    leaves: &mut Vec<(Plan, usize)>,
    joins: &mut Vec<RawJoin>,
    laterals: &mut Vec<(usize, Vec<Expr>)>,
) -> (u32, u32) {
    if let Plan::Join {
        kind,
        left,
        right,
        cond,
        hash,
        params,
        nleft,
        meta,
        ..
    } = p
        && (params.is_empty() || matches!(kind, JoinKind::Inner | JoinKind::Left))
        && *kind != JoinKind::Full
        && leaves.len() < 32
    {
        let nl = *nleft;
        let (l, li) = walk(left, off, virt, leaves, joins, laterals);
        let roff = if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
            let ro = *virt;
            *virt += crate::sql::binder::plan_width(right);
            ro
        } else {
            off + nl
        };
        let map = |c: usize| if c < nl { off + c } else { roff + (c - nl) };
        let (r, ri) = if params.is_empty() {
            walk(right, roff, virt, leaves, joins, laterals)
        } else {
            // A LATERAL item: its values from the left side, which it must be joined to
            // the inside of a nested loop of.
            let i = leaves.len();
            leaves.push(((**right).clone(), roff));
            laterals.push((i, params.iter().map(|e| e.clone().remap_cols(&map)).collect()));
            (1u32 << i.min(31), 0)
        };
        let mut conds = Vec::new();
        if let Some(c) = cond {
            let mut parts = Vec::new();
            conjuncts(c.clone(), &mut parts);
            for e in parts {
                conds.push(e.remap_cols(&map));
            }
        }
        if let Some((lk, rk)) = hash {
            for (i, (a, b)) in lk.iter().zip(rk).enumerate() {
                let (a, bad, coll) = crate::sql::binder::hash_key_operand(a);
                let (b, _, _) = crate::sql::binder::hash_key_operand(b);
                let (a, b) = (a.shift_cols(off as isize), b.shift_cols(roff as isize));
                // (An outer join's clause as written; an inner or semi join's comes from
                // its equivalence class.)
                let (a, b) = if matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Full)
                    && meta.hash_swapped.get(i) == Some(&true)
                {
                    (b, a)
                } else {
                    (a, b)
                };
                if let Some(e) = rebuild_eq(a, b, bad) {
                    conds.push(crate::sql::binder::eq_under(e, coll));
                }
            }
        }
        let inner = if *kind == JoinKind::Inner {
            l | r | li | ri
        } else {
            li | ri
        };
        let (k, jl, jr) = match kind {
            JoinKind::Right => (JoinKind::Left, r, l),
            k => (*k, l, r),
        };
        joins.push(RawJoin {
            kind: k,
            l: jl,
            r: jr,
            conds,
            inner,
            pushed: meta.pushed.iter().map(|&c| map(c)).collect(),
        });
        return (l | r, inner);
    }
    // A WHERE clause left above a join below (its rows' filter): its conjuncts are the
    // tree's clauses there.
    if let Plan::Filter { input, pred } = p
        && matches!(&**input, Plan::Join { kind, params, .. } if *kind != JoinKind::Full && params.is_empty())
        && absorbable(pred)
    {
        let (m, inner) = walk(input, off, virt, leaves, joins, laterals);
        // (A pulled-up subquery's WHERE over outer joins of its own: its items are one
        // unit to an outer join above, which can't be commuted past the clauses.)
        let has_outer = joins.iter().any(|j| j.kind != JoinKind::Inner && j.r & m != 0);
        let inner = if has_outer { inner | m } else { inner };
        let mut conds = Vec::new();
        conjuncts(pred.clone(), &mut conds);
        joins.push(RawJoin {
            kind: JoinKind::Inner,
            l: 0,
            r: 0,
            conds: conds.into_iter().map(|e| e.shift_cols(off as isize)).collect(),
            inner: 0,
            pushed: Vec::new(),
        });
        return (m, inner);
    }
    let i = leaves.len();
    leaves.push((p.clone(), off));
    (1u32 << i.min(31), 0)
}

/// A filter whose clauses can move into a join group: no volatile function, no sublink.
pub(in crate::sql::paths) fn absorbable(pred: &Expr) -> bool {
    let mut ok = !pred.is_volatile();
    pred.walk(&mut |x| {
        if matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)) {
            ok = false;
        }
    });
    ok
}

/// The items for which a clause can't be true when all their columns are NULL
/// (find_nonnullable_rels).
fn strict_items(e: &Expr, item: &dyn Fn(usize) -> u32) -> u32 {
    match e {
        Expr::Col(g, _) => item(*g),
        Expr::And(args) => args.iter().fold(0, |m, a| m | strict_items(a, item)),
        Expr::Or(args) => {
            let mut it = args.iter().map(|a| strict_items(a, item));
            let first = it.next().unwrap_or(0);
            it.fold(first, |m, x| m & x)
        }
        Expr::Call(d, args, _) if d.strict => args.iter().fold(0, |m, a| m | strict_items(a, item)),
        Expr::Cast(x, ..) | Expr::Relabel(x, _) | Expr::Not(x) => strict_items(x, item),
        Expr::IsNull(x, true) => strict_items(x, item),
        Expr::ArrayOp { left, .. } => strict_items(left, item),
        Expr::InList(x, ..) => strict_items(x, item),
        _ => 0,
    }
}

/// check_outerjoin_delay: the items a pushed-down clause needs, given the outer joins
/// below it, and the items it reads that those joins null.
fn delay(mut req: u32, sjs: &[Sj]) -> (u32, u32) {
    let mut nullable = 0;
    if req == 0 {
        return (0, 0);
    }
    loop {
        let mut more = false;
        for s in sjs {
            if req & s.min_r != 0 {
                let scope = s.min_l | s.min_r;
                if scope & !req != 0 {
                    req |= scope;
                    more = true;
                }
                nullable |= s.min_r;
            }
        }
        if !more {
            return (req, nullable);
        }
    }
}

/// The join tree at `p` as one group (None: not a tree of outer joins worth it, or too
/// many items).
pub(super) fn gather_group(p: &Plan, extra: &[Expr]) -> Option<Group> {
    let width = crate::sql::binder::plan_width(p);
    let mut virt = width;
    let mut leaves = Vec::new();
    let mut joins = Vec::new();
    let mut laterals = Vec::new();
    walk(p, 0, &mut virt, &mut leaves, &mut joins, &mut laterals);
    if leaves.len() < 2 || leaves.len() > COLLAPSE_LIMIT {
        return None;
    }
    let gw = leaves
        .iter()
        .map(|(pl, off)| off + crate::sql::binder::plan_width(pl))
        .max()
        .unwrap_or(width)
        .max(width);
    let mut leaf_of = vec![usize::MAX; gw];
    for (i, (pl, off)) in leaves.iter().enumerate() {
        for x in leaf_of
            .iter_mut()
            .take((off + crate::sql::binder::plan_width(pl)).min(gw))
            .skip(*off)
        {
            *x = i;
        }
    }
    let item = |g: usize| match leaf_of.get(g) {
        Some(&l) if l != usize::MAX => 1u32 << l,
        _ => 0,
    };
    let items = |e: &Expr| e.columns().into_iter().fold(0u32, |m, g| m | item(g));
    // The items each LATERAL item reads values of.
    let lateral_deps = |set: u32| {
        laterals
            .iter()
            .filter(|(i, _)| set & (1u32 << i) != 0)
            .fold(0u32, |m, (_, ps)| m | ps.iter().fold(0, |m, e| m | items(e)))
    };
    let mut sjs: Vec<Sj> = Vec::new();
    let mut clauses = Vec::new();
    let mut meta = Vec::new();
    for j in &joins {
        let below = sjs.len();
        if j.kind == JoinKind::Inner {
            for e in &j.conds {
                // (A pseudoconstant ON clause (no column of this level: an outer query's
                // value) holds at its join, as its syntactic scope: inside an outer join's
                // nullable side it gates only that side, not the whole group.)
                let it = items(e);
                let it = if it == 0 && !e.is_volatile() { j.l | j.r } else { it };
                let (req, nullable) = delay(it, &sjs[..below]);
                clauses.push((e.clone(), false));
                meta.push((None, req, nullable));
            }
            continue;
        }
        // make_outerjoininfo.
        let clause_items = j.conds.iter().fold(0u32, |m, e| m | items(e))
            | j.pushed.iter().fold(0u32, |m, &g| m | item(g));
        let strict = j.conds.iter().fold(0u32, |m, e| m | strict_items(e, &item));
        let semi = matches!(j.kind, JoinKind::Semi | JoinKind::Anti);
        // (What the right side's LATERAL items read must be joined before it.)
        let mut min_l = (clause_items | lateral_deps(j.r)) & j.l;
        let mut min_r = (clause_items | j.inner) & j.r;
        for o in &sjs {
            if j.l & o.syn_r != 0
                && clause_items & o.syn_r != 0
                && (semi || strict & o.min_r == 0)
            {
                min_l |= o.syn_l | o.syn_r;
            }
            if j.r & o.syn_r != 0
                && (clause_items & o.syn_r != 0
                    || clause_items & o.min_l == 0
                    || semi
                    || matches!(o.kind, JoinKind::Semi | JoinKind::Anti)
                    || !o.lhs_strict)
            {
                min_r |= o.syn_l | o.syn_r;
            }
        }
        if min_l == 0 {
            min_l = j.l;
        }
        if min_r == 0 {
            min_r = j.r;
        }
        min_l &= !min_r;
        let sj = Sj {
            kind: j.kind,
            syn_l: j.l,
            syn_r: j.r,
            min_l,
            min_r,
            lhs_strict: strict & j.l != 0,
        };
        let me = sjs.len();
        for e in &j.conds {
            let it = items(e);
            if it & j.l != 0 || it == 0 || e.is_volatile() {
                // The join's own condition: checked exactly where it joins (a volatile
                // one too: the binder kept it out of the nullable side).
                clauses.push((e.clone(), true));
                // (A semi join's clause needs only its own items, as an inner join's:
                // ojscope is NULL for it; join_is_legal keeps it from joining early.)
                let req = if j.kind == JoinKind::Semi { it } else { it | min_l | min_r };
                meta.push((Some(me), req, 0));
            } else {
                // A degenerate one (only the nullable side's items): pushed down there.
                let (req, nullable) = delay(it, &sjs[..below]);
                clauses.push((e.clone(), false));
                meta.push((None, req, nullable));
            }
        }
        sjs.push(sj);
    }
    // A WHERE clause over the whole tree (a Filter above it), pushed down as far as the
    // outer joins below let it.
    for e in extra {
        let (req, nullable) = delay(items(e), &sjs);
        clauses.push((e.clone(), false));
        meta.push((None, req, nullable));
    }
    Some(Group {
        leaves,
        clauses,
        meta,
        sjs,
        laterals,
    })
}

impl Search<'_, '_, '_> {
    /// join_is_legal: whether the items of `a` and `b` may be joined now, and if so as
    /// which outer join (None: an inner join) and whether `b` is its left side.
    pub(super) fn join_is_legal(&self, a: u32, b: u32) -> Option<(Option<usize>, bool)> {
        self.join_is_legal_u(a, b).map(|(sj, rev, _)| (sj, rev))
    }

    /// join_is_legal, and whether the join is legal only with the semi join's right side
    /// unique-ified (it is exactly one side, the other less than the semi join's left side).
    pub(super) fn join_is_legal_u(&self, a: u32, b: u32) -> Option<(Option<usize>, bool, bool)> {
        let j = a | b;
        let mut matched: Option<(usize, bool)> = None;
        let mut unique = false;
        // (create_unique_path succeeds: one item, joined by equalities.)
        let uniqueable = |x: u32| {
            x.count_ones() == 1
                && self
                    .rel_index(x)
                    .is_some_and(|r| self.unique_rel(r, &self.semi_rhs_clauses(x)).is_some())
        };
        let mut must_be_left = false;
        for (i, s) in self.sjs.iter().enumerate() {
            if s.min_r & j == 0 || j & !s.min_r == 0 {
                continue;
            }
            let done = |x: u32| s.min_l & !x == 0 && s.min_r & !x == 0;
            if done(a) || done(b) {
                continue;
            }
            if s.kind == JoinKind::Semi {
                if s.syn_r & !a == 0 && s.syn_r != a {
                    continue;
                }
                if s.syn_r & !b == 0 && s.syn_r != b {
                    continue;
                }
            }
            if s.min_l & !a == 0 && s.min_r & !b == 0 {
                if matched.is_some() {
                    return None;
                }
                matched = Some((i, false));
            } else if s.min_l & !b == 0 && s.min_r & !a == 0 {
                if matched.is_some() {
                    return None;
                }
                matched = Some((i, true));
            } else if s.kind == JoinKind::Semi && s.syn_r == b && uniqueable(b) {
                // (For a semi join, the right side unique-ified joins anything.)
                if matched.is_some() {
                    return None;
                }
                matched = Some((i, false));
                unique = true;
            } else if s.kind == JoinKind::Semi && s.syn_r == a && uniqueable(a) {
                if matched.is_some() {
                    return None;
                }
                matched = Some((i, true));
                unique = true;
            } else {
                if a & s.min_r != 0 && b & s.min_r != 0 {
                    continue;
                }
                if s.kind != JoinKind::Left || j & s.min_l != 0 {
                    return None;
                }
                must_be_left = true;
            }
        }
        if must_be_left {
            match matched {
                Some((i, _)) if self.sjs[i].kind == JoinKind::Left && self.sjs[i].lhs_strict => {}
                _ => return None,
            }
        }
        Some(match matched {
            Some((i, rev)) => (Some(i), rev, unique),
            None => (None, false, false),
        })
    }

    /// The clauses a semi join's right side `x` (one item) is compared by: all of them
    /// between it and the other items (its semi_rhs_exprs are unique-ified together).
    pub(super) fn semi_rhs_clauses(&self, x: u32) -> Vec<usize> {
        let full = (1u32 << self.leaves.len()) - 1;
        self.crossing(x, full & !x)
    }

    /// have_join_order_restriction.
    pub(super) fn order_restriction(&self, a: u32, b: u32) -> bool {
        // A direct LATERAL reference: the join is tried whatever the outer joins say.
        if self.lateral_pair(a, b) {
            return true;
        }
        let mut result = false;
        for s in &self.sjs {
            if (s.min_l & !a == 0 && s.min_r & !b == 0)
                || (s.min_l & !b == 0 && s.min_r & !a == 0)
                || (s.min_r & a != 0 && s.min_r & b != 0)
                || (s.min_l & a != 0 && s.min_l & b != 0)
            {
                result = true;
                break;
            }
        }
        if result && (self.legal_joinclause(a) || self.legal_joinclause(b)) {
            result = false;
        }
        result
    }

    /// has_join_restriction: some outer join constrains how `a` joins.
    pub(super) fn has_join_restriction(&self, a: u32) -> bool {
        if self.leaves.iter().enumerate().any(|(l, leaf)| {
            leaf.lateral
                .as_ref()
                .is_some_and(|(_, deps)| a & (1u32 << l) != 0 || deps & a != 0)
        }) {
            return true;
        }
        self.sjs.iter().any(|s| {
            !(s.min_l & !a == 0 && s.min_r & !a == 0)
                && (a & (s.min_l | s.min_r) != 0)
        })
    }

    /// has_legal_joinclause: `a` has a clause to join some other base set by, legally.
    fn legal_joinclause(&self, a: u32) -> bool {
        let all = (1u32 << self.leaves.len()) - 1;
        for l in 0..self.leaves.len() {
            let b = 1u32 << l;
            if a & b != 0 {
                continue;
            }
            if self.relevant_clause(a, b) && self.join_is_legal(a, b).is_some() {
                return true;
            }
        }
        let _ = all;
        false
    }

    /// have_relevant_joinclause: a clause needs exactly items of both.
    pub(super) fn relevant_clause(&self, a: u32, b: u32) -> bool {
        self.clauses.iter().any(|c| {
            c.req != 0 && c.req & !(a | b) == 0 && c.req & a != 0 && c.req & b != 0
        })
    }

    /// The clauses to check at the join of `a` and `b`: those needing items of both and
    /// no others (build_joinrel_restrictlist); clauses of no item or one at the top.
    pub(super) fn general_restrict(&self, a: u32, b: u32) -> Vec<usize> {
        let u = a | b;
        let full = (1u32 << self.leaves.len()) - 1;
        (0..self.clauses.len())
            .filter(|&i| {
                let r = self.clauses[i].req;
                if r.count_ones() <= 1 {
                    return u == full && (r == 0 || self.clauses[i].leaves.count_ones() <= 1);
                }
                r & !u == 0 && r & !a != 0 && r & !b != 0
            })
            .collect()
    }

    /// A clause can parameterize a scan of leaf `l` (join_clause_is_movable_to): not an
    /// outer join's condition moved to its outer side, nor a clause that must see nulls
    /// the leaf would be on the nullable side of.
    pub(super) fn movable_to(&self, c: usize, l: usize) -> bool {
        if !self.general {
            return true;
        }
        let cl = &self.clauses[c];
        let bit = 1u32 << l;
        match cl.sj {
            Some(t) => self.sjs[t].syn_r & bit != 0,
            None => cl.nullable & bit == 0 && cl.req == cl.leaves,
        }
    }
}
