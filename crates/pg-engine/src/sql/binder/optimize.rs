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
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => width(input),
        Plan::LockRows { width, .. } => *width,
        Plan::Foreign(f) => f.out.len(),
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
    /// A join's `column = constant` clauses of broken equivalence classes, pushed into its
    /// sides again (generate_base_implied_equalities_broken) while they are placed: a
    /// scan keeps both copies.
    static BROKEN_DUPS: std::cell::RefCell<Vec<Expr>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The PlaceHolderVars (column-free computed outputs) the quals being pushed down read.
    static PHV_PENDING: std::cell::RefCell<Vec<Expr>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The statement's row-level security quals as placed in their scans (security level
    /// 0): the quals of a security barrier view above them sit at a higher level.
    static LEVEL0_QUALS: std::cell::RefCell<Vec<Expr>> = const { std::cell::RefCell::new(Vec::new()) };
    /// `add_rls_filter` is adding a scan's row-level security quals.
    static RLS_ADDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Per-statement scratch back at rest (`scoped::reset_statement_scratch`).
pub(crate) fn reset_scratch() {
    PHV_PENDING.with(|p| p.borrow_mut().clear());
    SECURITY_QUALS.set(false);
    QUERY_RLS.set(false);
    PHV_OUTPUTS.set(false);
    RLS_ADDING.set(false);
    LEVEL0_QUALS.with(|l| l.borrow_mut().clear());
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    for (name, set) in [
        ("optimize::SECURITY_QUALS", SECURITY_QUALS.get()),
        ("optimize::QUERY_RLS", QUERY_RLS.get()),
        ("optimize::PHV_OUTPUTS", PHV_OUTPUTS.get()),
        ("optimize::PHV_PENDING", PHV_PENDING.with_borrow(|p| !p.is_empty())),
        ("optimize::RLS_ADDING", RLS_ADDING.get()),
        ("optimize::LEVEL0_QUALS", LEVEL0_QUALS.with_borrow(|p| !p.is_empty())),
    ] {
        if set {
            out.push(name);
        }
    }
}

/// `add_filter` with the projections' computed outputs as PlaceHolderVars (quals reading
/// them stay above the projection).
pub fn add_filter_phv(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    let _phv = crate::scoped::set(&PHV_OUTPUTS, true, "optimize::PHV_OUTPUTS");
    add_filter(cat, plan, pred)
}

/// Binding a statement: whether it has row-level security quals starts false, and its
/// security level 0 quals none, the previous values back when the guards drop.
pub fn begin_statement_rls() -> (crate::scoped::CellGuard<bool>, crate::scoped::RefGuard<Vec<Expr>>) {
    (
        crate::scoped::set(&QUERY_RLS, false, "optimize::QUERY_RLS.begin"),
        crate::scoped::set_ref(&LEVEL0_QUALS, Vec::new(), "optimize::LEVEL0_QUALS.begin"),
    )
}

pub fn mark_statement_rls() {
    QUERY_RLS.with(|f| f.set(true));
    crate::scoped::checkpoint("optimize::QUERY_RLS");
}


/// Add row-level security quals over `plan`: on a scan they go ahead of the quals already
/// there (the statement's own), as a SELECT's do (security level 0 vs 1).
pub fn add_security_filter(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    QUERY_RLS.with(|f| f.set(true));
    let _sq = crate::scoped::set(&SECURITY_QUALS, true, "optimize::SECURITY_QUALS");
    let _rls = crate::scoped::set(&RLS_ADDING, true, "optimize::RLS_ADDING");
    add_filter(cat, plan, pred)
}

/// Add a relation's row-level security quals over its scan (security level 0), where they
/// are the first quals.
pub fn add_rls_filter(cat: &Catalog, plan: Plan, pred: Expr) -> Plan {
    let _rls = crate::scoped::set(&RLS_ADDING, true, "optimize::RLS_ADDING");
    add_filter(cat, plan, pred)
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
    let cheap_proof = |e: &Expr, c: f64| c < 10.0 && !leaks(cat, e);
    // (Of `first`, the quals at security level 0: row-level security's, and cheap leakproof
    // ones, which order_qual_clauses moves there.)
    let rls_qual = |e: &Expr| LEVEL0_QUALS.with(|l| l.borrow().iter().any(|q| q.same_as(e)));
    // (Without row-level security quals there, the first level's are the quals there.)
    let any_rls = first.iter().any(rls_qual);
    let level0 = |e: &Expr, c: f64| !any_rls || cheap_proof(e, c) || rls_qual(e);
    // `first` keep their order; of `rest` (in their order), a cheap leakproof one moves ahead
    // of the costlier quals of `first` before it, and of those at a higher security level
    // (a security barrier view's).
    let mut out: Vec<(bool, f64, bool, Expr)> = first
        .into_iter()
        .map(|e| {
            let c = cost(&e);
            (true, c, level0(&e, c), e)
        })
        .collect();
    for e in rest {
        let c = cost(&e);
        let mut j = out.len();
        if cheap_proof(&e, c) {
            while j > 0 && out[j - 1].0 && (out[j - 1].1 > c || !out[j - 1].2) {
                j -= 1;
            }
        }
        out.insert(j, (false, c, false, e));
    }
    out.into_iter().map(|i| i.3).collect()
}

