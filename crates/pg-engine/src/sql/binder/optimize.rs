//! Plan rewrites done while binding: pushing filters into scans and join sides, choosing
//! indexes, hash join keys, and skipping sorts that an index scan already provides.

use crate::catalog::{Catalog, Index, IndexKey, Table};
use crate::sql::expr::Expr;
use crate::sql::plan::{IndexBounds, JoinKind, Plan, ScanPlan, SortKey};
use crate::types::{DataType, Value};
use std::sync::Arc;

/// Number of columns a plan produces.
pub fn width(p: &Plan) -> usize {
    match p {
        Plan::One => 0,
        Plan::Empty { ncols, .. }
        | Plan::Values { ncols, .. }
        | Plan::CteScan { ncols, .. }
        | Plan::WorkScan { ncols, .. }
        | Plan::EnrScan { ncols, .. } => *ncols,
        Plan::Recursive { ncols, .. } => *ncols,
        Plan::Scan(s) => {
            s.table.columns.len()
                + s.with_key as usize
                + if s.sys {
                    crate::exec::scan::NUM_SYS_COLS
                } else {
                    0
                }
        }
        Plan::SysScan { table, .. } => table.columns().len(),
        Plan::FunctionScan {
            calls, ordinality, ..
        } => calls.iter().map(|c| c.ncols).sum::<usize>() + *ordinality as usize,
        Plan::PlCall { ncols, .. } => *ncols,
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => width(input),
        Plan::Project { exprs, .. } | Plan::ProjectSet { exprs, .. } => exprs.len(),
        Plan::Agg {
            groups, sets, aggs, ..
        } => groups.len() + aggs.len() + sets.is_some() as usize,
        Plan::Window { input, calls } => width(input) + calls.len(),
        Plan::SetOp { left, .. } => width(left),
        Plan::Append { inputs, .. } => inputs.first().map(|i| width(i)).unwrap_or(0),
        Plan::Join {
            kind: JoinKind::Semi | JoinKind::Anti,
            nleft,
            ..
        } => *nleft,
        Plan::Join { nleft, nright, .. } => nleft + nright,
        Plan::With { body, .. } => width(body),
        Plan::Insert(p) => p.returning.len(),
        Plan::Update(p) => p.returning.len(),
        Plan::Delete(p) => p.returning.len(),
        Plan::Merge(p) => p.returning.len(),
    }
}

/// Split an AND tree into its conjuncts.
pub fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::And(items) => {
            for i in items {
                conjuncts(i, out);
            }
        }
        Expr::Const(Value::Bool(true), _) => {}
        e => out.push(e),
    }
}

pub(super) fn and_all(mut items: Vec<Expr>) -> Option<Expr> {
    match items.len() {
        0 => None,
        1 => items.pop(),
        _ => Some(Expr::And(items)),
    }
}

/// A scan's column names as a dummy rel in its place shows them.
pub(crate) fn scan_names(s: &crate::sql::plan::ScanPlan) -> Vec<String> {
    s.table
        .columns
        .iter()
        .map(|c| {
            let n = s
                .col_aliases
                .as_ref()
                .and_then(|a| a.iter().find(|(o, _)| *o == c.name).map(|(_, a)| a.clone()))
                .unwrap_or_else(|| c.name.clone());
            crate::catalog::quote_ident(&n)
        })
        .collect()
}

/// The name EXPLAIN qualifies a scan's columns with (before making it unique).
pub(crate) fn scan_refname(s: &crate::sql::plan::ScanPlan) -> String {
    s.alias.clone().unwrap_or_else(|| s.table.name.clone())
}

/// An empty appendrel's columns (no members: a Result with a false One-Time Filter), its
/// row identity columns too.
pub(crate) fn dummy_scan_names(s: &crate::sql::plan::ScanPlan) -> Vec<String> {
    let mut out = scan_names(s);
    if s.with_key {
        out.push("ctid".into());
    }
    if s.sys {
        out.extend(["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"].map(String::from));
    }
    out
}

thread_local! {
    /// `add_security_filter` is adding row-level security quals.
    static SECURITY_QUALS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The statement being bound has row-level security quals (its own quals then sit at
    /// a higher security level: order_qual_clauses doesn't order them by cost alone).
    static QUERY_RLS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Quals over a reduced outer join's formerly nullable side are being placed: its
    /// subqueries' computed outputs are PlaceHolderVars, not substituted into scans.
    static PHV_OUTPUTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The PlaceHolderVars (column-free computed outputs) the quals being pushed down read.
    static PHV_PENDING: std::cell::RefCell<Vec<Expr>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `add_filter` with the projections' computed outputs as PlaceHolderVars (quals reading
/// them stay above the projection).
pub fn add_filter_phv(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    let saved = PHV_OUTPUTS.with(|f| f.replace(true));
    let out = add_filter(cat, plan, pred);
    PHV_OUTPUTS.with(|f| f.set(saved));
    out
}

/// Binding a statement: whether it has row-level security quals starts false (the
/// previous value, to put back after it).
pub fn begin_statement_rls() -> bool {
    QUERY_RLS.with(|f| f.replace(false))
}

pub fn mark_statement_rls() {
    QUERY_RLS.with(|f| f.set(true));
}

pub fn end_statement_rls(saved: bool) {
    QUERY_RLS.with(|f| f.set(saved));
}

/// Add row-level security quals over `plan`: on a scan they go ahead of the quals already
/// there (the statement's own), as a SELECT's do (security level 0 vs 1).
pub fn add_security_filter(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    QUERY_RLS.with(|f| f.set(true));
    SECURITY_QUALS.with(|f| f.set(true));
    let p = add_filter(cat, plan, pred);
    SECURITY_QUALS.with(|f| f.set(false));
    p
}

/// The pg_proc functions marked LEAKPROOF.
pub(crate) fn leakproof_procs() -> &'static std::collections::HashSet<u32> {
    static SET: std::sync::OnceLock<std::collections::HashSet<u32>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        include_str!("../../catalog/pg_proc.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f.get(10) == Some(&"t")).then(|| f[0].parse().ok()).flatten()
            })
            .collect()
    })
}

/// contain_leaked_vars: whether a qual could reveal the values of the columns it reads to
/// a non-leakproof function (one taking only constants can't).
fn leaks(cat: &Catalog, e: &Expr) -> bool {
    let reads_cols = |args: &[Expr]| {
        let mut any = false;
        for a in args {
            a.walk(&mut |x| any |= matches!(x, Expr::Col(..) | Expr::Outer(..)));
        }
        any
    };
    let mut leak = false;
    e.walk(&mut |x| match x {
        Expr::Call(d, args, _) => {
            // (A made type's operator over a built-in's implementation (`LANGUAGE internal
            // AS 'int8eq'`) is its own function, not leakproof.)
            let made = args.iter().any(|a| matches!(a.ty(), DataType::Base(_)));
            let proof = (leakproof_procs().contains(&d.oid) && !made)
                || cat.functions.get(&d.oid).is_some_and(|f| f.leakproof);
            if !proof && reads_cols(args) {
                leak = true;
            }
        }
        // A user function's call: as its function.
        Expr::Subquery(sq) if sq.call.is_some() => {
            let oid = sq.call.as_ref().map_or(0, |c| c.def.oid);
            let proof = leakproof_procs().contains(&oid)
                || cat.functions.get(&oid).is_some_and(|f| f.leakproof);
            if !proof && reads_cols(&sq.params) {
                leak = true;
            }
        }
        Expr::Subquery(_) => leak = true,
        Expr::Cast(inner, from, to, _)
            if from.without_typmod() != to.without_typmod()
                && !(from.is_string() && to.is_string())
                && reads_cols(std::slice::from_ref(inner)) =>
        {
            leak = true
        }
        _ => {}
    });
    leak
}

/// Order a scan's quals as order_qual_clauses does: by security level (row-level security
/// quals `security` first; a cheap leakproof qual counts as one of them), then cost, stably.
/// A qual's cost, a user function's call costing its COST.
fn full_qual_cost(cat: &Catalog, e: &Expr) -> f64 {
    let mut c = qual_cost(e);
    e.walk(&mut |x| {
        // (A user function's call: its COST, not one operator.)
        if let Expr::Call(d, ..) = x
            && let Some(f) = cat.functions.get(&d.oid)
            && f.cost > 0.0
        {
            c += f.cost as f64 - 1.0;
        }
        if let Expr::Subquery(sq) = x
            && let Some(call) = &sq.call
        {
            let pc = cat
                .functions
                .get(&call.def.oid)
                .map_or(100.0, |f| if f.cost > 0.0 { f.cost as f64 } else { 100.0 });
            c += pc - 1000.0;
        }
    });
    c
}

fn order_security_quals(cat: &Catalog, first: Vec<Expr>, rest: Vec<Expr>) -> Vec<Expr> {
    let cost = |e: &Expr| full_qual_cost(cat, e);
    // `first` keep their order; of `rest` (in their order), a cheap leakproof one moves ahead
    // of the costlier quals of `first` before it.
    let mut out: Vec<(bool, f64, Expr)> = first.into_iter().map(|e| (true, cost(&e), e)).collect();
    for e in rest {
        let c = cost(&e);
        let mut j = out.len();
        if c < 10.0 && !leaks(cat, &e) {
            while j > 0 && out[j - 1].0 && out[j - 1].1 > c {
                j -= 1;
            }
        }
        out.insert(j, (false, c, e));
    }
    out.into_iter().map(|i| i.2).collect()
}

/// Add a filter over `plan`'s output, pushing it as far down as is safe.
/// eval_const_expressions' CoerceToDomain of a domain without constraints: a RelabelType,
/// which collapses into the implicit one an operator applies to its base type (the
/// operand is the value itself, `relname = 'x'::name`).
fn strip_free_domains(e: Expr) -> Expr {
    let free = |d: &crate::sql::expr::DomainCoerce| {
        !d.tag && !d.array && !d.not_null && d.checks.is_empty() && d.base.is_none()
    };
    e.transform(&mut |x| {
        Ok(match x {
            Expr::Call(d, args, ty)
                if !d.name.is_empty()
                    && !d.name.chars().any(|c| c.is_alphanumeric() || c == '_')
                    && args.iter().any(|a| matches!(a, Expr::CoerceToDomain(_, dc) if !dc.tag && !dc.array)) =>
            {
                // (A domain with constraints stays checked: its value relabeled to the
                // base type, `((x)::d)::integer`.)
                let args = args
                    .into_iter()
                    .map(|a| match a {
                        Expr::CoerceToDomain(inner, dc) if free(&dc) => *inner,
                        a @ Expr::CoerceToDomain(_, _) if !matches!(&a, Expr::CoerceToDomain(_, dc) if dc.tag || dc.array) => {
                            let t = a.ty();
                            Expr::Relabel(Box::new(a), t)
                        }
                        a => a,
                    })
                    .collect();
                Expr::Call(d, args, ty)
            }
            x => x,
        })
    })
    .unwrap()
}