/// Add a filter over `plan`'s output, pushing it as far down as is safe.
/// eval_const_expressions' CoerceToDomain of a domain without constraints: a RelabelType,
/// which collapses into the implicit one an operator applies to its base type (the
/// operand is the value itself, `relname = 'x'::name`).
fn strip_free_domains(e: Expr) -> Expr {
    // (Nothing to do without a domain coercion: no rebuilt tree. Past the statement's stack
    // budget the next guard raises the depth error; this walk, which can't fail, leaves the
    // qual as it is.)
    let mut domains = false;
    e.walk(&mut |x| domains |= matches!(x, Expr::CoerceToDomain(..)));
    if !domains || crate::stack::check().is_err() {
        return e;
    }
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

/// The equalities between columns among `parts` (process_equivalence's candidates whose
/// members are relations' values), for `JoinMeta::ec_written`.
pub fn written_equalities(parts: &[Expr]) -> Vec<Expr> {
    written_equalities_with(parts, false)
}

/// `written_equalities`; `outer`: also those equating a column with a value of the
/// enclosing query (a sub-select's correlated WHERE clause, the semi join's own equality
/// once pull_up_sublinks makes one of it).
pub fn written_equalities_with(parts: &[Expr], outer: bool) -> Vec<Expr> {
    let side = |e: &Expr| !e.columns().is_empty() || outer && !e.is_volatile() && e.has_outer_refs();
    parts
        .iter()
        .filter(|e| matches!(e, Expr::Call(d, a, _) if d.name == "=" && a.len() == 2
            && side(&a[0]) && side(&a[1]) && !(a[0].columns().is_empty() && a[1].columns().is_empty())))
        .cloned()
        .collect()
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
            if RLS_ADDING.with(|f| f.get()) {
                LEVEL0_QUALS.with(|l| l.borrow_mut().extend(parts.iter().cloned()));
            }
            let mut all = Vec::new();
            if let Some(f) = s.filter.take() {
                conjuncts(f, &mut all);
            }
            let parts = phv_class_order(&all, parts);
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
            // (A pseudoconstant qual is tested once: all its cost is startup cost, none per
            // row (cost_qual_eval_walker), so it sorts first, in the order written.)
            let cost = |e: &Expr| {
                if pseudo_constant(e) {
                    0.0
                } else if rls {
                    qual_cost(e)
                } else {
                    full_qual_cost(cat, e)
                }
            };
            parts.sort_by(|a, b| {
                cost(a)
                    .total_cmp(&cost(b))
                    .then(ec_order_clause(a).cmp(&ec_order_clause(b)))
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
                merged.sort_by(|a, b| cost(a).total_cmp(&cost(b)).then(ec_order_clause(a).cmp(&ec_order_clause(b))));
                merged
            };
            let mut all = all;
            // restriction_is_always_true / _false: IS [NOT] NULL of a NOT NULL column.
            let not_null = |x: &Expr| {
                matches!(x, Expr::Col(c, _) if s.table.columns.get(*c).is_some_and(|col| col.not_null))
            };
            let qual_false = all.iter().find(|q| super::refute::always_false(q)).cloned();
            reduce_null_tests(&mut all, &not_null);
            ec_scan_constants(&mut all);
            // Constraint exclusion: the filter contradicts the table's constraints (or
            // itself, equating a column with two constants).
            // (A constant false qual: a dummy rel, its columns still named.)
            if all.iter().any(super::refute::always_false) {
                let names = scan_names(&s);
                let rels = vec![Some(scan_refname(&s)); names.len()];
                let table = Some(s.table.oid);
                let ncols = width(&Plan::Scan(s));
                return Plan::Empty {
                    ncols,
                    cols: Some(Arc::new(names)),
                    info: Some(Arc::new(crate::sql::plan::DummyInfo {
                        rels,
                        ec: Vec::new(),
                        table,
                        qual_false,
                        kept: super::fold::candidates_in(&all),
                        ..Default::default()
                    })),
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
                let table = Some(s.table.oid);
                let scan = Plan::Scan(s);
                let ec = crate::sql::paths::column_classes(&scan);
                let ncols = width(&scan);
                let cols = (names.len() == ncols).then(|| Arc::new(names));
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec, table, ..Default::default() }));
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
        Plan::SysScan { table, filter, alias, mark, .. } => {
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
                mark,
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
            let mut parts = parts;
            // (A FROM subquery's planned join kept as bound: the clauses go to the tree as
            // bound too, `JoinMeta::bound_pushed`.)
            let (old_bound, mut bound_pushed) = (meta.bound, meta.bound_pushed);
            let pushed_in = if old_bound != 0 { parts.clone() } else { Vec::new() };
            let places = if kind == JoinKind::Inner { ec_const_order(&mut parts, nleft) } else { Vec::new() };
            // (A left join's: the WHERE's conjuncts in their order, for the classes above it.)
            let all_parts = if kind == JoinKind::Left { parts.clone() } else { Vec::new() };
            for p in parts {
                // (A volatile clause goes to the relations it reads, as any other; one that
                // reads none is checked above.)
                if p.is_volatile() && p.columns().is_empty() {
                    above.push(p);
                    continue;
                }
                // (So is one that reads none at all: distribute_qual_to_rels gives a
                // pseudoconstant clause the whole join's scope, a gating qual of the join.)
                // (A constant false or NULL too: the join is dummy, its relations planned
                // all the same, their subqueries' constants simplified. Of an outer join
                // too: the top join domain's pseudoconstant gates the whole join, not its
                // outer side.)
                if p.columns().is_empty()
                    && (!matches!(p, Expr::Const(..)) || matches!(p, Expr::Const(Value::Bool(false) | Value::Null, _)))
                {
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
            // (An inner join's inputs' own restrictions are class members too; a LATERAL
            // right side's aren't looked into, its left side's are.)
            let (lhave, rhave) = match kind {
                JoinKind::Inner if params.is_empty() => (input_restrictions(&left), input_restrictions(&right)),
                JoinKind::Inner => (input_restrictions(&left), Vec::new()),
                _ => (Vec::new(), Vec::new()),
            };
            // (What the join was written as, and the PlaceHolderVars a projection over it
            // computes, stay with it.)
            let (old_wk, old_phv) = (meta.written_kind, (meta.phv_eval, meta.phv_join));
            let old_written = meta.ec_written;
            let (lcoll, rcoll) = (|c: usize| output_collation(&left, c), |c: usize| output_collation(&right, c));
            let mut rewrites = Vec::new();
            // (A LATERAL subquery left for the group to pull up, or the join tree of one:
            // still so.)
            let old_lat = (
                meta.lateral_sub,
                !params.is_empty() && (meta.lateral_held || crate::sql::plan::deferred_lateral(&right)),
                meta.lateral_flat,
            );
            let broken = ec_constants(
                cat,
                &mut lpush,
                &mut rpush,
                &mut hash,
                &mut cond_parts,
                nleft,
                (to_right, to_left),
                &mut kept,
                (&lhave, &rhave),
                (&lcoll, &rcoll),
                &mut rewrites,
            );
            // (A left join's key a constant replaced, reconsider_outer_join_clause's
            // constant TRUE: its nullable side's columns are an ON clause's still, which
            // reduce_outer_joins, run on the clauses as written, sees strict.)
            let mut const_keyed: Vec<usize> = Vec::new();
            if kind == JoinKind::Left
                && let Some((ol, or)) = &old_keys
            {
                for (a, b) in ol.iter().zip(or) {
                    let still = hash
                        .as_ref()
                        .is_some_and(|(hl, hr)| hl.iter().zip(hr).any(|(x, y)| x.same_as(a) && y.same_as(b)));
                    if !still {
                        const_keyed.extend(b.columns().into_iter().map(|c| c + nleft));
                    }
                }
            }
            // (The constants' clauses derived for the other side too, in class order.)
            order_by_places(&mut lpush, &places, 0);
            order_by_places(&mut rpush, &places, nleft);
            let saved = BROKEN_DUPS.with(|b| b.replace(broken));
            let mut l = Arc::try_unwrap(left).unwrap_or_else(|a| (*a).clone());
            let mut r = Arc::try_unwrap(right).unwrap_or_else(|a| (*a).clone());
            // (A restriction replaced inside: the tree as bound is no longer the planned one.)
            let rewritten = !rewrites.is_empty();
            for (on_right, old, new) in rewrites {
                replace_restriction(cat, if on_right { &mut r } else { &mut l }, &old, &new);
            }
            if !lpush.is_empty() {
                l = add_filter(cat, l, Expr::And(lpush));
            }
            if !rpush.is_empty() {
                let nonnull = nonnull_params(&l, &params);
                r = push_lateral(cat, r, rpush, &params, &nonnull);
            }
            BROKEN_DUPS.with(|b| *b.borrow_mut() = saved);
            let mut j = join_lateral(cat, kind, l, r, and_all(cond_parts), params, nleft, nright, old_lat.1);
            if let Plan::Join { meta, .. } = &mut j {
                meta.kept = kept;
                meta.written_kind = old_wk;
                meta.ec_written = old_written;
                (meta.phv_eval, meta.phv_join) = old_phv;
                (meta.lateral_sub, meta.lateral_held, meta.lateral_flat) = old_lat;
                if old_bound != 0 && !rewritten {
                    bound_pushed.extend(pushed_in.into_iter().filter(|q| !above.iter().any(|a| a.same_as(q))));
                    meta.bound = old_bound;
                    meta.bound_pushed = bound_pushed;
                }
                for c in old_pushed.into_iter().chain(const_keyed) {
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
            let above = if kind == JoinKind::Left { oj_class_clauses(above, nleft, &all_parts) } else { above };
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
            ..
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
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec: Vec::new(), table: None, ..Default::default() }));
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
                let info = Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec, table: None, ..Default::default() }));
                return Plan::Empty { ncols, cols, info };
            }
            Plan::Append { inputs, inh, alias: None, pw: None }
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
            // (A semi join's clauses are equivalence classes: a left column fixed to a
            // constant fixes the right one it equals, and the clause goes.)
            let (mut cond, mut hash, mut right) = (cond, hash, right);
            if kind == JoinKind::Semi && params.is_empty() {
                let mut rpush = Vec::new();
                for p in &push {
                    let Expr::Call(d, args, _) = p else { continue };
                    if d.name != "=" || args.len() != 2 || p.is_volatile() || d.coll.as_ref().is_some_and(|c| !c.deterministic) {
                        continue;
                    }
                    let (l, k, col_first) = match (&args[0], &args[1]) {
                        (Expr::Col(l, _), k @ Expr::Const(..)) => (*l, k.clone(), true),
                        (k @ Expr::Const(..), Expr::Col(l, _)) => (*l, k.clone(), false),
                        _ => continue,
                    };
                    if l >= nleft {
                        continue;
                    }
                    let fix = |rcol: &Expr| -> Option<Expr> {
                        let Expr::Col(r, rt) = rcol else { return None };
                        let lt = args[if col_first { 0 } else { 1 }].ty();
                        (lt.without_typmod() == rt.without_typmod()).then(|| {
                            let c = Expr::Col(*r, rt.clone());
                            if col_first {
                                Expr::Call(d.clone(), vec![c, k.clone()], DataType::Bool)
                            } else {
                                Expr::Call(d.clone(), vec![k.clone(), c], DataType::Bool)
                            }
                        })
                    };
                    if let Some((lk, rk)) = &mut hash
                        && let Some(i) = lk.iter().position(|e| matches!(e, Expr::Col(c, _) if *c == l))
                        && let Some(q) = fix(&rk[i])
                    {
                        rpush.push(q);
                        lk.remove(i);
                        rk.remove(i);
                        if lk.is_empty() {
                            hash = None;
                        }
                        continue;
                    }
                    if let Some(c) = cond.take() {
                        let mut cps = Vec::new();
                        conjuncts(c, &mut cps);
                        let at = cps.iter().position(|q| match q {
                            Expr::Call(qd, qa, _) if qd.name == "=" && qa.len() == 2 => matches!(
                                (&qa[0], &qa[1]),
                                (Expr::Col(a, _), Expr::Col(b, _)) if (*a == l && *b >= nleft) || (*b == l && *a >= nleft)
                            ),
                            _ => false,
                        });
                        if let Some(at) = at
                            && let Expr::Call(_, qa, _) = &cps[at]
                        {
                            let rc = if matches!(&qa[0], Expr::Col(a, _) if *a == l) { &qa[1] } else { &qa[0] };
                            if let Some(q) = fix(&rc.clone().shift_cols(-(nleft as isize))) {
                                rpush.push(q);
                                cps.remove(at);
                            }
                        }
                        cond = and_all(cps);
                    }
                }
                if !rpush.is_empty() {
                    let r = Arc::try_unwrap(right).unwrap_or_else(|a| (*a).clone());
                    right = Arc::new(add_filter(cat, r, Expr::And(rpush)));
                }
            }
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
                // (A FROM-less one: its clauses over the values are its gating quals, when
                // they read an enclosing query's values; constant ones fold, as below.)
                | Plan::One
            ) && parts.iter().any(|p| projectable(p, &exprs, matches!(&*input, Plan::Join { .. })))
                && (!matches!(&*input, Plan::One)
                    || parts.iter().all(|p| {
                        let mut sub = false;
                        p.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
                        sub || p.has_outer_refs()
                            || p.columns().iter().any(|&c| exprs.get(c).is_some_and(|e| e.has_outer_refs()))
                    })) =>
        {
            let (down, up): (Vec<Expr>, Vec<Expr>) =
                parts.into_iter().partition(|p| projectable(p, &exprs, matches!(&*input, Plan::Join { .. })));
            let inner_cols: Vec<usize> = down.iter().flat_map(|p| p.columns()).collect();
            let inner = and_all(down)
                .unwrap()
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(i, _) => output_copy(&exprs[i]),
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
        // A constant-false (or NULL) qual restricting a subquery planned apart stays over it:
        // Postgres excludes the relation before planning the subquery
        // (relation_excluded_by_constraints), so nothing in it is simplified
        // (`analyzejoins::reduce_dummies`).
        Plan::Barrier { .. }
            if parts.iter().any(|q| matches!(q, Expr::Const(Value::Bool(false) | Value::Null, _))) =>
        {
            Plan::Filter {
                input: Arc::new(plan),
                pred: and_all(parts).unwrap(),
            }
        }
        // A security barrier: only the clauses that can't leak its rows' values go into
        // it (leakproof, not volatile); the rest stay above.
        Plan::Barrier { input, alias, cols, secure, sorted, path, rti, copies } => {
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
                    && (secure || !reads_lateral_var(p))
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
            let b = Plan::Barrier { input, alias, cols, secure, sorted, path, rti, copies };
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
        Plan::Distinct { input, on, order, hashed } => {
            let i = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
            Plan::Distinct {
                input: Arc::new(push_into_level(cat, i, parts)),
                on,
                order,
                hashed,
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
        Plan::Append { inh: None, inputs, .. } => inputs.iter().any(|i| grouping_level(i)),
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
        Plan::Append { inh: None, inputs, .. } => output_collation(inputs.first()?, c),
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

fn projectable(p: &Expr, exprs: &[Expr], over_join: bool) -> bool {
    let phv = PHV_OUTPUTS.with(|f| f.get());
    p.columns().iter().all(|&c| {
        exprs.get(c).is_some_and(|e| {
            // (A PlaceHolderVar over the subquery's own columns is evaluated above its
            // scan; a strict expression of them needs none, pullup_replace_vars_callback.)
            if phv && !matches!(e, Expr::Col(..)) && !e.columns().is_empty() && contain_nonstrict(e) {
                return false;
            }
            // (One of none of its columns over a join, a constant or a lateral reference, is
            // evaluated at the join of all its relations (ph_eval_at its phrels): a qual
            // reading it is a join clause of them, not any one relation's.)
            if phv && over_join && !matches!(e, Expr::Col(..)) && e.columns().is_empty() {
                return false;
            }
            // (A sub-select, nothing volatile in it, outside PlaceHolderVar semantics: the
            // qual reads a copy of its own, pullup_replace_vars.)
            let mut plain = !crate::sql::explain::expr_volatile_deep(e);
            e.walk(&mut |x| {
                plain &= match x {
                    Expr::Subquery(s) => !phv && s.call.is_none(),
                    Expr::Srf(..) | Expr::Agg(..) | Expr::Window(..) => false,
                    _ => true,
                }
            });
            plain
        })
    })
}

/// A pulled-up subquery's output put in place of a reference to it: a sub-select in it is a
/// copy of its own there (an id and plan nodes of its own, `make_subquery_copy`), the
/// reference a qual's.
fn output_copy(e: &Expr) -> Expr {
    let mut sub = false;
    e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(s) if s.call.is_none()));
    if !sub {
        return e.clone();
    }
    e.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Subquery(mut s) if s.call.is_none() => {
                    crate::sql::paths::make_subquery_copy(&mut s);
                    s.qual = true;
                    s.tlist = false;
                    Expr::Subquery(s)
                }
                x => x,
            })
        })
        .unwrap_or_else(|_| e.clone())
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
    (lhave, rhave): (&[Expr], &[Expr]),
    (lcoll, rcoll): (&dyn Fn(usize) -> Option<String>, &dyn Fn(usize) -> Option<String>),
    rewrites: &mut Vec<(bool, Expr, Expr)>,
) -> Vec<Expr> {
    // The nondeterministic collation a clause compares under (None: a deterministic one, all
    // of which compare bytes): an equivalence class is one collation's (process_equivalence
    // merges only clauses of the class's ec_collation), so a constant joins a join clause's
    // class only under the same one.
    let nondet = |e: &Expr| match e {
        Expr::Call(d, ..) => d.coll.as_ref().filter(|c| !c.deterministic).map(|c| c.name.clone()),
        _ => None,
    };
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
    // The equi-join pairs, the right side in the right input's columns: (left, right, the
    // hash key's index or else the condition's conjunct's, the class's nondeterministic
    // collation).
    let mut pairs: Vec<(Expr, Expr, Result<usize, usize>, Option<String>)> = Vec::new();
    if let Some((hl, hr)) = hash.as_ref() {
        for (i, (a, b)) in hl.iter().zip(hr).enumerate() {
            pairs.push((a.clone(), b.clone(), Ok(i), None));
        }
    }
    // (Whether a join clause names its right side's column first.)
    let mut right_first = Vec::new();
    for (i, c) in cond.iter().enumerate() {
        let pair = ec_pair(c, nleft)
            .map(|(a, b)| (a, b, None))
            .or_else(|| nondet_pair(c, nleft).map(|(a, b)| (a, b, nondet(c))));
        if let Some((a, b, coll)) = pair {
            right_first.push((
                pairs.len(),
                matches!(c, Expr::Call(_, args, _) if args[0].same_as(&b)),
            ));
            pairs.push((a, b.shift_cols(-(nleft as isize)), Err(i), coll));
        }
    }
    // (Into an outer join's nullable side not an outer query's value: in Postgres a
    // sublink reading it was pulled up into a join before equivalence classes form.)
    let outer_join = !(to_right && to_left);
    // (Under row-level security the statement's quals sit above a security level: one that
    // isn't leakproof is no equivalence class member (process_equivalence).)
    let rls = QUERY_RLS.with(|f| f.get());
    // (The restrictions already in each input count too, `lhave` / `rhave`: a pulled-up
    // subquery's `x = k` in its scan. They are the class's sources, never pushed again.)
    // (A nondeterministic class's member is its column: an explicit COLLATE is a
    // relabeling, eval_const_expressions' RelabelType.)
    let consts = |push: &[Expr], have: &[Expr], to: bool| -> Vec<(Expr, Expr, Expr, bool)> {
        push.iter()
            .map(|e| (e, false))
            .chain(have.iter().map(|e| (e, true)))
            .filter(|_| to)
            .filter(|(e, _)| !(rls && leaks(cat, e)))
            .filter_map(|(e, old)| {
                let x = match e {
                    Expr::Call(d, args, t) if nondet(e).is_some() => {
                        Expr::Call(d.clone(), args.iter().map(|a| uncollate(a).clone()).collect(), t.clone())
                    }
                    e => e.clone(),
                };
                const_eq(&x).map(|(x, k)| (x, k, e.clone(), old))
            })
            .filter(|(_, k, _, old)| !((outer_join || *old) && k.has_outer_refs()))
            .collect()
    };
    let lconst = consts(lpush, lhave, to_right);
    let rconst = consts(rpush, rhave, to_left);
    let mut drop_hash = Vec::new();
    let mut drop_cond = false;
    let derive = |clause: &Expr, from: &Expr, to: &Expr| -> Expr {
        clause
            .clone()
            .transform(&mut |e| Ok(if e.same_as(from) { to.clone() } else { e }))
            .unwrap_or_else(|_| clause.clone())
    };
    // (Across a `name = text` join clause, whose class has the name side's C collation: a
    // constant the name side is equated with gives the text side `text = k` (texteqname,
    // select_equality_operator), the text column relabeled to the class's collation
    // (canonicalize_ec_expression: `(i)::text`); the text side's `= k` joins the class when
    // it compares under C too (with a name, `i = 'x'::name`: then it comes back relabeled
    // itself, the second value), else it is of its column's collation, a class of its own
    // that stays on its side.)
    let relabel = |x: &Expr| Expr::Collate(Box::new(x.clone()), String::new(), -1);
    let cross = |clause: &Expr, from: &Expr, to: &Expr, k: &Expr| -> Option<(Expr, Option<Expr>)> {
        if !name_text_pair(&from.ty(), &to.ty()) {
            return Some((derive(clause, from, to), None));
        }
        let name = |t: DataType| t.without_typmod() == DataType::Name;
        let r = crate::sql::func::resolve_binop("=", &to.ty(), &k.ty()).ok()?;
        if name(from.ty()) {
            return Some((Expr::Call(r.def, vec![relabel(to), k.clone()], DataType::Bool), None));
        }
        if !name(k.ty()) {
            return None;
        }
        let source = clause
            .clone()
            .transform(&mut |e| Ok(if e.same_as(from) { relabel(&e) } else { e }))
            .ok()?;
        Some((Expr::Call(r.def, vec![to.clone(), k.clone()], DataType::Bool), Some(source)))
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
    // (Columns of one side the side's own equalities put in one class with a join
    // column: `ec1.ff = ec1.f1` with `ss1.x = ec1.f1` and `ec1.ff = 42` gives `ss1.x = 42`.)
    let side_class = |push: &[Expr], a: &Expr| -> Vec<Expr> {
        let mut class = vec![a.clone()];
        loop {
            let before = class.len();
            for e in push {
                let Expr::Call(_, args, _) = e else { continue };
                if !ec_clause(e) || !matches!((&args[0], &args[1]), (Expr::Col(..), Expr::Col(..))) {
                    continue;
                }
                for (p, q) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                    if class.iter().any(|m| m.same_as(p)) && !class.iter().any(|m| m.same_as(q)) {
                        class.push(q.clone());
                    }
                }
            }
            if class.len() == before {
                return class;
            }
        }
    };
    // generate_base_implied_equalities_const for a nondeterministic class: each member's
    // `member = k` built over the class's collation (canonicalize_ec_expression: a member
    // of another collation relabeled, `(x)::text`; the constant given the class's,
    // `'k'::text COLLATE c`).
    let regen = |clause: &Expr, m: &Expr, k: &Expr, coll: &str, mcoll: &dyn Fn(usize) -> Option<String>| -> Expr {
        let Expr::Call(def, _, ty) = clause else { return clause.clone() };
        let m = uncollate(m).clone();
        let own = match &m {
            Expr::Col(c, _) => mcoll(*c),
            _ => None,
        };
        let m = if own.as_deref() == Some(coll) { m } else { Expr::Collate(Box::new(m), String::new(), -1) };
        let k = Expr::Collate(Box::new(uncollate(k).clone()), coll.to_string(), -1);
        Expr::Call(def.clone(), vec![m, k], ty.clone())
    };
    // The collation an equality of operands `args` compares under (its inputcollid): an
    // explicit COLLATE's, else its columns' (`coll_of`), else the default. process_equivalence
    // keeps classes of different ec_collation apart, deterministic ones too: `a.x = b.x AND
    // a.x = 'k' COLLATE "C"` is two classes, no `b.x = 'k'` derived.
    let det_coll = |args: &[&Expr], coll_of: &dyn Fn(usize) -> Option<String>| -> String {
        let norm = |n: Option<String>| n.filter(|n| !n.is_empty() && n != "default").unwrap_or_else(|| "default".into());
        let mut implicit = None;
        for a in args {
            let mut x = *a;
            loop {
                match x {
                    Expr::Collate(_, name, _) if !name.is_empty() => return norm(Some(name.clone())),
                    Expr::Collate(y, ..) | Expr::Relabel(y, _) | Expr::Cast(y, ..) => x = y,
                    _ => break,
                }
            }
            if implicit.is_none()
                && let Expr::Col(c, _) = x
            {
                implicit = coll_of(*c);
            }
        }
        norm(implicit)
    };
    let pair_coll = |a: &Expr, b: &Expr| -> String {
        let l = det_coll(&[a], lcoll);
        if l != "default" {
            return l;
        }
        det_coll(&[b], rcoll)
    };
    let const_coll = |clause: &Expr, coll_of: &dyn Fn(usize) -> Option<String>| -> String {
        match clause {
            Expr::Call(_, args, _) => det_coll(&args.iter().collect::<Vec<_>>(), coll_of),
            _ => "default".into(),
        }
    };
    let mut broken_dups: Vec<Expr> = Vec::new();
    let mut used_pairs: Vec<usize> = Vec::new();
    for (n, (a, b, hi, pcoll)) in pairs.iter().enumerate() {
        let dcoll = pair_coll(a, b);
        let mut used = false;
        let (a, b) = if pcoll.is_some() { (uncollate(a), uncollate(b)) } else { (a, b) };
        let (lclass, rclass) = (side_class(lpush, a), side_class(rpush, b));
        // (The join clause goes when both columns compare with the constant.)
        for (x, k, clause, old) in &lconst {
            // (A constant of another collation's class: not this one's.)
            if nondet(clause) != *pcoll || pcoll.is_none() && const_coll(clause, lcoll) != dcoll {
                continue;
            }
            let direct = x.same_as(a);
            if !direct && !lclass.iter().any(|m| m.same_as(x)) {
                continue;
            }
            if let Some(coll) = pcoll {
                let d = regen(clause, b, k, coll, rcoll);
                if !rpush.iter().chain(rhave).any(|e| uncollated_eq(e, &d)) {
                    rpush.push(d);
                }
                // (Its own clause rebuilt too, in a class of the inner join's: one member
                // and the constant keep the clause as written, as on an outer join's side.)
                if !outer_join {
                    let own = regen(clause, x, k, coll, lcoll);
                    if *old {
                        rewrites.push((false, clause.clone(), own));
                    } else if let Some(c) = lpush.iter_mut().find(|e| e.same_as(clause)) {
                        *c = own;
                    }
                }
                used = true;
                continue;
            }
            if derivable(b, k)
                && let Some((d, source)) = cross(clause, x, b, k)
            {
                if !rpush.iter().chain(rhave).any(|e| e.same_as(&d)) {
                    rpush.push(d);
                }
                if let Some(s) = source
                    && let Some(c) = lpush.iter_mut().find(|e| e.same_as(clause))
                {
                    *c = s;
                }
                used |= derivable(a, k);
            }
            // (The class's members in the order its clauses merged them: the constant's
            // column first, and so its clause derived before the break, when the clause
            // equating it with the join column names it first.)
            let x_first = |push: &[Expr], class: &[Expr]| {
                push.iter().any(|e| match e {
                    Expr::Call(_, args, _) if ec_clause(e) && args.len() == 2 => {
                        args[0].same_as(x) && class.iter().any(|m| m.same_as(&args[1]))
                    }
                    _ => false,
                })
            };
            if (!derivable(b, k) || !derivable(a, k))
                && derivable(x, k)
                && !old
                && if direct {
                    !right_first.iter().any(|&(p, r)| p == n && r)
                } else {
                    x_first(lpush, &lclass)
                }
            {
                // A broken class: the constant's own clause comes back besides the one
                // derived for its column (generate_base_implied_equalities_broken).
                lpush.push(clause.clone());
                broken_dups.push(clause.clone());
            }
        }
        for (x, k, clause, old) in &rconst {
            if nondet(clause) != *pcoll || pcoll.is_none() && const_coll(clause, rcoll) != dcoll {
                continue;
            }
            let direct = x.same_as(b);
            if !direct && !rclass.iter().any(|m| m.same_as(x)) {
                continue;
            }
            if let Some(coll) = pcoll {
                let d = regen(clause, a, k, coll, lcoll);
                if !lpush.iter().chain(lhave).any(|e| uncollated_eq(e, &d)) {
                    lpush.push(d);
                }
                if !outer_join {
                    let own = regen(clause, x, k, coll, rcoll);
                    if *old {
                        rewrites.push((true, clause.clone(), own));
                    } else if let Some(c) = rpush.iter_mut().find(|e| e.same_as(clause)) {
                        *c = own;
                    }
                }
                used = true;
                continue;
            }
            if derivable(a, k)
                && let Some((d, source)) = cross(clause, x, a, k)
            {
                if !lpush.iter().chain(lhave).any(|e| e.same_as(&d)) {
                    lpush.push(d);
                }
                if let Some(s) = source
                    && let Some(c) = rpush.iter_mut().find(|e| e.same_as(clause))
                {
                    *c = s;
                }
                used |= derivable(b, k);
            }
            let x_first = |push: &[Expr], class: &[Expr]| {
                push.iter().any(|e| match e {
                    Expr::Call(_, args, _) if ec_clause(e) && args.len() == 2 => {
                        args[0].same_as(x) && class.iter().any(|m| m.same_as(&args[1]))
                    }
                    _ => false,
                })
            };
            if (!derivable(a, k) || !derivable(b, k))
                && derivable(x, k)
                && !old
                && if direct {
                    right_first.iter().any(|&(p, r)| p == n && r)
                } else {
                    x_first(rpush, &rclass)
                }
            {
                rpush.push(clause.clone());
                broken_dups.push(clause.clone());
            }
        }
        if used {
            used_pairs.push(n);
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
                Ok(i) => drop_hash.push(*i),
                Err(_) => drop_cond = true,
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
        let gone: Vec<usize> = used_pairs
            .iter()
            .filter_map(|&n| pairs[n].2.err())
            .collect();
        let mut i = 0;
        cond.retain(|_| {
            let keep = !gone.contains(&i);
            i += 1;
            keep
        });
    }
    broken_dups
}

/// Two `=` clauses the same but for their operands' COLLATE relabelings.
fn uncollated_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Call(d1, a1, _), Expr::Call(d2, a2, _)) => {
            d1.oid == d2.oid
                && a1.len() == a2.len()
                && a1.iter().zip(a2).all(|(x, y)| uncollate(x).same_as(uncollate(y)))
        }
        _ => a.same_as(b),
    }
}

/// Restriction `old` (over `p`'s row) of a scan or filter in `p` (through projections of
/// columns and inner joins, as `input_restrictions` finds them) replaced by `new`.
fn replace_restriction(cat: &Catalog, p: &mut Plan, old: &Expr, new: &Expr) -> bool {
    let swap = |pred: &Expr| -> Option<Expr> {
        let mut parts = Vec::new();
        conjuncts(pred.clone(), &mut parts);
        let i = parts.iter().position(|e| e.same_as(old))?;
        parts[i] = new.clone();
        and_all(parts)
    };
    match p {
        Plan::Scan(s) => {
            let Some(f) = s.filter.as_ref().and_then(swap) else { return false };
            s.filter = Some(f);
            if !matches!(s.access, crate::sql::plan::Access::Index { ordered: true, .. }) {
                s.access = crate::sql::plan::Access::Unplanned;
                choose_index(cat, s);
            }
            true
        }
        Plan::Filter { input, pred } => match swap(pred) {
            Some(f) => {
                *pred = f;
                true
            }
            None => replace_restriction(cat, Arc::make_mut(input), old, new),
        },
        Plan::Project { input, exprs } => {
            let back = |e: &Expr| {
                e.clone().transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(j, t) => match exprs.get(j) {
                            Some(Expr::Col(i, _)) => Expr::Col(*i, t),
                            _ => return Err(crate::error::internal("not a column")),
                        },
                        x => x,
                    })
                })
            };
            match (back(old), back(new)) {
                (Ok(o), Ok(n)) => replace_restriction(cat, Arc::make_mut(input), &o, &n),
                _ => false,
            }
        }
        Plan::Join {
            kind: JoinKind::Inner,
            left,
            right,
            nleft,
            meta,
            ..
        } => {
            // (A planned join kept as bound: the tree as bound doesn't have the new one.)
            meta.bound = 0;
            let n = *nleft;
            if old.columns().iter().all(|&c| c < n) {
                replace_restriction(cat, Arc::make_mut(left), old, new)
            } else if old.columns().iter().all(|&c| c >= n) {
                let (o, w) = (old.clone().shift_cols(-(n as isize)), new.clone().shift_cols(-(n as isize)));
                replace_restriction(cat, Arc::make_mut(right), &o, &w)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// An operand without its explicit COLLATE relabelings.
fn uncollate(e: &Expr) -> &Expr {
    match e {
        Expr::Collate(x, ..) => uncollate(x),
        e => e,
    }
}

/// A join clause of a nondeterministic collation's class: the built-in `=` between a column
/// (or its relabeling, `x COLLATE c`) of each side (the right one in the join's row).
fn nondet_pair(e: &Expr, nleft: usize) -> Option<(Expr, Expr)> {
    let Expr::Call(def, args, _) = e else {
        return None;
    };
    if def.name != "=" || args.len() != 2 || !def.coll.as_ref().is_some_and(|c| !c.deterministic) {
        return None;
    }
    if !builtin_eq(def, &args[0].ty(), &args[1].ty()) || e.is_volatile() {
        return None;
    }
    if !matches!(uncollate(&args[0]), Expr::Col(..)) || !matches!(uncollate(&args[1]), Expr::Col(..)) {
        return None;
    }
    match (side(&args[0], nleft), side(&args[1], nleft)) {
        (Side::Left, Side::Right) => Some((args[0].clone(), args[1].clone())),
        (Side::Right, Side::Left) => Some((args[1].clone(), args[0].clone())),
        _ => None,
    }
}

/// A WHERE's equalities above a left join, read in its nullable side: an equivalence class
/// of their members (all the WHERE's equalities, in their order: process_equivalence),
/// whose clauses Postgres derives at the join (generate_join_implied_equalities_normal): the
/// members only the join computes equated in turn, the last with the first member of the
/// outer side's (those among themselves go below), each clause reusing a written one over
/// the same two members as it was written. Only classes of three or more members of one
/// btree family without constants, and not when a qual is strict for the nullable side (the
/// join is an inner one then); the derived clauses go after the other quals (the join's
/// restrictions list them after its own clauses).
fn oj_class_clauses(above: Vec<Expr>, nleft: usize, all: &[Expr]) -> Vec<Expr> {
    if QUERY_RLS.with(|f| f.get()) || crate::sql::paths::strict_cols(all).iter().any(|&c| c >= nleft) {
        return above;
    }
    let member = |e: &Expr| {
        let mut ok = !e.columns().is_empty() && !e.is_volatile();
        e.walk(&mut |x| ok &= !matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Outer(..)));
        ok
    };
    // (One btree family: the same type, or the integer types.)
    let fam = |t: &DataType| match t {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => DataType::Int8,
        t => t.without_typmod(),
    };
    let pair = |e: &Expr| -> Option<(Expr, Expr)> {
        let Expr::Call(_, args, _) = e else { return None };
        if !ec_clause(e) || fam(&args[0].ty()) != fam(&args[1].ty()) {
            return None;
        }
        (member(&args[0]) && member(&args[1]) && !args[0].same_as(&args[1])).then(|| (args[0].clone(), args[1].clone()))
    };
    // process_equivalence: (members in order, the clauses that made the class).
    let mut classes: Vec<(Vec<Expr>, Vec<usize>)> = Vec::new();
    for (i, e) in all.iter().enumerate() {
        let Some((x, y)) = pair(e) else { continue };
        let find = |classes: &Vec<(Vec<Expr>, Vec<usize>)>, m: &Expr| classes.iter().position(|(ms, _)| ms.iter().any(|z| z.same_as(m)));
        match (find(&classes, &x), find(&classes, &y)) {
            (None, None) => classes.push((vec![x, y], vec![i])),
            (Some(c), None) => {
                classes[c].0.push(y);
                classes[c].1.push(i);
            }
            (None, Some(c)) => {
                classes[c].0.push(x);
                classes[c].1.push(i);
            }
            (Some(a), Some(b)) if a == b => classes[a].1.push(i),
            (Some(a), Some(b)) => {
                let (ms, cs) = classes.remove(b);
                let a = if b < a { a - 1 } else { a };
                classes[a].0.extend(ms);
                classes[a].1.extend(cs);
                classes[a].1.push(i);
            }
        }
    }
    let outer = |e: &Expr| e.columns().iter().all(|&c| c < nleft);
    let mut used = vec![false; above.len()];
    let mut derived: Vec<Expr> = Vec::new();
    for (ms, cs) in &classes {
        let olds: Vec<&Expr> = ms.iter().filter(|m| outer(m)).collect();
        let Some(f0) = ms.first().map(|m| fam(&m.ty())) else { continue };
        if ms.len() < 3 || ms.iter().any(|m| fam(&m.ty()) != f0) {
            continue;
        }
        // (Its clauses left above the join: the ones replaced.)
        let mine: Vec<usize> = (0..above.len())
            .filter(|&k| cs.iter().any(|&i| all[i].same_as(&above[k])))
            .collect();
        if mine.is_empty() {
            continue;
        }
        let mut chain: Vec<&Expr> = ms.iter().filter(|m| !outer(m)).collect();
        chain.extend(olds.first().copied());
        let mut made = Vec::new();
        for w in chain.windows(2) {
            let (a, b) = (w[0], w[1]);
            let written = cs.iter().find_map(|&i| {
                let (x, y) = pair(&all[i])?;
                ((x.same_as(a) && y.same_as(b)) || (x.same_as(b) && y.same_as(a))).then(|| all[i].clone())
            });
            let e = match written {
                Some(e) => e,
                None => match crate::sql::func::resolve_binop("=", &a.ty(), &b.ty()) {
                    // (An operator taking the members as they are: no coercion.)
                    Ok(r)
                        if r.arg_types.len() == 2
                            && r.arg_types[0].without_typmod() == a.ty().without_typmod()
                            && r.arg_types[1].without_typmod() == b.ty().without_typmod() =>
                    {
                        Expr::Call(r.def, vec![a.clone(), b.clone()], DataType::Bool)
                    }
                    _ => break,
                },
            };
            made.push(e);
        }
        if made.len() + 1 != chain.len() {
            continue;
        }
        derived.extend(made);
        for k in mine {
            used[k] = true;
        }
    }
    if derived.is_empty() {
        return above;
    }
    let mut out: Vec<Expr> = above.into_iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, e)| e).collect();
    out.extend(derived);
    out
}