pub fn add_filter(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    let mut parts = Vec::new();
    conjuncts(strip_free_domains(pred), &mut parts);
    if parts.is_empty() {
        return plan;
    }
    match plan {
        Plan::Scan(mut s) => {
            // (eval_const_expressions: a COLLATE naming the collation its column already has
            // is no relabelling at all.)
            let parts: Vec<Expr> = parts.into_iter().map(|p| same_collation_dropped(p, &s.table.columns)).collect();
            let mut all = Vec::new();
            if let Some(f) = s.filter.take() {
                conjuncts(f, &mut all);
            }
            // (Quals reading a PlaceHolderVar: no index quals.)
            PHV_PENDING.with(|pend| {
                let pend = pend.borrow();
                if pend.is_empty() {
                    return;
                }
                for p in &parts {
                    let mut reads = false;
                    p.walk(&mut |x| reads |= pend.iter().any(|v| v.same_as(x)));
                    if reads && !s.phv_quals.iter().any(|q| q.same_as(p)) {
                        s.phv_quals.push(p.clone());
                    }
                }
            });
            // (As baserestrictinfo holds them: written, the equivalence class equalities
            // coming back after the others.)
            for p in &parts {
                if ec_clause(p) {
                    s.written.push(p.clone());
                } else {
                    let at = s.written.iter().position(ec_clause).unwrap_or(s.written.len());
                    s.written.insert(at, p.clone());
                }
            }
            // Cheapest first (order_qual_clauses); quals already there (row security) stay
            // ahead.
            let mut parts = parts;
            // Equalities go into equivalence classes and come back after the other quals.
            // (A user function's call costs its COST; with row-level security in the
            // statement its own quals keep their security level's order.)
            let rls = QUERY_RLS.with(|f| f.get());
            let cost = |e: &Expr| if rls { qual_cost(e) } else { full_qual_cost(cat, e) };
            parts.sort_by(|a, b| {
                cost(a)
                    .total_cmp(&cost(b))
                    .then(ec_clause(a).cmp(&ec_clause(b)))
            });
            // Row-level security quals added under the statement's own; else the new quals
            // after those there (row security), cheap leakproof ones among them.
            let all = if SECURITY_QUALS.with(|f| f.get()) {
                order_security_quals(cat, parts, all)
            } else if all.is_empty() {
                parts
            } else if rls {
                order_security_quals(cat, all, parts)
            } else {
                // (No security barrier: one level, all of them by cost.)
                let mut merged = all;
                merged.extend(parts);
                merged.sort_by(|a, b| {
                    full_qual_cost(cat, a)
                        .total_cmp(&full_qual_cost(cat, b))
                        .then(ec_clause(a).cmp(&ec_clause(b)))
                });
                merged
            };
            let mut all = all;
            // restriction_is_always_true / _false: IS [NOT] NULL of a NOT NULL column.
            let not_null = |x: &Expr| {
                matches!(x, Expr::Col(c, _) if s.table.columns.get(*c).is_some_and(|col| col.not_null))
            };
            reduce_null_tests(&mut all, &not_null);
            ec_scan_constants(&mut all);
            // Constraint exclusion: the filter contradicts the table's constraints (or
            // itself, equating a column with two constants).
            // (A constant false qual: a dummy rel, its columns still named.)
            if all.iter().any(super::refute::always_false) {
                let names = scan_names(&s);
                let rels = vec![Some(scan_refname(&s)); names.len()];
                let ncols = width(&Plan::Scan(s));
                return Plan::Empty {
                    ncols,
                    cols: Some(Arc::new(names)),
                    info: Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec: Vec::new() })),
                };
            }
            if super::partprune::const_conflict(&all)
                || s.part
                .as_ref()
                .is_some_and(|l| super::partition::pruned(&all, l))
                || s.excl.as_ref().is_some_and(|c| {
                    super::refute::refuted(&all, c) || super::refute::self_refuted(&all)
                })
                || s.part_excl.as_ref().is_some_and(|l| {
                    (0..l.len()).any(|i| super::partprune::level_pruned(&all, l, i))
                })
            {
                // (Its columns still named, and the classes its quals made: a key equated
                // with a constant stays redundant.)
                let names = scan_names(&s);
                let rels = vec![Some(scan_refname(&s)); names.len()];
                s.filter = and_all(all);
                let scan = Plan::Scan(s);
                let ec = crate::sql::paths::column_classes(&scan);
                let ncols = width(&scan);
                let cols = (names.len() == ncols).then(|| Arc::new(names));
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec }));
                return Plan::Empty { ncols, cols, info };
            }
            s.filter = and_all(all);
            // New quals: the access path is chosen again (unless a sort relies on it).
            if !matches!(
                s.access,
                crate::sql::plan::Access::Index { ordered: true, .. }
            ) {
                s.access = crate::sql::plan::Access::Unplanned;
                choose_index(cat, &mut s);
            }
            Plan::Scan(s)
        }
        // No rows stay no rows.
        Plan::Empty { ncols, cols, info } => Plan::Empty { ncols, cols, info },
        Plan::SysScan { table, filter, alias, .. } => {
            let mut all = Vec::new();
            if let Some(f) = filter {
                conjuncts(f, &mut all);
            }
            all.extend(parts);
            Plan::SysScan {
                table,
                filter: and_all(all),
                path: None,
                alias,
            }
        }
        Plan::Filter { input, pred } => {
            conjuncts(pred, &mut parts);
            let input = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            add_filter(cat, input, Expr::And(parts))
        }
        Plan::Join {
            kind: kind @ (JoinKind::Inner | JoinKind::Left | JoinKind::Right),
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        } => {
            let mut above = Vec::new();
            let mut lpush = Vec::new();
            let mut rpush = Vec::new();
            let mut cond_parts = Vec::new();
            if let Some(c) = cond {
                conjuncts(c, &mut cond_parts);
            }
            for p in parts {
                // (A volatile clause goes to the relations it reads, as any other; one that
                // reads none is checked above.)
                if p.is_volatile() && p.columns().is_empty() {
                    above.push(p);
                    continue;
                }
                // (So is one that reads none at all: distribute_qual_to_rels gives a
                // pseudoconstant clause the whole join's scope, a gating qual of the join.)
                if p.columns().is_empty() && !matches!(p, Expr::Const(..)) {
                    if kind == JoinKind::Inner {
                        cond_parts.push(p);
                    } else {
                        above.push(p);
                    }
                    continue;
                }
                match side(&p, nleft) {
                    Side::Left | Side::None if kind != JoinKind::Right => lpush.push(p),
                    Side::Right if kind != JoinKind::Left && right_pushable(&p, &params) => {
                        rpush.push(p.shift_cols(-(nleft as isize)))
                    }
                    _ if kind == JoinKind::Inner => cond_parts.push(p),
                    _ => above.push(p),
                }
            }
            let mut hash = hash;
            // (Into an outer join's nullable side only: its own constants don't hold for
            // the null-extended rows.)
            let (to_right, to_left) = match kind {
                JoinKind::Inner => (true, true),
                JoinKind::Left => (true, false),
                _ => (false, true),
            };
            // (The keys' original orientations, kept with them below.)
            let old_keys = hash.clone();
            let (old_hs, old_kf) = (meta.hash_swapped.clone(), meta.key_flipped.clone());
            let mut kept = meta.kept;
            let old_pushed = meta.pushed;
            ec_constants(
                cat,
                &mut lpush,
                &mut rpush,
                &mut hash,
                &mut cond_parts,
                nleft,
                (to_right, to_left),
                &mut kept,
            );
            let mut l = Arc::try_unwrap(left).unwrap_or_else(|a| (*a).clone());
            let mut r = Arc::try_unwrap(right).unwrap_or_else(|a| (*a).clone());
            if !lpush.is_empty() {
                l = add_filter(cat, l, Expr::And(lpush));
            }
            if !rpush.is_empty() {
                r = add_filter(cat, r, Expr::And(rpush));
            }
            let mut j = join(cat, kind, l, r, and_all(cond_parts), params, nleft, nright);
            if let Plan::Join { meta, .. } = &mut j {
                meta.kept = kept;
                for c in old_pushed {
                    if !meta.pushed.contains(&c) {
                        meta.pushed.push(c);
                    }
                }
            }
            // Keep the equi-join keys the join already had.
            if let (Some((hl, hr)), Plan::Join { hash: h, meta: m, .. }) = (hash, &mut j) {
                let n = h.as_ref().map_or(0, |(l2, _)| l2.len());
                let flags = |old: &[bool]| -> Vec<bool> {
                    hl.iter()
                        .zip(&hr)
                        .map(|(a, b)| {
                            old_keys
                                .as_ref()
                                .and_then(|(ol, or)| {
                                    ol.iter().zip(or).position(|(x, y)| x.same_as(a) && y.same_as(b))
                                })
                                .and_then(|i| old.get(i).copied())
                                .unwrap_or(false)
                        })
                        .collect()
                };
                let (hs, kf) = (flags(&old_hs), flags(&old_kf));
                if hs.iter().any(|x| *x) {
                    m.hash_swapped.resize(n, false);
                    m.hash_swapped.extend(hs);
                }
                if kf.iter().any(|x| *x) {
                    m.key_flipped.resize(n, false);
                    m.key_flipped.extend(kf);
                }
                match h {
                    Some((l2, r2)) => {
                        l2.extend(hl);
                        r2.extend(hr);
                    }
                    None => *h = Some((hl, hr)),
                }
            }
            match and_all(above) {
                Some(p) => Plan::Filter {
                    input: Arc::new(j),
                    pred: p,
                },
                None => j,
            }
        }
        Plan::Append {
            inputs,
            inh: inh @ Some(_),
        } => {
            let ncols = inputs.first().map_or(0, |i| width(i));
            // (A dummy relation's columns keep their names.)
            let names = inputs.first().and_then(|i| match &**i {
                Plan::Scan(s) => Some(scan_names(s)),
                Plan::Project { input, exprs } => match &**input {
                    Plan::Scan(s) => {
                        let n = scan_names(s);
                        exprs
                            .iter()
                            .map(|e| match e {
                                Expr::Col(j, _) => n.get(*j).cloned(),
                                _ => None,
                            })
                            .collect::<Option<Vec<_>>>()
                    }
                    _ => None,
                },
                _ => None,
            });
            let cols = names.map(Arc::new);
            // (Its columns the parent's.)
            let rels = vec![inh.clone(); ncols];
            // A constant false filter excludes every member whatever constraint_exclusion
            // says.
            if parts.iter().any(super::refute::always_false) {
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec: Vec::new() }));
                return Plan::Empty { ncols, cols, info };
            }
            // (The classes the parent's quals make, should no member be left.)
            let ec = {
                let f = Plan::Filter {
                    input: Arc::new(Plan::Empty { ncols, cols: None, info: None }),
                    pred: and_all(parts.clone()).unwrap(),
                };
                crate::sql::paths::column_classes(&f)
            };
            let pred = and_all(parts).unwrap();
            let inputs: Vec<Arc<Plan>> = inputs
                .into_iter()
                .map(|i| {
                    Arc::new(add_member_filter(
                        cat,
                        Arc::try_unwrap(i).unwrap_or_else(|a| (*a).clone()),
                        pred.clone(),
                    ))
                })
                .filter(|p| !super::inherit::excluded(p))
                .collect();
            if inputs.is_empty() {
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec }));
                return Plan::Empty { ncols, cols, info };
            }
            Plan::Append { inputs, inh }
        }
        // reconsider_full_join_clause: `COALESCE(l, r) = constant` over a full join on
        // `l = r` (a USING column) holds the constant on both sides: each side is restricted
        // to it, and the join clause and that one say nothing more.
        Plan::Join {
            kind: JoinKind::Full,
            left,
            right,
            cond: None,
            hash: Some((lk, rk)),
            params,
            nleft,
            nright,
            meta,
        } if params.is_empty() && parts.iter().any(|p| full_key_const(p, &lk, &rk, nleft).is_some()) => {
            let (mut lk, mut rk) = (lk, rk);
            let mut hs = meta.hash_swapped.clone();
            let (mut lpush, mut rpush, mut above) = (Vec::new(), Vec::new(), Vec::new());
            for p in parts {
                match full_key_const(&p, &lk, &rk, nleft) {
                    Some((i, c, def)) => {
                        lpush.push(Expr::Call(def.clone(), vec![lk[i].clone(), c.clone()], DataType::Bool));
                        rpush.push(Expr::Call(def, vec![rk[i].clone(), c], DataType::Bool));
                        lk.remove(i);
                        rk.remove(i);
                        if i < hs.len() {
                            hs.remove(i);
                        }
                    }
                    None => above.push(p),
                }
            }
            let l = add_filter(cat, Arc::try_unwrap(left).unwrap_or_else(|a| (*a).clone()), Expr::And(lpush));
            let r = add_filter(cat, Arc::try_unwrap(right).unwrap_or_else(|a| (*a).clone()), Expr::And(rpush));
            let mut meta = meta;
            meta.hash_swapped = hs;
            let j = Plan::Join {
                kind: JoinKind::Full,
                left: Arc::new(l),
                right: Arc::new(r),
                cond: None,
                hash: (!lk.is_empty()).then_some((lk, rk)),
                params,
                nleft,
                nright,
                meta,
            };
            match and_all(above) {
                Some(p) => add_filter(cat, j, p),
                None => j,
            }
        }
        // A semi / anti join's rows are its left side's: the filter goes there.
        Plan::Join {
            kind: kind @ (JoinKind::Semi | JoinKind::Anti),
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        } => {
            let (above, push): (Vec<Expr>, Vec<Expr>) =
                parts.into_iter().partition(|p| p.is_volatile() && p.columns().is_empty());
            let mut left = left;
            if !push.is_empty() {
                let l = Arc::try_unwrap(left).unwrap_or_else(|a| (*a).clone());
                left = Arc::new(add_filter(cat, l, Expr::And(push)));
            }
            let j = Plan::Join {
                kind,
                left,
                right,
                cond,
                hash,
                params,
                nleft,
                nright,
                meta,
            };
            match and_all(above) {
                Some(p) => Plan::Filter {
                    input: Arc::new(j),
                    pred: p,
                },
                None => j,
            }
        }
        // Through a projection of plain columns (an inheritance child's mapping to its parent).
        // (Or a view's / subquery's: the columns the clauses read being plain values.)
        // Over window functions: the clauses on columns every window partitions by.
        Plan::Project { input, exprs }
            if matches!(&*input, Plan::Window { .. })
                && parts.iter().any(|p| window_pushable(p, &exprs, &input)) =>
        {
            let (down, up): (Vec<Expr>, Vec<Expr>) =
                parts.into_iter().partition(|p| window_pushable(p, &exprs, &input));
            let inner = Expr::And(down)
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(i, _) => exprs[i].clone(),
                        e => e,
                    })
                })
                .unwrap();
            let Plan::Window { input: wi, calls } = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone())
            else {
                unreachable!()
            };
            let wi = Arc::try_unwrap(wi).unwrap_or_else(|a| (*a).clone());
            let proj = Plan::Project {
                input: Arc::new(Plan::Window {
                    input: Arc::new(add_filter(cat, wi, inner)),
                    calls,
                }),
                exprs,
            };
            match and_all(up) {
                Some(p) => Plan::Filter {
                    input: Arc::new(proj),
                    pred: p,
                },
                None => proj,
            }
        }
        // Into a plain view (or subquery) pulled up: the clauses on its plain values, a
        // volatile one too (a security barrier is a `Barrier` below); the rest above.
        Plan::Project { input, exprs }
            if matches!(
                &*input,
                Plan::Scan(_) | Plan::SysScan { .. } | Plan::Join { .. } | Plan::Project { .. } | Plan::Append { inh: Some(_), .. }
            ) && parts.iter().any(|p| projectable(p, &exprs)) =>
        {
            let (down, up): (Vec<Expr>, Vec<Expr>) =
                parts.into_iter().partition(|p| projectable(p, &exprs));
            let inner_cols: Vec<usize> = down.iter().flat_map(|p| p.columns()).collect();
            let inner = and_all(down)
                .unwrap()
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(i, _) => exprs[i].clone(),
                        e => e,
                    })
                })
                .unwrap();
            let input = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            // (Under PlaceHolderVar semantics the computed outputs read stay what they
            // are: the scans' quals over them aren't index quals.)
            let phvs: Vec<Expr> = if PHV_OUTPUTS.with(|f| f.get()) {
                let mut v = Vec::new();
                for c in inner_cols.iter() {
                    if let Some(e) = exprs.get(*c)
                        && !matches!(e, Expr::Col(..))
                        && !v.iter().any(|x: &Expr| x.same_as(e))
                    {
                        v.push(e.clone());
                    }
                }
                v
            } else {
                Vec::new()
            };
            let n_before = PHV_PENDING.with(|p| {
                let mut p = p.borrow_mut();
                let n = p.len();
                p.extend(phvs);
                n
            });
            let filtered = add_filter(cat, input, inner);
            PHV_PENDING.with(|p| p.borrow_mut().truncate(n_before));
            let proj = Plan::Project {
                input: Arc::new(filtered),
                exprs,
            };
            match and_all(up) {
                Some(p) => Plan::Filter {
                    input: Arc::new(proj),
                    pred: p,
                },
                None => proj,
            }
        }
        // A security barrier: only the clauses that can't leak its rows' values go into
        // it (leakproof, not volatile); the rest stay above.
        Plan::Barrier { input, alias, cols, secure, sorted } => {
            // (A subquery grouping by grouping sets takes none; one not pulled up for its
            // set-returning or volatile outputs, the clauses not reading them that aren't
            // volatile themselves: subquery_is_pushdown_safe.)
            // (Nor does one with a LIMIT / OFFSET; a DISTINCT ON subquery takes none on its
            // other columns: subquery_is_pushdown_safe, check_output_expressions.)
            let fence = super::select::grouping_sets_subquery(&input) || (!secure && limited_level(&input));
            let mut unsafe_out = if secure { Vec::new() } else { unsafe_outputs(&input) };
            if !secure && let Some(d) = distinct_on_unsafe(&input) {
                for (u, x) in unsafe_out.iter_mut().zip(d) {
                    *u |= x;
                }
            }
            // (qual_is_pushdown_safe: an operator of another btree family than the
            // grouping's equality — record_image_ops, text_pattern_ops — can tell apart
            // rows the subquery's DISTINCT or GROUP BY merged.)
            let grouped = !secure && grouping_level(&input);
            // (Nor a clause comparing a column the level groups under a nondeterministic
            // collation other than by that collation: it can tell apart rows the grouping
            // merged.)
            let nondet: Vec<Option<String>> = if grouped {
                (0..width(&input))
                    .map(|c| {
                        output_collation(&input, c).filter(|n| {
                            crate::types::collation::by_name(cat, n).is_some_and(|c| !c.deterministic)
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let (down, up): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|p| {
                !fence
                    && if secure {
                        !leaks(cat, p)
                    } else {
                        !crate::sql::paths::expr_volatile_deep(p)
                            && p.columns().iter().all(|c| !unsafe_out.get(*c).copied().unwrap_or(false))
                            && !(grouped && foreign_family_op(p))
                            && same_collation_tests(p, &nondet)
                    }
            });
            let input = if down.is_empty() {
                input
            } else {
                let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
                Arc::new(if secure { add_filter(cat, i, Expr::And(down)) } else { push_into_level(cat, i, down) })
            };
            let b = Plan::Barrier { input, alias, cols, secure, sorted };
            match and_all(up) {
                Some(p) => Plan::Filter {
                    input: Arc::new(b),
                    pred: p,
                },
                None => b,
            }
        }
        // A select list with set-returning calls: clauses not reading their results (nor
        // anything volatile) go below it, over the values they read.
        Plan::ProjectSet { input, exprs, srfs } => {
            // (The calls' results are columns past the input's.)
            let iw = width(&input);
            let (down, up): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|p| {
                !crate::sql::paths::expr_volatile_deep(p)
                    && p.columns().iter().all(|&c| {
                        exprs.get(c).is_some_and(|e| {
                            let mut ok = !crate::sql::paths::expr_volatile_deep(e) && e.columns().iter().all(|&x| x < iw);
                            e.walk(&mut |x| ok &= !matches!(x, Expr::Srf(..) | Expr::Subquery(_)));
                            ok
                        })
                    })
            });
            let input = if down.is_empty() {
                input
            } else {
                let mapped: Vec<Expr> = down
                    .into_iter()
                    .map(|p| {
                        p.transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(c, t) => exprs.get(c).cloned().unwrap_or(Expr::Col(c, t)),
                                x => x,
                            })
                        })
                        .unwrap()
                    })
                    .collect();
                let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
                Arc::new(add_filter(cat, i, Expr::And(mapped)))
            };
            let ps = Plan::ProjectSet { input, exprs, srfs };
            match and_all(up) {
                Some(p) => Plan::Filter {
                    input: Arc::new(ps),
                    pred: p,
                },
                None => ps,
            }
        }
        plan => Plan::Filter {
            input: Arc::new(plan),
            pred: and_all(parts).unwrap(),
        },
    }
}

/// Clauses pushed into a FROM subquery's level (subquery_push_qual): through its top
/// projection (its outputs' expressions put in), sort and DISTINCT, and below its
/// grouping when they read only its group keys (a HAVING without aggregates is a WHERE
/// clause there); a clause over aggregates stays over the grouping (its HAVING).
fn push_into_level(cat: &Catalog, plan: Plan, parts: Vec<Expr>) -> Plan {
    if parts.is_empty() {
        return plan;
    }
    match plan {
        Plan::Project { input, exprs }
            if parts.iter().all(|p| {
                p.columns().iter().all(|&c| {
                    exprs.get(c).is_some_and(|e| {
                        let mut ok = !crate::sql::paths::expr_volatile_deep(e);
                        e.walk(&mut |x| ok &= !matches!(x, Expr::Srf(..) | Expr::Window(..) | Expr::Agg(..)));
                        ok
                    })
                })
            }) =>
        {
            let mapped = parts
                .into_iter()
                .map(|p| {
                    p.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) => exprs.get(c).cloned().unwrap_or(Expr::Col(c, t)),
                            x => x,
                        })
                    })
                    .unwrap()
                })
                .collect();
            let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            Plan::Project {
                input: Arc::new(push_into_level(cat, i, mapped)),
                exprs,
            }
        }
        Plan::Sort { input, keys } => {
            let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            Plan::Sort {
                input: Arc::new(push_into_level(cat, i, parts)),
                keys,
            }
        }
        Plan::Distinct { input, on, order } => {
            let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            Plan::Distinct {
                input: Arc::new(push_into_level(cat, i, parts)),
                on,
                order,
            }
        }
        Plan::Agg { input, groups, sets: None, aggs } if !groups.is_empty() => {
            let ng = groups.len();
            let (below, above): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|p| {
                let cols = p.columns();
                !cols.is_empty() && cols.iter().all(|&c| c < ng)
            });
            let below: Vec<Expr> = below
                .into_iter()
                .map(|p| {
                    p.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) => groups.get(c).cloned().unwrap_or(Expr::Col(c, t)),
                            x => x,
                        })
                    })
                    .unwrap()
                })
                .collect();
            let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            let input = if below.is_empty() { i } else { add_filter(cat, i, Expr::And(below)) };
            let agg = Plan::Agg {
                input: Arc::new(input),
                groups,
                sets: None,
                aggs,
            };
            match and_all(above) {
                Some(p) => add_filter(cat, agg, p),
                None => agg,
            }
        }
        plan => add_filter(cat, plan, Expr::And(parts)),
    }
}