/// The restrictions an inner join's input already applies to its rows, over its output
/// columns: its scans' filters and the filters above them, through projections of plain
/// columns and the inner joins below (a pulled-up subquery's WHERE, pushed into its scan
/// before the join above is formed). For `ec_constants`, which takes only their
/// `column = constant` equalities.
pub(crate) fn input_restrictions(p: &Plan) -> Vec<Expr> {
    restrictions(p, false)
}

/// `input_restrictions`, `lower`: the nullable sides' too (a lower join domain's).
fn restrictions(p: &Plan, lower: bool) -> Vec<Expr> {
    let input_restrictions = |q: &Plan| restrictions(q, lower);
    let mut out = Vec::new();
    match p {
        Plan::Scan(s) => {
            if let Some(f) = &s.filter {
                conjuncts(f.clone(), &mut out);
            }
        }
        Plan::Filter { input, pred } => {
            conjuncts(pred.clone(), &mut out);
            out.extend(input_restrictions(input));
        }
        Plan::Project { input, exprs } => {
            for e in input_restrictions(input) {
                // (Only over columns the projection returns as they are.)
                let mut ok = true;
                let mapped = e
                    .transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(i, t) => match exprs.iter().position(|o| matches!(o, Expr::Col(j, _) if *j == i)) {
                                Some(j) => Expr::Col(j, t),
                                None => {
                                    ok = false;
                                    Expr::Col(i, t)
                                }
                            },
                            x => x,
                        })
                    })
                    .ok();
                if let Some(m) = mapped.filter(|_| ok) {
                    out.push(m);
                }
            }
        }
        Plan::Join {
            kind: JoinKind::Inner,
            left,
            right,
            nleft,
            params,
            ..
        } => {
            out.extend(input_restrictions(left));
            if params.is_empty() {
                out.extend(input_restrictions(right).into_iter().map(|e| e.shift_cols(*nleft as isize)));
            }
        }
        // (An outer join's preserved side: its restrictions hold for every row of the join,
        // in the same join domain; the nullable side's are a lower domain's.)
        Plan::Join { kind: kind @ (JoinKind::Left | JoinKind::Right), left, right, nleft, params, .. } => {
            if *kind == JoinKind::Left || lower {
                out.extend(input_restrictions(left));
            }
            if (*kind == JoinKind::Right || lower) && params.is_empty() {
                out.extend(input_restrictions(right).into_iter().map(|e| e.shift_cols(*nleft as isize)));
            }
        }
        _ => {}
    }
    out
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
            ..
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
        // (A class a join found broken: its clauses stay as they came, both copies.)
        let broken = BROKEN_DUPS.with(|b| {
            let b = b.borrow();
            !b.is_empty()
                && found
                    .iter()
                    .zip(all.iter())
                    .any(|(f, e)| f.as_ref().is_some_and(|(a, _, _)| in_class(*a)) && b.iter().any(|x| x.same_as(e)))
        });
        if broken {
            continue;
        }
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