/// A query level that merges rows (DISTINCT, DISTINCT ON, GROUP BY) at its top, or a set
/// operation with one that does (anything but UNION ALL) among its arms.
fn grouping_level(p: &Plan) -> bool {
    match p {
        Plan::Distinct { .. } | Plan::Agg { .. } | Plan::SetOp { .. } => true,
        Plan::Append { inh: None, inputs } => inputs.iter().any(|i| grouping_level(i)),
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Filter { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Context { input, .. } => grouping_level(input),
        Plan::With { body, .. } => grouping_level(body),
        _ => false,
    }
}

/// The collation of output column `c` of a query level, as far as it is a column's
/// (through projections of it, the grouping, set operations' first arm, a scan).
pub(crate) fn output_collation(p: &Plan, c: usize) -> Option<String> {
    match p {
        Plan::Project { input, exprs } => match exprs.get(c)? {
            Expr::Col(j, _) => output_collation(input, *j),
            Expr::Collate(_, name, _) => Some(name.clone()),
            Expr::Relabel(x, _) => match &**x {
                Expr::Col(j, _) => output_collation(input, *j),
                _ => None,
            },
            _ => None,
        },
        Plan::Sort { input, .. }
        | Plan::Filter { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::Limit { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. }
        | Plan::Context { input, .. } => output_collation(input, c),
        Plan::With { body, .. } => output_collation(body, c),
        Plan::Agg { input, groups, .. } => match groups.get(c)? {
            Expr::Col(j, _) => output_collation(input, *j),
            Expr::Collate(_, name, _) => Some(name.clone()),
            _ => None,
        },
        Plan::SetOp { left, .. } => output_collation(left, c),
        Plan::Append { inh: None, inputs } => output_collation(inputs.first()?, c),
        Plan::Join { left, right, nleft, .. } => {
            if c < *nleft {
                output_collation(left, c)
            } else {
                output_collation(right, c - nleft)
            }
        }
        Plan::Scan(s) => s.table.columns.get(c).filter(|col| !col.dropped).and_then(|col| col.collation.clone()),
        _ => None,
    }
}

/// Whether every operation in `p` over a column with a nondeterministic collation (in
/// `nondet`, by column) compares it by that collation.
fn same_collation_tests(p: &Expr, nondet: &[Option<String>]) -> bool {
    if nondet.iter().all(Option::is_none) {
        return true;
    }
    let reads = |e: &Expr| e.columns().iter().any(|c| nondet.get(*c).is_some_and(Option::is_some));
    let mut ok = true;
    p.walk(&mut |x| {
        if let Expr::Call(d, args, _) = x
            && args.iter().any(|a| matches!(a, Expr::Col(..) | Expr::Relabel(..) | Expr::Collate(..)) && reads(a))
        {
            let same = args.iter().all(|a| {
                a.columns().iter().all(|c| match nondet.get(*c) {
                    Some(Some(n)) => d.coll.as_ref().is_some_and(|k| k.name == *n),
                    _ => true,
                })
            });
            ok &= same;
        }
    });
    ok
}

/// expression_has_grouping_conflict (Postgres 18.6): whether `e` could tell apart rows a
/// grouping merged. `key(k)`: column k is a grouping key, with Some(its collation's name)
/// when that is nondeterministic. A key is safe only as a direct operand of a btree
/// comparison under the grouping's equality (an operator of another family conflicts)
/// and, when nondeterministic, under its own collation; any other reference to a
/// nondeterministic key (a function over it, `<>`, LIKE, another collation) conflicts.
pub(crate) fn grouping_conflict(e: &Expr, key: &dyn Fn(usize) -> Option<Option<String>>) -> bool {
    fn strip(e: &Expr) -> &Expr {
        match e {
            Expr::Relabel(x, _) | Expr::Collate(x, ..) => strip(x),
            e => e,
        }
    }
    let coll_of = |d: &crate::sql::func::FuncDef| d.coll.as_ref().map(|c| c.name.clone());
    let mut handled: Vec<*const Expr> = Vec::new();
    let mut conflict = false;
    let operand = |arg: &Expr, d: &crate::sql::func::FuncDef, handled: &mut Vec<*const Expr>| {
        let a = strip(arg);
        if let Expr::Col(k, _) = a
            && let Some(nd) = key(*k)
        {
            if matches!(d.name, "*=" | "*<>" | "*<" | "*<=" | "*>" | "*>=" | "~<~" | "~<=~" | "~>=~" | "~>~") {
                return true;
            }
            if let Some(n) = nd
                && coll_of(d).as_deref() != Some(n.as_str())
            {
                return true;
            }
            handled.push(a as *const Expr);
        }
        false
    };
    let cmp = |d: &crate::sql::func::FuncDef| {
        matches!(
            d.name,
            "=" | "<" | "<=" | ">" | ">=" | "*=" | "*<" | "*<=" | "*>" | "*>=" | "~<~" | "~<=~" | "~>=~" | "~>~"
        )
    };
    e.walk(&mut |x| match x {
        Expr::Call(d, args, _) if args.len() == 2 && cmp(d) => {
            for a in args {
                conflict |= operand(a, d, &mut handled);
            }
        }
        Expr::ArrayOp { left, op, .. } if cmp(op) => conflict |= operand(left, op, &mut handled),
        Expr::InList(left, items, _) => {
            for (_, d) in items {
                conflict |= operand(left, d, &mut handled);
            }
        }
        Expr::Case { operand: Some(op), whens, .. } => {
            let a = strip(op);
            if let Expr::Col(k, _) = a
                && let Some(nd) = key(*k)
            {
                if let Some(n) = nd {
                    for (w, _) in whens {
                        if let Expr::Call(d, ..) = w
                            && coll_of(d).as_deref() != Some(n.as_str())
                        {
                            conflict = true;
                        }
                    }
                }
                handled.push(a as *const Expr);
            }
        }
        _ => {}
    });
    if conflict {
        return true;
    }
    e.walk(&mut |x| {
        if let Expr::Col(k, _) = x
            && let Some(Some(_)) = key(*k)
            && !handled.contains(&(x as *const Expr))
        {
            conflict = true;
        }
    });
    conflict
}

/// A clause using an operator of a btree family other than its type's default one
/// (record_image_ops' `*=`, text_pattern_ops' `~<~`, …).
fn foreign_family_op(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let Expr::Call(d, args, _) = x
            && args.len() == 2
        {
            found |= matches!(d.name, "*=" | "*<>" | "*<" | "*<=" | "*>" | "*>=" | "~<~" | "~<=~" | "~>=~" | "~>~");
        }
    });
    found
}

/// A query level with a LIMIT / OFFSET of its own (above its FROM's nodes).
pub(crate) fn limited_level(p: &Plan) -> bool {
    match p {
        Plan::Limit { .. } => true,
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Context { input, .. } => limited_level(input),
        Plan::With { body, .. } => limited_level(body),
        _ => false,
    }
}

/// A DISTINCT ON level's output columns that aren't among its DISTINCT ON keys (None: no
/// DISTINCT ON at the level's top).
pub(crate) fn distinct_on_unsafe(p: &Plan) -> Option<Vec<bool>> {
    match p {
        Plan::Project { input, exprs } => {
            let below = distinct_on_unsafe(input)?;
            Some(
                exprs
                    .iter()
                    .map(|e| match e {
                        Expr::Col(j, _) => below.get(*j).copied().unwrap_or(true),
                        e => e.columns().iter().any(|j| below.get(*j).copied().unwrap_or(true)),
                    })
                    .collect(),
            )
        }
        Plan::Sort { input, .. }
        | Plan::Filter { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Context { input, .. } => distinct_on_unsafe(input),
        Plan::With { body, .. } => distinct_on_unsafe(body),
        Plan::Distinct { input, on: Some(on), .. } => Some(
            (0..width(input))
                .map(|j| {
                    !on.iter().any(|e| {
                        // (A key carrying its column's nondeterministic collation is the
                        // column.)
                        let e = match e {
                            Expr::Collate(x, _, -1) => &**x,
                            e => e,
                        };
                        matches!(e, Expr::Col(k, _) if *k == j)
                    })
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The output columns of a subquery's plan whose values come from a set-returning or a
/// volatile call (through its projections, sort and grouping): clauses reading them can't
/// go into it (check_output_expressions).
pub(crate) fn unsafe_outputs(p: &Plan) -> Vec<bool> {
    let bad = |e: &Expr, below: &[bool]| -> bool {
        let mut b = crate::sql::paths::expr_volatile_deep(e);
        e.walk(&mut |x| match x {
            Expr::Srf(..) => b = true,
            Expr::Col(j, _) if below.get(*j).copied().unwrap_or(false) => b = true,
            _ => {}
        });
        b
    };
    match p {
        Plan::Project { input, exprs } => {
            let below = unsafe_outputs(input);
            exprs.iter().map(|e| bad(e, &below)).collect()
        }
        // (The calls' results are columns past the input's.)
        Plan::ProjectSet { input, exprs, srfs } => {
            let mut below = unsafe_outputs(input);
            below.resize(width(input) + srfs.iter().map(|s| s.ncols.max(1)).sum::<usize>(), true);
            exprs.iter().map(|e| bad(e, &below)).collect()
        }
        Plan::Sort { input, .. } | Plan::Filter { input, .. } | Plan::Context { input, .. } => unsafe_outputs(input),
        Plan::Agg { input, groups, aggs, sets: None } => {
            let below = unsafe_outputs(input);
            groups.iter().map(|g| bad(g, &below)).chain(aggs.iter().map(|_| false)).collect()
        }
        p => vec![false; width(p)],
    }
}

/// A clause over a projection that can go below it: the columns it reads are plain
/// values there (no sub-select, set-returning, aggregate, window or volatile call).
/// contain_nonstrict_functions.
pub(crate) fn contain_nonstrict(e: &Expr) -> bool {
    let mut ns = false;
    e.walk(&mut |x| match x {
        Expr::Call(d, ..) if !d.strict => ns = true,
        Expr::Agg(..)
        | Expr::Window(..)
        | Expr::Grouping(..)
        | Expr::Srf(..)
        | Expr::IsDistinct(..)
        | Expr::NullIf(..)
        | Expr::And(_)
        | Expr::Or(_)
        | Expr::Subquery(_)
        | Expr::Case { .. }
        | Expr::CaseTest(_)
        | Expr::Array(..)
        | Expr::Row(..)
        | Expr::RowCompare(_)
        | Expr::Coalesce(..)
        | Expr::MinMax(..)
        | Expr::Xml(_)
        | Expr::IsNull(..)
        | Expr::BoolTest(..)
        | Expr::Assign { .. }
        | Expr::ArrayOp { .. }
        | Expr::InList(..)
        | Expr::Default(_)
        | Expr::Param(..) => ns = true,
        _ => {}
    });
    ns
}

fn projectable(p: &Expr, exprs: &[Expr]) -> bool {
    let phv = PHV_OUTPUTS.with(|f| f.get());
    p.columns().iter().all(|&c| {
        exprs.get(c).is_some_and(|e| {
            // (A PlaceHolderVar over the subquery's own columns is evaluated above its
            // scan; a strict expression of them needs none, pullup_replace_vars_callback.)
            if phv && !matches!(e, Expr::Col(..)) && !e.columns().is_empty() && contain_nonstrict(e) {
                return false;
            }
            let mut plain = !e.is_volatile();
            e.walk(&mut |x| {
                plain &= !matches!(x, Expr::Subquery(_) | Expr::Srf(..) | Expr::Agg(..) | Expr::Window(..))
            });
            plain
        })
    })
}

/// `col COLLATE c` where the column's collation is `c` already: the column
/// (applyRelabelType keeps no RelabelType that changes nothing).
fn same_collation_dropped(e: Expr, cols: &[crate::catalog::Column]) -> Expr {
    let mut any = false;
    e.walk(&mut |x| any |= matches!(x, Expr::Collate(..)));
    if !any {
        return e;
    }
    e.transform(&mut |x| {
        Ok(match x {
            Expr::Collate(inner, c, l) => match &*inner {
                Expr::Col(i, _)
                    if cols.get(*i).is_some_and(|col| {
                        col.collation.as_deref().or(col.ty.default_collation()) == Some(c.as_str())
                    }) =>
                {
                    *inner
                }
                _ => Expr::Collate(inner, c, l),
            },
            x => x,
        })
    })
    .unwrap()
}

/// A filter over an inheritance member's rows: a child's scan projected to the parent's
/// columns takes it whatever it is (volatile, or reading a subquery), as an appendrel
/// child's restrictions are the parent's translated (no projection in between).
fn add_member_filter(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    match plan {
        Plan::Project { input, exprs }
            if matches!(&*input, Plan::Scan(_))
                && exprs.iter().all(|e| matches!(e, Expr::Col(..) | Expr::Const(..))) =>
        {
            let inner = pred
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(i, _) => exprs[i].clone(),
                        e => e,
                    })
                })
                .unwrap();
            let input = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            Plan::Project {
                input: Arc::new(add_filter(cat, input, inner)),
                exprs,
            }
        }
        plan => add_filter(cat, plan, pred),
    }
}

/// A clause over a projection of window functions that can go below them: its columns
/// are input columns every window partitions by (check_output_expressions).
fn window_pushable(p: &Expr, exprs: &[Expr], window: &Plan) -> bool {
    let Plan::Window { input, calls } = window else {
        return false;
    };
    let iw = width(input);
    let cols = p.columns();
    let mut plain = !p.is_volatile() && !cols.is_empty();
    p.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..)));
    plain
        && cols.iter().all(|&c| match exprs.get(c) {
            Some(Expr::Col(j, _)) if *j < iw => calls.iter().all(|w| {
                w.partition_by.iter().any(|e| {
                    let mut e = e;
                    while let Expr::Relabel(x, _) = e {
                        e = x;
                    }
                    matches!(e, Expr::Col(k, _) if k == j)
                })
            }),
            _ => false,
        })
}