/// generate_base_implied_equalities_const in the sub-selects that stay SubPlans: the outer
/// query's values a sub-select reads are Params there, pseudo-constant members of its
/// equivalence classes. Postgres plans a sub-select (make_subplan) once pull_up_sublinks has
/// taken the ones it pulls up; here a sub-select's level is planned as it is bound, before
/// the level reading it decides, so the classes with outer values are made once that level
/// is planned (`plan_level`), for the sub-selects left in its expressions. A class's
/// constant is its first Const, else its last pseudo-constant; each other member is compared
/// with it, a pseudo-constant giving a gating qual (the One-Time Filter of a Result over the
/// scan). Done where the sub-select's level is one sequential scan (a join's classes span
/// its relations); an EXISTS that also gets a hashed form keeps its quals (Postgres hashes
/// the query as written, convert_EXISTS_to_ANY).
pub fn subplan_classes(plan: &mut Plan) {
    fn rewritten(s: &crate::sql::expr::Subquery) -> Option<Plan> {
        if s.params.is_empty()
            || s.call.is_some()
            || (s.kind == crate::sql::expr::SubqueryKind::Exists
                && crate::sql::explain::exists_hashed_from(&s.plan, true).is_some())
        {
            return None;
        }
        let mut p = (*s.plan).clone();
        rewrite_level_scan(&mut p).then_some(p)
    }
    fn wants(p: &Plan) -> bool {
        let mut es = Vec::new();
        crate::sql::explain::node_exprs(p, &mut es);
        let mut found = false;
        for e in es {
            e.walk(&mut |x| {
                if !found && let Expr::Subquery(s) = x {
                    found = rewritten(s).is_some();
                }
            });
        }
        found || crate::sql::explain::plan_inputs(p).into_iter().any(|c| wants(c))
    }
    if !wants(plan) {
        return;
    }
    for c in crate::sql::explain::plan_inputs_mut(plan) {
        if wants(c) {
            subplan_classes(Arc::make_mut(c));
        }
    }
    let one = |e: &mut Expr| {
        *e = std::mem::replace(e, Expr::Const(Value::Null, DataType::Unknown))
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Subquery(mut s) => {
                        if let Some(p) = rewritten(&s) {
                            s.plan = Arc::new(p);
                        }
                        Expr::Subquery(s)
                    }
                    x => x,
                })
            })
            .expect("rewriting a sub-select");
    };
    match plan {
        Plan::Scan(s) => s.filter.iter_mut().for_each(one),
        Plan::SysScan { filter, .. } => filter.iter_mut().for_each(one),
        Plan::Filter { pred, .. } => one(pred),
        Plan::Project { exprs, .. } | Plan::ProjectSet { exprs, .. } => exprs.iter_mut().for_each(one),
        Plan::Join { cond, .. } => cond.iter_mut().for_each(one),
        Plan::Agg { groups, .. } => groups.iter_mut().for_each(one),
        Plan::Sort { keys, .. } => keys.iter_mut().for_each(|k| one(&mut k.expr)),
        _ => {}
    }
}

/// `subplan_classes` for one sub-select's plan: its level's scan, under the projections,
/// sorts, limits and aggregation over it, when that is a sequential scan whose
/// restrictions equate a column with an outer value. Whether it changed anything.
fn rewrite_level_scan(p: &mut Plan) -> bool {
    use crate::sql::plan::Access;
    match p {
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Agg { input, sets: None, .. } => {
            let mut c = (**input).clone();
            let done = rewrite_level_scan(&mut c);
            if done {
                *input = Arc::new(c);
            }
            done
        }
        Plan::Scan(s) if s.index.is_none() && matches!(s.access, Access::Seq | Access::Unplanned) => {
            let Some(f) = s.filter.clone() else { return false };
            let mut cs = Vec::new();
            conjuncts(f, &mut cs);
            // (In the order baserestrictinfo holds them, which the classes met them in.)
            cs.sort_by_key(|q| s.written.iter().position(|w| w.same_as(q)).unwrap_or(usize::MAX));
            let Some(mut all) = outer_value_classes(cs) else { return false };
            let mut written: Vec<Expr> = s
                .written
                .iter()
                .filter(|w| all.iter().any(|q| q.same_as(w)))
                .cloned()
                .collect();
            for q in &all {
                if !written.iter().any(|w| w.same_as(q)) {
                    written.push(q.clone());
                }
            }
            order_quals(&mut all);
            s.written = written;
            s.filter = match all.len() {
                1 => all.pop(),
                _ => Some(Expr::And(all)),
            };
            true
        }
        _ => false,
    }
}

/// The restrictions `cs` (in the order the classes met them) with the classes holding an
/// outer value made (`subplan_classes`): the other quals, then each class's equalities with
/// its constant. None when there is no such class.
fn outer_value_classes(cs: Vec<Expr>) -> Option<Vec<Expr>> {
    let konst = |x: &Expr| {
        let mut sub = false;
        x.walk(&mut |y| sub |= matches!(y, Expr::Subquery(_)));
        x.columns().is_empty() && !sub && !x.is_volatile() && pseudo_constant(x)
    };
    // `member = member` with a column on one side at least: (left, right, operator).
    let pair = |e: &Expr| -> Option<(Expr, Expr, Arc<crate::sql::func::FuncDef>)> {
        let Expr::Call(def, args, _) = e else { return None };
        if !ec_clause(e)
            || !def.strict
            || def.coll.as_ref().is_some_and(|c| !c.deterministic)
            || args[0].ty().without_typmod() != args[1].ty().without_typmod()
        {
            return None;
        }
        let col = |x: &Expr| matches!(x, Expr::Col(..));
        let ok = (col(&args[0]) || konst(&args[0])) && (col(&args[1]) || konst(&args[1]));
        (ok && (col(&args[0]) || col(&args[1]))).then(|| (args[0].clone(), args[1].clone(), def.clone()))
    };
    let found: Vec<_> = cs.iter().map(pair).collect();
    let mut members: Vec<Expr> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    fn root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let idx = |e: &Expr, members: &mut Vec<Expr>, parent: &mut Vec<usize>| -> usize {
        match members.iter().position(|m| m.same_as(e)) {
            Some(i) => i,
            None => {
                members.push(e.clone());
                parent.push(parent.len());
                members.len() - 1
            }
        }
    };
    let mut src: Vec<Option<usize>> = Vec::new();
    for f in &found {
        src.push(f.as_ref().map(|(a, b, _)| {
            let (i, j) = (idx(a, &mut members, &mut parent), idx(b, &mut members, &mut parent));
            let (ri, rj) = (root(&mut parent, i), root(&mut parent, j));
            if ri != rj {
                parent[rj] = ri;
            }
            i
        }));
    }
    let roots: Vec<usize> = (0..members.len()).map(|i| root(&mut parent, i)).collect();
    let mut drop = vec![false; cs.len()];
    let mut derived = Vec::new();
    let mut done = Vec::new();
    for &r in &roots {
        if done.contains(&r) {
            continue;
        }
        done.push(r);
        let class: Vec<usize> = (0..members.len()).filter(|&i| roots[i] == r).collect();
        let sources: Vec<usize> = (0..cs.len()).filter(|&q| src[q].is_some_and(|i| roots[i] == r)).collect();
        let konsts: Vec<usize> = class.iter().copied().filter(|&i| konst(&members[i])).collect();
        let consts = konsts.iter().filter(|&&i| matches!(members[i], Expr::Const(..))).count();
        if sources.len() < 2 || consts > 1 || !konsts.iter().any(|&i| members[i].has_outer_refs()) {
            continue;
        }
        // (One operator for the whole class.)
        let def = found[sources[0]].as_ref().unwrap().2.clone();
        if sources.iter().any(|&q| found[q].as_ref().unwrap().2.oid != def.oid) {
            continue;
        }
        let k = konsts
            .iter()
            .copied()
            .find(|&i| matches!(members[i], Expr::Const(..)))
            .or(konsts.last().copied())
            .unwrap();
        for &q in &sources {
            drop[q] = true;
        }
        for &m in &class {
            if m != k {
                derived.push(Expr::Call(def.clone(), vec![members[m].clone(), members[k].clone()], DataType::Bool));
            }
        }
    }
    if derived.is_empty() {
        return None;
    }
    let mut out: Vec<Expr> = cs.into_iter().zip(drop).filter(|(_, d)| !d).map(|(q, _)| q).collect();
    out.extend(derived);
    Some(out)
}