/// Equivalence classes with a constant across an inner join: `l = r` with `l = k` gives
/// `r = k` on the other side (and the other way), and the join clause, implied by the two,
/// goes (generate_base_implied_equalities_const).
#[allow(clippy::too_many_arguments)]
fn ec_constants(
    cat: &Catalog,
    lpush: &mut Vec<Expr>,
    rpush: &mut Vec<Expr>,
    hash: &mut Option<(Vec<Expr>, Vec<Expr>)>,
    cond: &mut Vec<Expr>,
    nleft: usize,
    (to_right, to_left): (bool, bool),
    kept: &mut Vec<usize>,
) {
    // `x = k`: (x, k, the clause).
    let const_eq = |e: &Expr| -> Option<(Expr, Expr)> {
        let Expr::Call(def, args, _) = e else {
            return None;
        };
        if def.name != "=" || args.len() != 2 || e.is_volatile() {
            return None;
        }
        // (A made type's `=` in no btree family isn't mergejoinable: no class.)
        if args
            .iter()
            .any(|a| matches!(a.ty(), DataType::Base(_)) && alias_family(&a.ty()).is_none())
        {
            return None;
        }
        // (Or an expression of them: an equivalence class member too, `COALESCE(b.x, a.x)`
        // equated with a constant above an outer join, reconsider_outer_join_clause.)
        let colish = |x: &Expr| match x {
            Expr::Col(..) => true,
            Expr::Cast(y, ..) | Expr::Relabel(y, _) => matches!(&**y, Expr::Col(..)),
            Expr::Coalesce(..) => {
                let mut plain = !x.columns().is_empty() && !x.is_volatile();
                x.walk(&mut |y| plain &= !matches!(y, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)));
                plain
            }
            _ => false,
        };
        if colish(&args[0]) && pseudo_constant(&args[1]) {
            Some((args[0].clone(), args[1].clone()))
        } else if colish(&args[1]) && pseudo_constant(&args[0]) {
            Some((args[1].clone(), args[0].clone()))
        } else {
            None
        }
    };
    // The equi-join pairs, the right side in the right input's columns.
    let mut pairs: Vec<(Expr, Expr, Option<usize>)> = Vec::new();
    if let Some((hl, hr)) = hash.as_ref() {
        for (i, (a, b)) in hl.iter().zip(hr).enumerate() {
            pairs.push((a.clone(), b.clone(), Some(i)));
        }
    }
    let mut cond_pairs = Vec::new();
    // (Whether a join clause names its right side's column first.)
    let mut right_first = Vec::new();
    for (i, c) in cond.iter().enumerate() {
        if let Some((a, b)) = ec_pair(c, nleft) {
            cond_pairs.push(i);
            right_first.push((
                pairs.len(),
                matches!(c, Expr::Call(_, args, _) if args[0].same_as(&b)),
            ));
            pairs.push((a, b.shift_cols(-(nleft as isize)), None));
        }
    }
    // (Into an outer join's nullable side not an outer query's value: in Postgres a
    // sublink reading it was pulled up into a join before equivalence classes form.)
    let outer_join = !(to_right && to_left);
    // (Under row-level security the statement's quals sit above a security level: one that
    // isn't leakproof is no equivalence class member (process_equivalence).)
    let rls = QUERY_RLS.with(|f| f.get());
    let lconst: Vec<(Expr, Expr, Expr)> = lpush
        .iter()
        .filter(|_| to_right)
        .filter(|e| !(rls && leaks(cat, e)))
        .filter_map(|e| const_eq(e).map(|(x, k)| (x, k, e.clone())))
        .filter(|(_, k, _)| !(outer_join && k.has_outer_refs()))
        .collect();
    let rconst: Vec<(Expr, Expr, Expr)> = rpush
        .iter()
        .filter(|_| to_left)
        .filter(|e| !(rls && leaks(cat, e)))
        .filter_map(|e| const_eq(e).map(|(x, k)| (x, k, e.clone())))
        .filter(|(_, k, _)| !(outer_join && k.has_outer_refs()))
        .collect();
    let mut drop_hash = Vec::new();
    let mut drop_cond = false;
    let derive = |clause: &Expr, from: &Expr, to: &Expr| -> Expr {
        clause
            .clone()
            .transform(&mut |e| Ok(if e.same_as(from) { to.clone() } else { e }))
            .unwrap_or_else(|_| clause.clone())
    };
    // (A made type's `=` with the constant's type must exist in the family: else the
    // class is broken and the join clause stays.)
    let derivable = |target: &Expr, k: &Expr| {
        let (tt, kt) = (target.ty(), k.ty());
        if alias_family(&tt).is_none() && alias_family(&kt).is_none() {
            return true;
        }
        let (to, ko) = (tt.oid(), kt.oid());
        cat.opmembers
            .values()
            .any(|m| !m.func && m.number == 3 && m.left == to && m.right == ko)
    };
    let mut used_pairs: Vec<(Expr, Expr)> = Vec::new();
    for (n, (a, b, hi)) in pairs.iter().enumerate() {
        let mut used = false;
        // (The join clause goes when both columns compare with the constant.)
        for (x, k, clause) in &lconst {
            if x.same_as(a) && derivable(b, k) {
                let d = derive(clause, a, b);
                if !rpush.iter().any(|e| e.same_as(&d)) {
                    rpush.push(d);
                }
                used |= derivable(a, k);
            } else if x.same_as(a)
                && derivable(a, k)
                && !right_first.iter().any(|&(p, r)| p == n && r)
            {
                // A broken class: the constant's own clause comes back besides the one
                // derived for its column (generate_base_implied_equalities_broken).
                lpush.push(clause.clone());
            }
        }
        for (x, k, clause) in &rconst {
            if x.same_as(b) && derivable(a, k) {
                let d = derive(clause, b, a);
                if !lpush.iter().any(|e| e.same_as(&d)) {
                    lpush.push(d);
                }
                used |= derivable(b, k);
            } else if x.same_as(b)
                && derivable(b, k)
                && right_first.iter().any(|&(p, r)| p == n && r)
            {
                rpush.push(clause.clone());
            }
        }
        if used {
            used_pairs.push((a.clone(), b.clone()));
            // (Postgres still carries the columns up to the join.)
            for c in a.columns() {
                if !kept.contains(&c) {
                    kept.push(c);
                }
            }
            for c in b.columns() {
                if !kept.contains(&(c + nleft)) {
                    kept.push(c + nleft);
                }
            }
            match hi {
                Some(i) => drop_hash.push(*i),
                None => {
                    let _ = n;
                    drop_cond = true;
                }
            }
        }
    }
    if !drop_hash.is_empty()
        && let Some((hl, hr)) = hash.as_mut()
    {
        let keep: Vec<bool> = (0..hl.len()).map(|i| !drop_hash.contains(&i)).collect();
        let mut k = keep.iter();
        hl.retain(|_| *k.next().unwrap());
        let mut k = keep.iter();
        hr.retain(|_| *k.next().unwrap());
        if hl.is_empty() {
            *hash = None;
        }
    }
    if drop_cond {
        // The implied equalities among the join clauses.
        let mut i = 0;
        cond.retain(|c| {
            let keep = !cond_pairs.contains(&i) || {
                let (a, b) = ec_pair(c, nleft).unwrap();
                let b = b.shift_cols(-(nleft as isize));
                !used_pairs.iter().any(|(x, y)| x.same_as(&a) && y.same_as(&b))
            };
            i += 1;
            keep
        });
    }
}

/// Every table scan reachable through filters, projections and inheritance appends.
pub fn scans_mut(p: &mut Plan, f: &mut dyn FnMut(&mut ScanPlan)) {
    match p {
        Plan::Scan(s) => f(s),
        Plan::Filter { input, .. } | Plan::Project { input, .. } => {
            scans_mut(Arc::make_mut(input), f)
        }
        Plan::Append {
            inputs,
            inh: Some(_),
        } => inputs
            .iter_mut()
            .for_each(|i| scans_mut(Arc::make_mut(i), f)),
        _ => {}
    }
}

/// A mergejoinable equality that becomes an equivalence class (`col = const`, `a = b`).
/// generate_base_implied_equalities_const over one table's quals: columns equated with
/// each other and with one constant are each compared with the constant instead (in the
/// order the class met them), the column = column clauses dropped.
fn ec_scan_constants(all: &mut Vec<Expr>) {
    // (column, column) and (column, constant) equalities of one type.
    let pair = |e: &Expr| -> Option<(usize, Result<usize, Expr>, Arc<crate::sql::func::FuncDef>)> {
        let Expr::Call(def, args, _) = e else {
            return None;
        };
        // (Both of one btree family: the same type, the integer or float types, or made
        // types added to their family.)
        let fam = |t: &DataType| match t {
            DataType::Int2 | DataType::Int4 | DataType::Int8 => DataType::Int8,
            DataType::Float4 | DataType::Float8 => DataType::Float8,
            t => alias_family(t).unwrap_or_else(|| t.without_typmod()),
        };
        if !ec_clause(e)
            || !def.strict
            || def.coll.as_ref().is_some_and(|c| !c.deterministic)
            || fam(&args[0].ty()) != fam(&args[1].ty())
        {
            return None;
        }
        match (&args[0], &args[1]) {
            (Expr::Col(a, _), Expr::Col(b, _)) if a != b => Some((*a, Ok(*b), def.clone())),
            (Expr::Col(a, _), c @ Expr::Const(v, _)) | (c @ Expr::Const(v, _), Expr::Col(a, _))
                if !v.is_null() =>
            {
                Some((*a, Err(c.clone()), def.clone()))
            }
            // (Any expression without columns is a constant member of its class.)
            (Expr::Col(a, _), c) | (c, Expr::Col(a, _))
                if !matches!(c, Expr::Col(..) | Expr::Const(..))
                    && pseudo_constant(c)
                    && !c.has_outer_refs()
                    && !matches!(c, Expr::Subquery(_)) =>
            {
                Some((*a, Err(c.clone()), def.clone()))
            }
            _ => None,
        }
    };
    let found: Vec<Option<_>> = all.iter().map(pair).collect();
    if found.iter().flatten().all(|(_, b, _)| b.is_err()) {
        // (Constants only: a class needs a column two clauses equate to one constant.)
        let cols: Vec<usize> = found.iter().flatten().map(|(a, _, _)| *a).collect();
        if !cols.iter().enumerate().any(|(i, a)| cols[..i].contains(a)) {
            return;
        }
    }
    let mut parent: std::collections::HashMap<usize, usize> = Default::default();
    fn root(p: &mut std::collections::HashMap<usize, usize>, x: usize) -> usize {
        let mut r = x;
        while let Some(&n) = p.get(&r) {
            if n == r {
                break;
            }
            r = n;
        }
        p.insert(x, r);
        r
    }
    let mut order = Vec::new();
    for (a, b, _) in found.iter().flatten() {
        for c in std::iter::once(*a).chain(b.as_ref().ok().copied()) {
            if !order.contains(&c) {
                order.push(c);
                parent.insert(c, c);
            }
        }
        if let Ok(b) = b {
            let (ra, rb) = (root(&mut parent, *a), root(&mut parent, *b));
            if ra != rb {
                parent.insert(rb, ra);
            }
        }
    }
    let mut derived = Vec::new();
    let mut drop = vec![false; all.len()];
    let roots: Vec<usize> = order.iter().map(|&c| root(&mut parent, c)).collect();
    let mut done = Vec::new();
    for &r in &roots {
        if done.contains(&r) {
            continue;
        }
        done.push(r);
        let members: Vec<usize> = order
            .iter()
            .zip(&roots)
            .filter(|(_, x)| **x == r)
            .map(|(c, _)| *c)
            .collect();
        let in_class = |a: usize| members.contains(&a);
        let mut konst: Option<(Expr, Arc<crate::sql::func::FuncDef>)> = None;
        let mut single = true;
        for (a, b, d) in found.iter().flatten() {
            if let Err(c) = b
                && in_class(*a)
            {
                match &konst {
                    None => konst = Some((c.clone(), d.clone())),
                    Some((k, _)) if k.same_as(c) => {}
                    Some(_) => single = false,
                }
            }
        }
        // (One column equated to the constant by several clauses is a class too: one
        // `column = constant` comes back for them.)
        let sources = found
            .iter()
            .flatten()
            .filter(|(a, _, _)| in_class(*a))
            .count();
        let Some((k, _)) = konst.filter(|_| single && (members.len() > 1 || sources > 1)) else {
            continue;
        };
        // Each member's `=` with the constant's type, as some clause here has it.
        let kt = k.ty().without_typmod();
        let types: Vec<DataType> = members
            .iter()
            .map(|m| {
                found
                    .iter()
                    .zip(all.iter())
                    .find_map(|(f, e)| match (f, e) {
                        (Some(_), Expr::Call(_, args, _)) => args.iter().find_map(|a| match a {
                            Expr::Col(c, t) if c == m => Some(t.clone()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .unwrap_or(DataType::Unknown)
            })
            .collect();
        let op_for = |mt: &DataType| -> Option<Arc<crate::sql::func::FuncDef>> {
            all.iter().zip(&found).find_map(|(e, f)| match (e, f) {
                (Expr::Call(d, args, _), Some(_))
                    if args[0].ty().without_typmod() == mt.without_typmod()
                        && args[1].ty().without_typmod() == kt =>
                {
                    Some(d.clone())
                }
                _ => None,
            })
        };
        let defs: Option<Vec<_>> = types.iter().map(op_for).collect();
        let Some(defs) = defs else {
            continue;
        };
        for (i, f) in found.iter().enumerate() {
            if let Some((a, _, _)) = f
                && in_class(*a)
            {
                drop[i] = true;
            }
        }
        for ((m, t), def) in members.into_iter().zip(types).zip(defs) {
            derived.push(Expr::Call(
                def,
                vec![Expr::Col(m, t), k.clone()],
                crate::types::DataType::Bool,
            ));
        }
    }
    if derived.is_empty() {
        return;
    }
    let mut i = 0;
    all.retain(|_| {
        i += 1;
        !drop[i - 1]
    });
    all.extend(derived);
}

fn ec_clause(e: &Expr) -> bool {
    let Expr::Call(def, args, _) = e else {
        return false;
    };
    if def.name != "=" || args.len() != 2 || e.is_volatile() {
        return false;
    }
    let sortable = |t: &crate::types::DataType| {
        use crate::types::DataType as T;
        !matches!(
            t,
            T::Json
                | T::Xml
                | T::Point
                | T::Lseg
                | T::Path
                | T::GBox
                | T::Polygon
                | T::Line
                | T::Circle
                | T::Xid
                | T::Cid
                | T::Aclitem
                | T::Unknown
        )
    };
    sortable(&args[0].ty())
        && sortable(&args[1].ty())
        && !(args[0].columns().is_empty() && args[1].columns().is_empty())
}

/// Restriction clauses in the order a scan checks them (order_qual_clauses): cheapest
/// first, equalities after the other quals of their cost; stable.
pub fn order_quals(parts: &mut [Expr]) {
    parts.sort_by(|a, b| {
        qual_cost(a)
            .total_cmp(&qual_cost(b))
            .then(ec_clause(a).cmp(&ec_clause(b)))
    });
}

/// Estimated evaluation cost of a qual in operator calls (cost_qual_eval): calls and
/// comparisons cost one, `= ANY (array)` half per element, a sub-select a lot; column
/// references, constants and IS NULL tests are free.
fn qual_cost(e: &Expr) -> f64 {
    let mut cost = 0.0;
    e.walk(&mut |x| match x {
        Expr::Call(..) | Expr::IsDistinct(..) | Expr::NullIf(..) => cost += 1.0,
        Expr::ArrayOp { right, .. } => {
            cost += match &**right {
                Expr::Const(Value::Array(a), _) => 0.5 * a.items.len().max(1) as f64,
                _ => 5.0,
            }
        }
        // (One item: an `=`, as the parser makes it.)
        Expr::InList(_, items, _) if items.len() == 1 => cost += 1.0,
        Expr::InList(_, items, _) => cost += 0.5 * items.len() as f64,
        // (Between text types it is a relabeling.)
        Expr::Cast(_, from, to, _)
            if from.without_typmod() != to.without_typmod()
                && !(from.is_string() && to.is_string()) =>
        {
            cost += 1.0
        }
        // (An uncorrelated `= ANY` over hashable values is a hashed SubPlan: per row, its
        // comparisons (cost_subplan with useHashTable; the table is built once).)
        Expr::Subquery(s)
            if s.kind == crate::sql::expr::SubqueryKind::Any
                && s.params.is_empty()
                && s.test_op.is_none()
                && !s.left.is_empty()
                && s.ops.iter().all(|o| o.name == "=")
                && s.left.iter().all(|l| hashable(&l.ty()))
                && !crate::exec::reads_enclosing(&s.plan, 0) =>
        {
            cost += s.ops.len().max(1) as f64
        }
        Expr::Subquery(_) => cost += 1000.0,
        _ => {}
    });
    cost
}

#[derive(PartialEq, Eq)]
enum Side {
    None,
    Left,
    Right,
    Both,
}

fn side(e: &Expr, nleft: usize) -> Side {
    let cols = e.columns();
    let l = cols.iter().any(|c| *c < nleft);
    let r = cols.iter().any(|c| *c >= nleft);
    match (l, r) {
        (false, false) => Side::None,
        (true, false) => Side::Left,
        (false, true) => Side::Right,
        (true, true) => Side::Both,
    }
}

/// `COALESCE(l, r) = c` (either way round) with `l = r` the full join's `i`th key (`l` over
/// its left side, `r` over its right) and `c` a non-null constant of their type: `i`, `c`
/// and the `=`.
fn full_key_const(
    p: &Expr,
    lk: &[Expr],
    rk: &[Expr],
    nleft: usize,
) -> Option<(usize, Expr, Arc<crate::sql::func::FuncDef>)> {
    let Expr::Call(def, args, _) = p else { return None };
    if def.name != "=" || args.len() != 2 {
        return None;
    }
    let (co, c) = match (&args[0], &args[1]) {
        (co @ Expr::Coalesce(..), c @ Expr::Const(v, _)) | (c @ Expr::Const(v, _), co @ Expr::Coalesce(..)) if !v.is_null() => {
            (co, c)
        }
        _ => return None,
    };
    let Expr::Coalesce(items, ty) = co else { return None };
    let [x, y] = items.as_slice() else { return None };
    let i = (0..lk.len()).find(|&i| {
        x.same_as(&lk[i]) && y.clone().shift_cols(-(nleft as isize)).same_as(&rk[i]) && y.columns().iter().all(|&k| k >= nleft)
    })?;
    if lk[i].ty() != *ty || rk[i].ty() != *ty || c.ty() != *ty || args[0].ty() != args[1].ty() {
        return None;
    }
    Some((i, c.clone(), def.clone()))
}

/// The column ranges of a join tree's relations (its leaves), from `off`.
fn leaf_ranges(p: &Plan, off: usize) -> Vec<(usize, usize)> {
    match p {
        Plan::Join { kind, left, right, nleft, .. } if !matches!(kind, JoinKind::Semi | JoinKind::Anti) => {
            let mut v = leaf_ranges(left, off);
            v.extend(leaf_ranges(right, off + nleft));
            v
        }
        Plan::Filter { input, .. } if matches!(&**input, Plan::Join { .. }) => leaf_ranges(input, off),
        p => vec![(off, off + width(p))],
    }
}

/// Inside a LATERAL right side, Outer(1, _) means the join's params, so predicates that read
/// the enclosing query can't move there.
fn right_pushable(e: &Expr, params: &[Expr]) -> bool {
    params.is_empty() || !e.has_outer_refs()
}

/// restriction_is_always_true / restriction_is_always_false over a list of conjuncts: a
/// NullTest of a non-nullable column (`nonnull`), or an OR with such a NullTest arm, is
/// always true (dropped); an IS NULL of one, or an OR of nothing else, is always false.
fn reduce_null_tests(all: &mut Vec<Expr>, nonnull: &dyn Fn(&Expr) -> bool) {
    let always = |q: &Expr, not: bool| matches!(q, Expr::IsNull(x, n) if *n == not && nonnull(x));
    all.retain(|q| match q {
        Expr::Or(arms) => !arms.iter().any(|a| always(a, true)),
        q => !always(q, true),
    });
    for q in all.iter_mut() {
        let falsy = match &*q {
            Expr::Or(arms) => !arms.is_empty() && arms.iter().all(|a| always(a, false)),
            other => always(other, false),
        };
        if falsy {
            *q = Expr::Const(Value::Bool(false), DataType::Bool);
        }
    }
}

/// Column `col` of `plan`'s output is a NOT NULL table column no outer join below nulls.
fn nonnullable_col(plan: &Plan, col: usize) -> bool {
    match plan {
        Plan::Scan(s) => s.table.columns.get(col).is_some_and(|c| c.not_null),
        Plan::Filter { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. } => nonnullable_col(input, col),
        Plan::Project { input, exprs } => {
            matches!(exprs.get(col), Some(Expr::Col(c, _)) if nonnullable_col(input, *c))
        }
        Plan::Join {
            kind,
            left,
            right,
            nleft,
            ..
        } => {
            if col < *nleft {
                !matches!(kind, JoinKind::Right | JoinKind::Full) && nonnullable_col(left, col)
            } else {
                matches!(kind, JoinKind::Inner | JoinKind::Right)
                    && nonnullable_col(right, col - nleft)
            }
        }
        _ => false,
    }
}

/// Build a join, pushing single-side ON conditions down and picking hash keys.
#[allow(clippy::too_many_arguments)]
pub fn join(
    cat: &Catalog,
    kind: JoinKind,
    left: Plan,
    right: Plan,
    cond: Option<Expr>,
    params: Vec<Expr>,
    nleft: usize,
    nright: usize,
) -> Plan {
    let mut parts = Vec::new();
    if let Some(c) = cond {
        conjuncts(c, &mut parts);
    }
    if params.is_empty() {
        let nonnull = |x: &Expr| match x {
            Expr::Col(c, _) if *c < nleft => nonnullable_col(&left, *c),
            Expr::Col(c, _) => nonnullable_col(&right, *c - nleft),
            _ => false,
        };
        reduce_null_tests(&mut parts, &nonnull);
    }
    let (mut left, mut right) = (left, right);
    let mut rest = Vec::new();
    let mut lpush = Vec::new();
    let mut rpush = Vec::new();
    for p in parts {
        if p.is_volatile() && p.columns().is_empty() {
            rest.push(p);
            continue;
        }
        match side(&p, nleft) {
            // ON conditions on the preserved side of an outer join filter nothing.
            Side::Left if matches!(kind, JoinKind::Inner | JoinKind::Right) => lpush.push(p),
            Side::Right
                if matches!(
                    kind,
                    JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti
                ) && right_pushable(&p, &params) =>
            {
                rpush.push(p.shift_cols(-(nleft as isize)))
            }
            _ => rest.push(p),
        }
    }
    // A LATERAL right side reads the left row anyway: the join clauses over both sides go
    // into it (its parameterized path's clauses), reading the left side's columns as
    // params too.
    let mut params = params;
    if !params.is_empty() && matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti) {
        // (A subquery level, not pulled up, takes only those over the relations its
        // LATERAL references read: its parameterized path's clauses. The others are the
        // join's.)
        let sources: Option<Vec<(usize, usize)>> = matches!(right, Plan::Barrier { .. }).then(|| {
            let read: Vec<usize> = params.iter().flat_map(|e| e.columns()).collect();
            leaf_ranges(&left, 0)
                .into_iter()
                .filter(|(a, b)| read.iter().any(|c| c >= a && c < b))
                .collect()
        });
        let mut stay = Vec::new();
        for p in std::mem::take(&mut rest) {
            if p.is_volatile() || p.has_outer_refs() || side(&p, nleft) != Side::Both {
                stay.push(p);
                continue;
            }
            if let Some(src) = &sources
                && p.columns().iter().any(|&c| c < nleft && !src.iter().any(|(a, b)| c >= *a && c < *b))
            {
                stay.push(p);
                continue;
            }
            let moved = p
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(i, t) if i < nleft => {
                            let k = match params
                                .iter()
                                .position(|q| matches!(q, Expr::Col(j, _) if *j == i))
                            {
                                Some(k) => k,
                                None => {
                                    params.push(Expr::Col(i, t.clone()));
                                    params.len() - 1
                                }
                            };
                            Expr::Outer(1, k, t)
                        }
                        Expr::Col(i, t) => Expr::Col(i - nleft, t),
                        x => x,
                    })
                })
                .expect("moving a join clause");
            rpush.push(moved);
        }
        rest = stay;
    }
    // An inner join's ON conditions are WHERE clauses all the same: equivalence classes
    // give each side the constants the other side's are equated with.
    let mut kept: Vec<usize> = Vec::new();
    if kind == JoinKind::Inner && params.is_empty() {
        let mut no_hash = None;
        ec_constants(cat, &mut lpush, &mut rpush, &mut no_hash, &mut rest, nleft, (true, true), &mut kept);
    }
    // (An outer join's ON conditions pushed into its nullable side: their columns.)
    let mut pushed: Vec<usize> = Vec::new();
    if kind == JoinKind::Right {
        lpush.iter().for_each(|e| pushed.extend(e.columns()));
    } else if kind != JoinKind::Inner {
        rpush.iter().for_each(|e| pushed.extend(e.columns().into_iter().map(|c| c + nleft)));
    }
    pushed.sort_unstable();
    pushed.dedup();
    if !lpush.is_empty() {
        left = add_filter(cat, left, Expr::And(lpush));
    }
    if !rpush.is_empty() {
        right = add_filter(cat, right, Expr::And(rpush));
    }
    // Hash keys: `l = r` with each side reading only one input, same type.
    let mut hash: Option<(Vec<Expr>, Vec<Expr>)> = None;
    let mut residual = Vec::new();
    let mut hash_swapped = Vec::new();
    if params.is_empty() {
        let (mut lk, mut rk) = (Vec::new(), Vec::new());
        for p in rest {
            match equi_key(&p, nleft) {
                Some((l, r)) => {
                    hash_swapped.push(
                        matches!(&p, Expr::Call(_, a, _) if side(&a[0], nleft) == Side::Right),
                    );
                    lk.push(l);
                    rk.push(r.shift_cols(-(nleft as isize)));
                }
                None => residual.push(p),
            }
        }
        if !lk.is_empty() {
            hash = Some((lk, rk));
        }
    } else {
        residual = rest;
    }
    Plan::Join {
        kind,
        left: Arc::new(left),
        right: Arc::new(right),
        cond: and_all(residual),
        hash,
        params,
        nleft,
        nright,
        meta: Box::new(crate::sql::plan::JoinMeta {
            pushed,
            hash_swapped,
            kept,
            ..Default::default()
        }),
    }
}