/// generate_base_implied_equalities_no_const for a scan's restriction equating a
/// PlaceHolderVar it evaluates (a LATERAL subquery's output under an outer join, read as
/// the join's value) with one of its columns: the class lists its members in the order the
/// clauses brought them in, and gives the scan `earlier member = later member`. A column an
/// earlier equality already put in a class (a join clause below, the subquery's own clause,
/// distributed before the clause above) comes first, so `phv = col` comes back `col = phv`.
fn phv_class_order(have: &[Expr], parts: Vec<Expr>) -> Vec<Expr> {
    let pend = PHV_PENDING.with(|p| p.borrow().clone());
    if pend.is_empty() {
        return parts;
    }
    let reads_phv = |e: &Expr| {
        let mut r = false;
        e.walk(&mut |x| r |= pend.iter().any(|v| v.same_as(x)));
        r
    };
    let member_col = |e: &Expr| match e {
        Expr::Col(c, _) => Some(*c),
        Expr::Relabel(x, _) => match &**x {
            Expr::Col(c, _) => Some(*c),
            _ => None,
        },
        _ => None,
    };
    // (An equality of two members: one with a constant gives each member `= constant`.)
    let in_class = |c: usize, earlier: &[Expr]| {
        earlier.iter().any(|q| {
            ec_clause(q)
                && matches!(q, Expr::Call(_, a, _) if a.len() == 2
                    && a.iter().enumerate().any(|(i, x)| member_col(x) == Some(c)
                        && (!a[1 - i].columns().is_empty() || a[1 - i].has_outer_refs())))
        })
    };
    let mut out: Vec<Expr> = Vec::with_capacity(parts.len());
    for p in parts {
        let p = match p {
            Expr::Call(d, a, ty)
                if d.name == "="
                    && a.len() == 2
                    && a[0].columns().is_empty()
                    && reads_phv(&a[0])
                    && !reads_phv(&a[1])
                    && !a.iter().any(Expr::is_volatile)
                    && a[0].ty().without_typmod() == a[1].ty().without_typmod()
                    && member_col(&a[1]).is_some_and(|c| in_class(c, have) || in_class(c, &out)) =>
            {
                let [x, y]: [Expr; 2] = a.try_into().unwrap();
                Expr::Call(d, vec![y, x], ty)
            }
            p => p,
        };
        out.push(p);
    }
    out
}

/// A sub-select that is an InitPlan of the level reading it (SS_process_sublinks): no
/// column of the level read, its output a Param.
fn initplan_sublink(s: &crate::sql::expr::Subquery) -> bool {
    s.call.is_none()
        && matches!(
            s.kind,
            crate::sql::expr::SubqueryKind::Scalar | crate::sql::expr::SubqueryKind::Exists | crate::sql::expr::SubqueryKind::Array
        )
        && !s.params.iter().any(|p| !p.columns().is_empty())
        && !crate::exec::reads_enclosing(&s.plan, 0)
}