/// A join clause an equivalence class takes: an equi-join key, or an `=` between the two
/// sides' columns of a made type and the built-in family it was added to (not hashable).
fn ec_pair(e: &Expr, nleft: usize) -> Option<(Expr, Expr)> {
    if let Some(k) = equi_key(e, nleft) {
        return Some(k);
    }
    let Expr::Call(def, args, _) = e else {
        return None;
    };
    if def.name != "=" || args.len() != 2 || !def.strict || e.is_volatile() {
        return None;
    }
    let fam = |t: &DataType| match t {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => Some(DataType::Int8),
        DataType::Float4 | DataType::Float8 => Some(DataType::Float8),
        t => alias_family(t),
    };
    let (ta, tb) = (args[0].ty(), args[1].ty());
    if alias_family(&ta).is_none() && alias_family(&tb).is_none() {
        return None;
    }
    if fam(&ta).is_none() || fam(&ta) != fam(&tb) {
        return None;
    }
    match (side(&args[0], nleft), side(&args[1], nleft)) {
        (Side::Left, Side::Right) => Some((args[0].clone(), args[1].clone())),
        (Side::Right, Side::Left) => Some((args[1].clone(), args[0].clone())),
        _ => None,
    }
}

fn equi_key(e: &Expr, nleft: usize) -> Option<(Expr, Expr)> {
    let Expr::Call(def, args, _) = e else {
        return None;
    };
    if def.name != "=" || args.len() != 2 {
        return None;
    }
    // Under a nondeterministic collation, equal strings needn't be equal bytes (the
    // planner's hash keys hash the collation's sort keys).
    if def.coll.as_ref().is_some_and(|c| !c.deterministic) {
        return None;
    }
    let (a, b) = (&args[0], &args[1]);
    let (ta, tb) = (a.ty(), b.ty());
    // (integer_ops hashes int2, int4 and int8 alike: a cross-type `=` hashes too.)
    let int = |t: &DataType| matches!(t, DataType::Int2 | DataType::Int4 | DataType::Int8);
    if !hashable(&ta) || (ta.without_typmod() != tb.without_typmod() && !(int(&ta) && int(&tb))) {
        return None;
    }
    // Keys without a collation fail as they're hashed.
    let key = |k: &Expr| super::expr::hash_key(e, k.clone());
    match (side(a, nleft), side(b, nleft)) {
        (Side::Left, Side::Right) => Some((key(a), key(b))),
        (Side::Right, Side::Left) => Some((key(b), key(a))),
        _ => None,
    }
}

/// Types a hash join can use (hashing agrees with their `=`).
pub(crate) fn hashable_type(t: &DataType) -> bool {
    hashable(t)
}

/// Types whose `=` is exactly equality of the stored value (so hashing agrees with it).
fn hashable(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Bool
            | DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Text
            | DataType::Varchar(_)
            | DataType::Name
            | DataType::Char
            | DataType::Bytea
            | DataType::Date
            | DataType::Timestamp(_)
            | DataType::Timestamptz(_)
            | DataType::Time(_)
            | DataType::Uuid
            | DataType::Oid
            | DataType::Regclass
            | DataType::Regtype
            | DataType::Enum(..)
            // (Values compare and hash by value: 1.0 and 1.00, -0.0 and 0.0 alike.)
            | DataType::Numeric(_)
            | DataType::Float4
            | DataType::Float8
            | DataType::Interval(_)
            | DataType::Tid
            | DataType::PgLsn
            | DataType::Xid8
            | DataType::Macaddr
            | DataType::Macaddr8
    )
}

thread_local! {
    /// Made base types with operators added to a built-in btree family (ALTER OPERATOR
    /// FAMILY integer_ops ADD ...): the family's type, by the made type's oid.
    static ALIAS_FAMILIES: std::cell::RefCell<Vec<(u32, DataType)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Note the made types `cat` puts into built-in btree operator families (for
/// `alias_family`).
pub(crate) fn note_alias_families(cat: &Catalog) {
    let mut v: Vec<(u32, DataType)> = Vec::new();
    for m in cat.opmembers.values() {
        if m.func {
            continue;
        }
        let fam = match m.family {
            1976 => DataType::Int8,
            1970 => DataType::Float8,
            _ => continue,
        };
        for t in [m.left, m.right] {
            if !matches!(t, 20 | 21 | 23 | 700 | 701) && !v.iter().any(|(o, _)| *o == t) {
                v.push((t, fam.clone()));
            }
        }
    }
    ALIAS_FAMILIES.with(|a| *a.borrow_mut() = v);
}

/// The built-in btree family a made base type's operators were added to (its type).
pub(crate) fn alias_family(t: &DataType) -> Option<DataType> {
    let DataType::Base(b) = t else {
        return None;
    };
    ALIAS_FAMILIES.with(|a| a.borrow().iter().find(|(o, _)| *o == b.oid).map(|(_, f)| f.clone()))
}

/// Whether a value of type `v` can probe an index on a column of type `c` (same key
/// encoding and the same ordering).
pub(crate) fn probe_ok(c: &DataType, v: &DataType) -> bool {
    use DataType as T;
    // (A made type in the column's integer / float family probes as that family's.)
    if let Some(f) = alias_family(v) {
        let int = |t: &DataType| matches!(t, T::Int2 | T::Int4 | T::Int8);
        let float = |t: &DataType| matches!(t, T::Float4 | T::Float8);
        return match f {
            T::Int8 => int(c) || alias_family(c) == Some(T::Int8),
            T::Float8 => float(c) || alias_family(c) == Some(T::Float8),
            _ => false,
        };
    }
    let int = |t: &DataType| matches!(t, T::Int2 | T::Int4 | T::Int8);
    let text = |t: &DataType| matches!(t, T::Text | T::Varchar(_) | T::Name);
    (int(c) && int(v))
        || (text(c) && text(v))
        || matches!(
            (c, v),
            (
                T::Oid | T::Regclass | T::Regtype,
                T::Oid | T::Regclass | T::Regtype
            )
        )
        || matches!(
            (c, v),
            (T::Date, T::Date) | (T::Bool, T::Bool) | (T::Uuid, T::Uuid) | (T::Bytea, T::Bytea)
        )
        || matches!(
            (c, v),
            (T::Timestamp(_), T::Timestamp(_))
                | (T::Timestamptz(_), T::Timestamptz(_))
                | (T::Time(_), T::Time(_))
        )
        || matches!((c, v), (T::Numeric(_), T::Numeric(_)))
}

/// A value fixed for the whole scan: no columns of the row, nothing volatile.
fn scan_constant(e: &Expr) -> bool {
    e.columns().is_empty() && !e.is_volatile() && !matches!(e, Expr::Default(_))
}

#[derive(Default)]
struct ColConds {
    eq: Option<Expr>,
    any: Option<Expr>,
    lower: Option<(Expr, bool)>,
    upper: Option<(Expr, bool)>,
}

fn col_conds(parts: &[Expr], col: usize, cty: &DataType) -> ColConds {
    let mut out = ColConds::default();
    let is_col = |e: &Expr| matches!(e, Expr::Col(i, _) if *i == col);
    for p in parts {
        match p {
            Expr::Call(def, args, _) if args.len() == 2 => {
                let (op, v) = if is_col(&args[0]) && scan_constant(&args[1]) {
                    (def.name, &args[1])
                } else if is_col(&args[1]) && scan_constant(&args[0]) {
                    let flipped = match def.name {
                        "<" => ">",
                        "<=" => ">=",
                        ">" => "<",
                        ">=" => "<=",
                        o => o,
                    };
                    (flipped, &args[0])
                } else {
                    continue;
                };
                if !probe_ok(cty, &v.ty()) {
                    continue;
                }
                match op {
                    "=" => out.eq = Some(v.clone()),
                    ">" => out.lower = Some((v.clone(), false)),
                    ">=" => out.lower = Some((v.clone(), true)),
                    "<" => out.upper = Some((v.clone(), false)),
                    "<=" => out.upper = Some((v.clone(), true)),
                    _ => {}
                }
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all: false,
            } if op.name == "=" && is_col(left) && scan_constant(right) => {
                if let Some(et) = right.ty().elem()
                    && probe_ok(cty, et)
                {
                    out.any = Some((**right).clone());
                }
            }
            _ => {}
        }
    }
    out
}

fn index_columns(cat: &Catalog, t: &Table) -> Vec<Arc<Index>> {
    let mut v = cat.table_indexes(t);
    if let Some(p) = t.pkey_index
        && !v.iter().any(|i| i.oid == p)
        && let Some(ix) = cat.index(p)
    {
        v.push(ix.clone());
    }
    v.retain(|i| {
        i.valid
            && i.predicate.is_none()
            && i.columns
                .iter()
                .all(|c| matches!(c.key, IndexKey::Column(_)))
    });
    v
}

/// Pick the best index for a scan's filter.
fn choose_index(cat: &Catalog, s: &mut ScanPlan) {
    let Some(f) = &s.filter else { return };
    if s.sample.is_some() {
        return;
    }
    let mut parts = Vec::new();
    conjuncts(f.clone(), &mut parts);
    let mut best: Option<(i32, Arc<Index>, IndexBounds)> = None;
    for ix in index_columns(cat, &s.table) {
        let mut b = IndexBounds::default();
        let mut score = 0;
        for c in &ix.columns {
            let IndexKey::Column(col) = c.key else { break };
            let cty = &s.table.columns[col].ty;
            let cc = col_conds(&parts, col, cty);
            if let Some(v) = cc.eq {
                b.eq.push(v);
                score += 10;
                continue;
            }
            if let Some(a) = cc.any {
                b.any = Some(a);
                score += 8;
            } else if cc.lower.is_some() || cc.upper.is_some() {
                score += 3 + cc.lower.is_some() as i32 + cc.upper.is_some() as i32;
                b.lower = cc.lower;
                b.upper = cc.upper;
            }
            break;
        }
        if score == 0 {
            continue;
        }
        if (ix.unique || ix.primary) && b.eq.len() == ix.columns.len() {
            score += 100;
        }
        if ix.primary {
            // No extra lookup per row.
            score += 1;
        }
        if best.as_ref().is_none_or(|(s0, _, _)| score > *s0) {
            best = Some((score, ix, b));
        }
    }
    if let Some((_, ix, b)) = best {
        s.index = Some((ix, b));
    }
}

/// Plain projection, skipped when it is the identity.
pub fn project(plan: Plan, exprs: Vec<Expr>) -> Plan {
    let w = width(&plan);
    if exprs.len() == w
        && exprs
            .iter()
            .enumerate()
            .all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
    {
        return plan;
    }
    Plan::Project {
        input: Arc::new(plan),
        exprs,
    }
}

/// `is_pseudo_constant_clause`: no columns of the query's own rows, nothing volatile.
pub fn pseudo_constant(e: &Expr) -> bool {
    // An uncorrelated subquery is an InitPlan's output parameter.
    let mut vars = false;
    e.walk(&mut |x| {
        vars |= match x {
            Expr::Col(..) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) => true,
            Expr::Call(d, ..) => d.volatility == 'v',
            Expr::Subquery(s) => s.func == Some('v'),
            _ => false,
        }
    });
    !vars
}

/// The output columns of `plan` a WHERE clause equates to a constant: an equivalence class
/// with a constant, so they order nothing (Postgres drops such sort keys).
pub fn fixed_columns(plan: &Plan) -> Vec<usize> {
    fn from_pred(e: &Expr, out: &mut Vec<usize>) {
        match e {
            Expr::And(list) => list.iter().for_each(|x| from_pred(x, out)),
            // (An equality under an explicit COLLATE is another equivalence class than the
            // column's ordering: it fixes nothing the sort sees.)
            Expr::Call(d, args, _)
                if d.name == "=" && args.len() == 2 && !args.iter().any(|a| matches!(a, Expr::Collate(..))) =>
            {
                let col = |x: &Expr| match x {
                    Expr::Col(i, _) => Some(*i),
                    Expr::Relabel(y, _) => match &**y {
                        Expr::Col(i, _) => Some(*i),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(i) = col(&args[0]).filter(|_| pseudo_constant(&args[1])) {
                    out.push(i);
                } else if let Some(i) = col(&args[1]).filter(|_| pseudo_constant(&args[0])) {
                    out.push(i);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    match plan {
        Plan::Filter { input, pred } => {
            // (A HAVING fixes nothing: no equivalence classes of its own.)
            if !matches!(&**input, Plan::Agg { .. }) {
                from_pred(pred, &mut out);
            }
            out.extend(fixed_columns(input));
        }
        Plan::Scan(s) => {
            if let Some(f) = &s.filter {
                from_pred(f, &mut out);
            }
        }
        Plan::Project { input, exprs } => {
            let inner = fixed_columns(input);
            for (j, e) in exprs.iter().enumerate() {
                // (Or an expression a filter below equates with a constant.)
                if matches!(e, Expr::Col(i, _) if inner.contains(i))
                    || (!matches!(e, Expr::Col(..)) && !e.columns().is_empty() && expr_fixed(input, e))
                {
                    out.push(j);
                }
            }
        }
        Plan::Barrier { input, .. } | Plan::Context { input, .. } => out = fixed_columns(input),
        _ => {}
    }
    out
}

/// Whether a filter in `plan` (through its projections) equates expression `e` (over
/// `plan`'s output) with a constant.
fn expr_fixed(plan: &Plan, e: &Expr) -> bool {
    match plan {
        Plan::Project { input, exprs } => {
            let Ok(inner) = e.clone().transform(&mut |x| {
                Ok(match x {
                    Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                    x => x,
                })
            }) else {
                return false;
            };
            expr_fixed(input, &inner)
        }
        Plan::Filter { input, pred } => {
            if !matches!(&**input, Plan::Agg { .. }) {
                let mut parts = Vec::new();
                conjuncts(pred.clone(), &mut parts);
                for q in parts {
                    if let Expr::Call(d, args, _) = &q
                        && d.name == "="
                        && args.len() == 2
                        && !args.iter().any(|a| matches!(a, Expr::Collate(..)))
                        && ((pseudo_constant(&args[1]) && args[0].same_as(e))
                            || (pseudo_constant(&args[0]) && args[1].same_as(e)))
                    {
                        return true;
                    }
                }
            }
            expr_fixed(input, e)
        }
        Plan::Barrier { input, .. } | Plan::Context { input, .. } => expr_fixed(input, e),
        _ => false,
    }
}

/// Sort (the planner may read an index in that order instead: `sql::paths`).
pub fn sort(cat: &Catalog, plan: Plan, keys: Vec<SortKey>) -> Plan {
    let _ = cat;
    Plan::Sort {
        input: Arc::new(plan),
        keys,
    }
}

/// LIMIT n without ORDER BY over a scan: stop the scan early.
pub fn push_limit(plan: &mut Plan, n: usize) {
    match plan {
        Plan::Project { input, .. } => {
            if let Some(p) = Arc::get_mut(input) {
                push_limit(p, n)
            }
        }
        Plan::Scan(s) => s.limit = Some(s.limit.map_or(n, |l| l.min(n))),
        _ => {}
    }
}