/// `ec_clause` for order_qual_clauses: an equality with an InitPlan's output too (a Param
/// by then, which process_equivalence takes as a constant member).
fn ec_order_clause(e: &Expr) -> bool {
    if ec_clause(e) {
        return true;
    }
    let Expr::Call(def, args, _) = e else { return false };
    if def.name != "=" || args.len() != 2 {
        return false;
    }
    let mut other = false;
    let mut subs = false;
    e.walk(&mut |x| match x {
        Expr::Call(d, ..) if d.volatility == 'v' => other = true,
        Expr::Subquery(s) if initplan_sublink(s) => subs = true,
        Expr::Subquery(s) if !matches!(s.func, Some('i' | 's')) => other = true,
        _ => {}
    });
    subs && !other && (!args[0].columns().is_empty() || !args[1].columns().is_empty())
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
            .then(ec_order_clause(a).cmp(&ec_order_clause(b)))
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
                && s.left.iter().all(|l| pg_hashable(&l.ty()))
                && !crate::exec::reads_enclosing(&s.plan, 0) =>
        {
            cost += s.ops.len().max(1) as f64
        }
        // (One reading no column of this level is an InitPlan (SS_process_sublinks): the
        // qual reads its output Param, which costs nothing per row (order_qual_clauses).)
        Expr::Subquery(s) if initplan_sublink(s) => {}
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

/// Whether a FULL JOIN on these clauses has a path (populate_joinrel_with_paths: "FULL JOIN
/// is only supported with merge-joinable or hash-joinable join conditions"). `has_keys`: it
/// has hash keys (built-in, hashable `=` across the sides); `cond`: its other ON clauses.
/// Taking either input as the outer one, a merge join needs every clause but constants to be
/// a merge-joinable `=` across the sides (select_mergejoin_clauses), a hash join one
/// hash-joinable `=` (hash_inner_and_outer); an operator without a commutator only with the
/// outer side on its left. A constant false clause makes the join a dummy one. Clauses not
/// recognised count as both (no error Postgres might not raise).
pub(crate) fn full_join_supported(cat: &Catalog, cond: Option<&Expr>, has_keys: bool, nleft: usize) -> bool {
    if has_keys {
        return true;
    }
    let parts: Vec<&Expr> = match cond {
        None => return true,
        Some(Expr::And(ps)) => ps.iter().collect(),
        Some(e) => vec![e],
    };
    // (mergeable, hashable, the `=`'s left operand's side: Some(true) the join's left input,
    // None when the operator commutes).
    let mut clauses: Vec<(bool, bool, Option<bool>)> = Vec::new();
    for p in parts {
        let info = match p {
            Expr::Const(Value::Bool(false) | Value::Null, _) => return true,
            Expr::Const(..) => continue,
            Expr::Call(d, args, _) if d.name == "=" && args.len() == 2 => {
                let (a, b) = (&args[0], &args[1]);
                let left = match (side(a, nleft), side(b, nleft)) {
                    (Side::Left, Side::Right) => true,
                    (Side::Right, Side::Left) => false,
                    _ => {
                        clauses.push((false, false, None));
                        continue;
                    }
                };
                if p.is_volatile() {
                    (false, false, None)
                } else if let Some((m, h, comm)) = crate::sql::paths::user_eq_props(cat, d) {
                    (m, h, (!comm).then_some(left))
                } else {
                    use DataType as T;
                    let (ta, tb) = (a.ty(), b.ty());
                    let geo = |t: &DataType| matches!(t, T::Lseg | T::Path | T::GBox | T::Line | T::Circle);
                    let datetime = |t: &DataType| matches!(t, T::Date | T::Timestamp(_) | T::Timestamptz(_));
                    if geo(&ta) || (matches!(ta, T::Xid) && !matches!(tb, T::Xid)) {
                        (false, false, None)
                    } else if matches!(ta, T::Xid | T::Cid | T::Aclitem) {
                        (false, true, None)
                    } else if matches!(ta, T::Money | T::Bit(_) | T::Varbit(_) | T::TsVector | T::TsQuery)
                        || (datetime(&ta) && datetime(&tb) && ta.without_typmod() != tb.without_typmod())
                    {
                        (true, false, None)
                    } else {
                        (true, true, None)
                    }
                }
            }
            Expr::Call(..)
            | Expr::Or(_)
            | Expr::Not(_)
            | Expr::IsNull(..)
            | Expr::BoolTest(..)
            | Expr::IsDistinct(..)
            | Expr::InList(..)
            | Expr::ArrayOp { .. }
            | Expr::Col(..) => (false, false, None),
            // (Nor is any other expression over the sides' columns: a NULLIF, CASE,
            // COALESCE.)
            // (An outer join's clause of no column is no gating qual but a join clause too:
            // a sub-select alone is one.)
            Expr::Subquery(_) => (false, false, None),
            Expr::NullIf(..) | Expr::Case { .. } | Expr::Coalesce(..) | Expr::Field(..) if !p.columns().is_empty() => {
                (false, false, None)
            }
            _ => (true, true, None),
        };
        clauses.push(info);
    }
    [true, false].into_iter().any(|outer_left| {
        let usable = |c: &(bool, bool, Option<bool>)| c.2.is_none_or(|l| l == outer_left);
        clauses.iter().all(|c| c.0 && usable(c)) || clauses.iter().any(|c| c.1 && usable(c))
    })
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
/// Clauses onto a join's right side; a LATERAL subquery planned apart takes none reading
/// the left side's values (qual_is_pushdown_safe refuses lateral references): they stay its
/// Subquery Scan's filter.
/// generate_base_implied_equalities_const over an inner join's conjuncts: an equivalence
/// class with a constant and more than one other member (or more than one source clause)
/// gives each member `member = constant`, the classes in the order they were made (by the
/// clauses in order, a merged class keeping its left operand's place); a class of one
/// clause gives that clause back as written, at its class's place. The `column = constant`
/// conjuncts are reoriented so and put in that order (in the places they had).
/// Returns each column member's class place.
///
/// A join clause between the sides (`nleft` columns on the left) is between each side's
/// first member of its class (generate_join_implied_equalities_normal), a same-side
/// equality before it making the class's first member another column.
fn ec_const_order(parts: &mut [Expr], nleft: usize) -> Vec<(usize, usize)> {
    // Members: columns and non-null constants of `=` clauses of one type family.
    let operands = |e: &Expr| -> Option<(Expr, Expr)> {
        let Expr::Call(def, args, _) = e else { return None };
        if def.name != "=" || args.len() != 2 || !ec_clause(e) || !def.strict {
            return None;
        }
        let ok = |x: &Expr| matches!(x, Expr::Col(..)) || matches!(x, Expr::Const(v, _) if !v.is_null());
        (ok(&args[0]) && ok(&args[1]) && args[0].ty().without_typmod() == args[1].ty().without_typmod())
            .then(|| (args[0].clone(), args[1].clone()))
    };
    let mut members: Vec<Expr> = Vec::new();
    let mut class: Vec<usize> = Vec::new();
    // Per class: its place (creation order) and how many source clauses it has.
    let mut place: Vec<usize> = Vec::new();
    let mut sources: Vec<usize> = Vec::new();
    let member = |e: &Expr, members: &mut Vec<Expr>, class: &mut Vec<usize>| -> usize {
        members.iter().position(|m| m.same_as(e)).unwrap_or_else(|| {
            members.push(e.clone());
            class.push(usize::MAX);
            members.len() - 1
        })
    };
    let mut classed: Vec<bool> = vec![false; parts.len()];
    let mut next_place = 0;
    for (i, p) in parts.iter().enumerate() {
        let Some((a, b)) = operands(p) else { continue };
        if matches!(a, Expr::Const(..)) && matches!(b, Expr::Const(..)) {
            continue;
        }
        let ia = member(&a, &mut members, &mut class);
        let ib = member(&b, &mut members, &mut class);
        let (ca, cb) = (class[ia], class[ib]);
        let c = match (ca, cb) {
            (usize::MAX, usize::MAX) => {
                place.push(next_place);
                sources.push(0);
                next_place += 1;
                let c = place.len() - 1;
                class[ia] = c;
                class[ib] = c;
                c
            }
            (x, usize::MAX) => {
                class[ib] = x;
                x
            }
            (usize::MAX, y) => {
                class[ia] = y;
                y
            }
            (x, y) => {
                // (The left operand's class absorbs the other.)
                for k in class.iter_mut() {
                    if *k == y {
                        *k = x;
                    }
                }
                sources[x] += sources[y];
                x
            }
        };
        sources[c] += 1;
        classed[i] = true;
    }
    let size = |c: usize| class.iter().filter(|k| **k == c).count();
    // The `column = constant` conjuncts: their classes' places, oriented member first when
    // the class's clauses are generated.
    let mut slots: Vec<usize> = Vec::new();
    let mut items: Vec<(usize, usize, Expr)> = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let Some((a, b)) = operands(p) else { continue };
        if !classed[i] {
            continue;
        }
        // (Its class now: after any merge.)
        let Some(c) = members.iter().position(|m| m.same_as(&a)).map(|k| class[k]) else { continue };
        let (col, k, flipped) = match (&a, &b) {
            (Expr::Col(..), Expr::Const(..)) => (a.clone(), b.clone(), false),
            (Expr::Const(..), Expr::Col(..)) => (b.clone(), a.clone(), true),
            _ => continue,
        };
        let generated = size(c) > 2 || sources[c] > 1;
        let e = if generated && flipped {
            let Expr::Call(def, _, ty) = p else { continue };
            Expr::Call(def.clone(), vec![col, k], ty.clone())
        } else {
            p.clone()
        };
        slots.push(i);
        items.push((place[c], i, e));
    }
    items.sort_by_key(|(pl, i, _)| (*pl, *i));
    for (slot, (_, _, e)) in slots.into_iter().zip(items) {
        parts[slot] = e;
    }
    // (Only through equalities of one side's columns, which that side checks itself: a
    // member the class has only through other join clauses isn't equal to it there.)
    let mut side: Vec<usize> = (0..members.len()).collect();
    fn root(side: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while side[r] != r {
            r = side[r];
        }
        side[i] = r;
        r
    }
    for (i, p) in parts.iter().enumerate() {
        if !classed[i] {
            continue;
        }
        let Expr::Call(_, args, _) = p else { continue };
        let (Expr::Col(x, _), Expr::Col(y, _)) = (&args[0], &args[1]) else { continue };
        if (*x < nleft) != (*y < nleft) {
            continue;
        }
        let (Some(a), Some(b)) = (
            members.iter().position(|m| m.same_as(&args[0])),
            members.iter().position(|m| m.same_as(&args[1])),
        ) else {
            continue;
        };
        let (ra, rb) = (root(&mut side, a), root(&mut side, b));
        side[rb] = ra;
    }
    let mut first_of = |e: &Expr| {
        let k = members.iter().position(|m| m.same_as(e))?;
        let r = root(&mut side, k);
        (0..members.len()).find(|&j| root(&mut side, j) == r).map(|j| members[j].clone())
    };
    for (i, p) in parts.iter_mut().enumerate() {
        if !classed[i] {
            continue;
        }
        let Expr::Call(def, args, ty) = &*p else { continue };
        let (Expr::Col(x, _), Expr::Col(y, _)) = (&args[0], &args[1]) else { continue };
        if (*x < nleft) == (*y < nleft) {
            continue;
        }
        let (Some(a), Some(b)) = (first_of(&args[0]), first_of(&args[1])) else { continue };
        if a.same_as(&args[0]) && b.same_as(&args[1]) {
            continue;
        }
        *p = Expr::Call(def.clone(), vec![a, b], ty.clone());
    }
    members
        .iter()
        .zip(&class)
        .filter_map(|(m, &c)| match m {
            Expr::Col(i, _) if c != usize::MAX => Some((*i, place[c])),
            _ => None,
        })
        .collect()
}

/// A side's `column = constant` clauses put in their classes' order (`places`, over the
/// join's row; the side's columns from `off`), in the places they have.
fn order_by_places(list: &mut [Expr], places: &[(usize, usize)], off: usize) {
    let place_of = |e: &Expr| -> Option<usize> {
        let Expr::Call(def, args, _) = e else { return None };
        if def.name != "=" || args.len() != 2 {
            return None;
        }
        let col = match (&args[0], &args[1]) {
            (Expr::Col(i, _), Expr::Const(..)) | (Expr::Const(..), Expr::Col(i, _)) => *i,
            _ => return None,
        };
        places.iter().find(|(c, _)| *c == col + off).map(|(_, p)| *p)
    };
    let mut slots = Vec::new();
    let mut items = Vec::new();
    for (i, e) in list.iter().enumerate() {
        if let Some(p) = place_of(e) {
            slots.push(i);
            items.push((p, i, e.clone()));
        }
    }
    items.sort_by_key(|(p, i, _)| (*p, *i));
    for (slot, (_, _, e)) in slots.into_iter().zip(items) {
        list[slot] = e;
    }
}

/// `nonnull`: per LATERAL parameter, whether it is a NOT NULL column no outer join nulls.
fn push_lateral(cat: &Catalog, right: Plan, parts: Vec<Expr>, params: &[Expr], nonnull: &[bool]) -> Plan {
    let lateral = !params.is_empty();
    let right = match push_lateral_union(cat, right, &parts, nonnull) {
        Ok(p) => return p,
        Err(p) => p,
    };
    if lateral && matches!(right, Plan::Barrier { secure: false, .. }) {
        let (up, down): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(Expr::has_outer_refs);
        let right = if down.is_empty() { right } else { add_filter(cat, right, Expr::And(down)) };
        return match and_all(up) {
            Some(pred) => Plan::Filter { input: Arc::new(right), pred },
            None => right,
        };
    }
    if !lateral {
        return add_filter(cat, right, Expr::And(parts));
    }
    // (Deeper down, a subquery planned apart (a UNION ALL arm's) refuses them too: the
    // join's values a clause reads as `Outer(1, k)`.)
    lateral_push(params);
    let out = add_filter(cat, right, Expr::And(parts));
    lateral_pop();
    out
}

/// Entering a LATERAL join's right side (its values `params`, read as `Outer(1, k)`):
/// clauses `add_filter` pushes there don't go into a subquery level when they read them.
pub fn lateral_push(params: &[Expr]) {
    LATERAL_PARAMS.with(|s| s.borrow_mut().push(params.to_vec()));
}

/// Leaving the LATERAL right side `lateral_push` entered.
pub fn lateral_pop() {
    LATERAL_PARAMS.with(|s| s.borrow_mut().pop());
}

thread_local! {
    /// The LATERAL values of the joins whose right sides `add_filter` is inside (innermost
    /// last).
    static LATERAL_PARAMS: std::cell::RefCell<Vec<Vec<Expr>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether a clause reads a LATERAL value of a relation of this query (a join's value that
/// is one of its relations' columns, not an outer query's), inside a LATERAL right side.
fn reads_lateral_var(e: &Expr) -> bool {
    LATERAL_PARAMS.with(|s| {
        let s = s.borrow();
        let Some(top) = s.last() else { return false };
        let mut found = false;
        e.walk(&mut |x| {
            if let Expr::Outer(1, k, _) = x
                && top.get(*k).is_none_or(|p| !p.columns().is_empty())
            {
                found = true;
            }
        });
        found
    })
}

/// Per LATERAL parameter of a join over `left`: a NOT NULL column of `left` no outer join
/// nulls (restriction_is_always_true of a NullTest of it: its Var has no nullingrels).
fn nonnull_params(left: &Plan, params: &[Expr]) -> Vec<bool> {
    params
        .iter()
        .map(|e| matches!(e, Expr::Col(c, _) if nonnullable_col(left, *c)))
        .collect()
}

/// set_append_rel_size over a LATERAL UNION ALL of FROM-less arms: the clauses go into each
/// arm, and in an arm that outputs a parameter `nonnull` marks, a NullTest of it is always
/// true (restriction_is_always_true: dropped) or false (the arm goes). Done here, where the
/// join's parameters are known, only when that changes an arm (else the planner pushes the
/// clauses into the arms as usual): Err gives `right` back.
fn push_lateral_union(cat: &Catalog, right: Plan, parts: &[Expr], nonnull: &[bool]) -> std::result::Result<Plan, Plan> {
    if !nonnull.iter().any(|b| *b) {
        return Err(right);
    }
    let Plan::Append { inputs, inh: None, alias, .. } = &right else {
        return Err(right);
    };
    let safe = parts.iter().all(|p| {
        let mut ok = !p.is_volatile();
        p.walk(&mut |x| ok &= !matches!(x, Expr::Subquery(_) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..)));
        ok
    });
    if !safe || inputs.len() < 2 {
        return Err(right);
    }
    let mut arms = Vec::new();
    let mut changed = false;
    for a in inputs {
        let Plan::Project { input, exprs } = &**a else {
            return Err(right);
        };
        if !matches!(&**input, Plan::One) {
            return Err(right);
        }
        let mut qs: Vec<Expr> = parts
            .iter()
            .map(|p| {
                p.clone()
                    .transform(&mut |e| {
                        Ok(match e {
                            Expr::Col(i, t) => exprs.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                            e => e,
                        })
                    })
                    .unwrap()
            })
            .collect();
        let before: Vec<Expr> = qs.clone();
        reduce_null_tests(&mut qs, &|x| matches!(x, Expr::Outer(1, i, _) if nonnull.get(*i).copied().unwrap_or(false)));
        changed |= qs.len() != before.len() || qs.iter().zip(&before).any(|(a, b)| !a.same_as(b));
        arms.push((a.clone(), exprs.clone(), qs));
    }
    if !changed {
        return Err(right);
    }
    let mut out = Vec::new();
    for (arm, exprs, qs) in arms {
        if qs.iter().any(super::refute::always_false) {
            continue;
        }
        out.push(match and_all(qs) {
            None => arm,
            Some(q) => Arc::new(Plan::Project {
                input: Arc::new(add_filter(cat, Plan::One, q)),
                exprs,
            }),
        });
    }
    let ncols = width(&right);
    Ok(match out.len() {
        0 => Plan::Empty { ncols, cols: None, info: None },
        1 => Arc::try_unwrap(out.pop().unwrap()).unwrap_or_else(|a| (*a).clone()),
        _ => Plan::Append { inputs: out, inh: None, alias: alias.clone(), pw: None },
    })
}

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
    let deferred = !params.is_empty() && crate::sql::plan::deferred_lateral(&right);
    join_lateral(cat, kind, left, right, cond, params, nleft, nright, deferred)
}

/// `join`, `deferred` saying whether the right side is a LATERAL subquery left unplanned
/// for the join group above to pull up (`JoinMeta::lateral_sub`): an outer join's ON
/// clauses stay the join's then (the group distributes them as Postgres does after
/// pull_up_subqueries, outer-join clauses: no equivalence class of them).
#[allow(clippy::too_many_arguments)]
pub fn join_lateral(
    cat: &Catalog,
    kind: JoinKind,
    left: Plan,
    right: Plan,
    cond: Option<Expr>,
    params: Vec<Expr>,
    nleft: usize,
    nright: usize,
    deferred: bool,
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
    // (A LATERAL subquery the join group above pulls up takes none of an outer join's
    // clauses: they are the group's, as Postgres distributes them after pull_up_subqueries.)
    let deferred = deferred && !params.is_empty();
    let hold = deferred && !matches!(kind, JoinKind::Inner);
    for p in parts {
        if p.is_volatile() && p.columns().is_empty() {
            rest.push(p);
            continue;
        }
        match side(&p, nleft) {
            // ON conditions on the preserved side of an outer join filter nothing. A semi
            // join's conditions are pushed down as an inner join's (deconstruct_jointree:
            // ojscope NULL for JOIN_SEMI): its left side's restrictions.
            Side::Left if matches!(kind, JoinKind::Inner | JoinKind::Right | JoinKind::Semi) => lpush.push(p),
            Side::Right
                if matches!(
                    kind,
                    JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti
                ) && right_pushable(&p, &params)
                    && !hold =>
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
    if !params.is_empty() && !hold && matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti) {
        // (Only those over the relations its LATERAL references read: the clauses of its
        // parameterized path, whose required relations are its lateral_relids
        // (get_baserel_parampathinfo). The others are the join's.)
        let sources: Option<Vec<(usize, usize)>> = Some({
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
    let mut broken = Vec::new();
    if matches!(kind, JoinKind::Inner | JoinKind::Semi) && params.is_empty() {
        let mut no_hash = None;
        let (lhave, rhave) = (input_restrictions(&left), input_restrictions(&right));
        let (lcoll, rcoll) = (|c: usize| output_collation(&left, c), |c: usize| output_collation(&right, c));
        let mut rewrites = Vec::new();
        broken = ec_constants(
            cat,
            &mut lpush,
            &mut rpush,
            &mut no_hash,
            &mut rest,
            nleft,
            (true, true),
            &mut kept,
            (&lhave, &rhave),
            (&lcoll, &rcoll),
            &mut rewrites,
        );
        for (on_right, old, new) in rewrites {
            replace_restriction(cat, if on_right { &mut right } else { &mut left }, &old, &new);
        }
    }
    // reconsider_outer_join_clause: an outer join's `outer = inner` whose outer column its
    // side equates with a constant (in any join domain below) gives the nullable side
    // `inner = constant`; the clause itself stays (the join search drops it where it is
    // the constant-TRUE clause, `oj_const_redundant`).
    // (An anti join's too: its clauses are a left join's, left_join_clauses.)
    if params.is_empty() && matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Anti) {
        let have: Vec<Expr> = if kind != JoinKind::Right {
            restrictions(&left, true)
        } else {
            restrictions(&right, true).into_iter().map(|e| e.shift_cols(nleft as isize)).collect()
        };
        for p in &rest {
            let Some((l, r)) = equi_key(p, nleft) else { continue };
            let (ov, iv) = if kind != JoinKind::Right { (l, r) } else { (r, l) };
            if iv.ty().without_typmod() != ov.ty().without_typmod() {
                continue;
            }
            for h in &have {
                let Some((x, k, def)) = col_const(h) else { continue };
                if !x.same_as(&ov) {
                    continue;
                }
                let d = Expr::Call(def, vec![iv.clone(), k], DataType::Bool);
                if kind != JoinKind::Right {
                    let d = d.shift_cols(-(nleft as isize));
                    if !rpush.iter().any(|e| e.same_as(&d)) {
                        rpush.push(d);
                    }
                } else if !lpush.iter().any(|e| e.same_as(&d)) {
                    lpush.push(d);
                }
                break;
            }
        }
    }
    let saved_broken = BROKEN_DUPS.with(|b| b.replace(broken));
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
        let nonnull = nonnull_params(&left, &params);
        right = push_lateral(cat, right, rpush, &params, &nonnull);
    }
    BROKEN_DUPS.with(|b| *b.borrow_mut() = saved_broken);
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
            lateral_held: deferred,
            ..Default::default()
        }),
    }
}

/// `x = k`, a column (through a relabeling) equated with a constant, nothing volatile: the
/// column, the constant and the `=`.
fn col_const(e: &Expr) -> Option<(Expr, Expr, Arc<crate::sql::func::FuncDef>)> {
    let Expr::Call(d, args, _) = e else { return None };
    if d.name != "=" || args.len() != 2 || e.is_volatile() {
        return None;
    }
    let colish = |x: &Expr| match x {
        Expr::Col(..) => true,
        Expr::Relabel(y, _) => matches!(&**y, Expr::Col(..)),
        _ => false,
    };
    match (&args[0], &args[1]) {
        (x, k @ Expr::Const(..)) if colish(x) => Some((x.clone(), k.clone(), d.clone())),
        (k @ Expr::Const(..), x) if colish(x) && args[0].ty().without_typmod() == args[1].ty().without_typmod() => {
            Some((x.clone(), k.clone(), d.clone()))
        }
        _ => None,
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
    // (A made type's `=` added to a built-in family: its own function. Not a CREATE
    // OPERATOR `=` of built-in types over some other built-in function.)
    if func_is_builtin_for_builtin_types(def, &args[0].ty(), &args[1].ty()) && !builtin_eq(def, &args[0].ty(), &args[1].ty()) {
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
    if def.name != "=" || args.len() != 2 || !builtin_eq(def, &args[0].ty(), &args[1].ty()) {
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
    if !pg_hashable(&ta)
        || (ta.without_typmod() != tb.without_typmod() && !(int(&ta) && int(&tb)) && !name_text_pair(&ta, &tb))
    {
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

/// Whether `def` is the built-in `=` for these operand types (the equality their hash and
/// btree operator classes have), not a CREATE OPERATOR `=` shown under that name.
fn builtin_eq(def: &crate::sql::func::FuncDef, l: &DataType, r: &DataType) -> bool {
    crate::sql::func::resolve_binop("=", l, r).is_ok_and(|res| res.def.oid == def.oid)
}

/// Both operands of built-in types: an `=` over them that isn't the built-in one is a user's.
fn func_is_builtin_for_builtin_types(_def: &crate::sql::func::FuncDef, l: &DataType, r: &DataType) -> bool {
    l.oid() < crate::catalog::FIRST_USER_OID && r.oid() < crate::catalog::FIRST_USER_OID
}

/// `name = text` / `text = name` (nameeqtext, texteqname): the hash text_ops family has
/// both, hashname and hashtext hashing equal strings alike, so a hash join takes it. (Not
/// a merge key here: the text side would sort under the name side's C collation.)
pub(crate) fn name_text_pair(a: &DataType, b: &DataType) -> bool {
    matches!(
        (a.without_typmod(), b.without_typmod()),
        (DataType::Name, DataType::Text) | (DataType::Text, DataType::Name)
    )
}

/// Types a hash join can use (hashing agrees with their `=`).
pub(crate) fn hashable_type(t: &DataType) -> bool {
    hashable(t)
}

/// Types Postgres hashes an `=` of (`catalog::userobj::type_hashable`): hash join keys and
/// hashed SubPlans. (The engine's keys compare and hash as `=` does for all of them:
/// numerics by value, bpchar without trailing spaces, arrays, ranges and jsonb by their
/// elements.)
fn pg_hashable(t: &DataType) -> bool {
    crate::catalog::userobj::type_hashable(t, &|_| false)
}

/// Types whose `=` is exactly equality of the stored value (so hashing agrees with it).
fn hashable(t: &DataType) -> bool {
    // (An extension's native type hashes by its canonical bytes, as its hash class does.)
    if let DataType::Base(b) = t
        && b.ext != 0
    {
        return crate::ext::types::get(b.ext).is_some_and(|x| x.hash);
    }
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

/// A pseudoconstant clause that isn't a constant (RestrictInfo.pseudoconstant): no column
/// of the query level, nothing volatile, a sub-select's only as an InitPlan's output.
/// clause_selectivity_ext counts it 1.0: it gates the plan, filtering no rows.
pub fn pseudo_gate(e: &Expr) -> bool {
    if matches!(e, Expr::Const(..)) || !e.columns().is_empty() {
        return false;
    }
    let mut ok = pseudo_constant(e);
    e.walk(&mut |x| ok &= !matches!(x, Expr::Subquery(s) if !s.params.is_empty()));
    ok
}

/// `is_pseudo_constant_clause`: no columns of the query's own rows, nothing volatile.
pub fn pseudo_constant(e: &Expr) -> bool {
    // An uncorrelated subquery is an InitPlan's output parameter.
    let mut vars = false;
    e.walk(&mut |x| {
        vars |= match x {
            Expr::Col(..) | Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Grouping(..) => true,
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
