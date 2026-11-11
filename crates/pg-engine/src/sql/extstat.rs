//! Extended statistics in planning: a relation's statistics objects (get_relation_statistics),
//! expression statistics for examine_variable, estimate_num_groups with its multivariate
//! n-distinct coefficients (selfuncs.c), and clause selectivity from MCV lists and functional
//! dependencies (clausesel.c, extended_stats.c, mcv.c, dependencies.c).

use crate::catalog::{StatExtData, Table};
use crate::types::{DataType, Value};
use crate::exec::ExecCtx;
use crate::sql::costs::{self, VarRef};
use crate::sql::expr::Expr;
use std::collections::BTreeSet;
use std::sync::Arc;

/// One statistics object of a relation (StatisticExtInfo, all its kinds together).
pub struct StatInfo {
    pub oid: u32,
    /// pg_statistic_ext.stxkind.
    pub kinds: Vec<char>,
    /// Plain column keys (column indexes).
    pub keys: Vec<usize>,
    /// The expressions, bound over the table's columns.
    pub exprs: Vec<Expr>,
    pub data: StatExtData,
}

/// The statistics objects on `t` built for `inh` (with its children or not), by oid.
pub fn statlist(ctx: &ExecCtx, t: &Arc<Table>, inh: bool) -> Vec<StatInfo> {
    let cat = ctx.catalog();
    let mut out = Vec::new();
    for st in cat.statistics.values().filter(|s| s.table == t.oid) {
        let Some(data) = st.data.iter().find(|d| d.inherit == inh) else {
            continue;
        };
        let mut b = crate::sql::binder::Binder::new(ctx, Vec::new());
        let exprs: crate::error::Result<Vec<Expr>> = st
            .exprs
            .iter()
            .map(|e| {
                let n = crate::sql::binder::ast::parse_expr(e)?;
                b.bind_over_table(t, &n, crate::sql::binder::Clause::StatsExpr)
            })
            .collect();
        let Ok(exprs) = exprs else {
            continue;
        };
        out.push(StatInfo {
            oid: st.oid,
            kinds: st.stxkind(),
            keys: st.columns.clone(),
            exprs,
            data: data.clone(),
        });
    }
    out
}

/// examine_variable for an expression over `t`'s columns: the statistics of an index on
/// the same expression, else of a statistics object's expression.
pub fn expr_var(ctx: &ExecCtx, t: &Arc<Table>, tuples: f64, e: &Expr) -> Option<VarRef> {
    let cat = ctx.catalog();
    let var = |rel: u32, col: usize| VarRef {
        table: t.clone(),
        col: 0,
        tuples,
        expr: Some((rel, col, e.ty())),
        rows: 0.0,
        inh: false,
    };
    if let Ok(ixs) = crate::sql::binder::bound_indexes(t, ctx) {
        for bix in &ixs {
            for (k, key) in bix.keys.iter().enumerate() {
                if !matches!(key, Expr::Col(..))
                    && key.same_as(e)
                    && costs::col_stats(&cat, bix.index.oid, k).is_some()
                {
                    return Some(var(bix.index.oid, k));
                }
            }
        }
    }
    for info in statlist(ctx, t, false) {
        if !info.kinds.contains(&'e') {
            continue;
        }
        if let Some(pos) = info.exprs.iter().position(|x| strip_relabel(x).same_as(e))
            && costs::col_stats(&cat, info.oid, pos).is_some()
        {
            return Some(var(info.oid, pos));
        }
    }
    None
}

/// Without a binary-compatible relabeling (RelabelType).
fn strip_relabel(e: &Expr) -> &Expr {
    costs::strip(e)
}

/// A GroupVarInfo: a column or an expression (with statistics of its own) of one relation.
pub struct GroupVar {
    pub table: Arc<Table>,
    pub tuples: f64,
    /// Over the table's columns (Col(c) for a column); None: one not to match statistics
    /// objects against, `key` telling duplicates apart.
    pub expr: Option<Expr>,
    pub key: (u32, usize),
    pub ndistinct: f64,
    /// Over an inheritance tree (its inherited statistics objects' data).
    pub inh: bool,
    /// The relation's rows after its restrictions (RelOptInfo.rows; 0: not known).
    pub rows: f64,
}

impl GroupVar {
    pub fn of(cat: &crate::catalog::Catalog, v: &VarRef) -> GroupVar {
        GroupVar {
            table: v.table.clone(),
            tuples: v.tuples,
            expr: match v.expr {
                None => Some(Expr::Col(v.col, v.table.columns[v.col].ty.clone())),
                Some(_) => None,
            },
            key: match &v.expr {
                Some((r, c, _)) => (*r, *c),
                None => (v.table.oid, v.col),
            },
            ndistinct: v.ndistinct(cat).0,
            inh: v.inh,
            rows: v.rows,
        }
    }

    fn same(&self, o: &GroupVar) -> bool {
        self.table.oid == o.table.oid
            && match (&self.expr, &o.expr) {
                (Some(a), Some(b)) => a.same_as(b),
                (None, None) => self.key == o.key,
                _ => false,
            }
    }
}

/// add_unique_group_var: `v` unless an equal one is there already.
pub fn add_unique(vars: &mut Vec<GroupVar>, v: GroupVar) {
    if !vars.iter().any(|x| x.same(&v)) {
        vars.push(v);
    }
}

/// The rest of estimate_num_groups once the group expressions are variables: per relation,
/// the product of the distinct counts (a statistics object's for the columns it covers),
/// clamped, scaled down when the relation's filtered rows are fewer than its tuples (its
/// variables' `rows`; else `rows`, for one relation). `numdistinct`: the factor the other
/// expressions contribute.
pub fn num_groups(
    ctx: &ExecCtx,
    mut vars: Vec<GroupVar>,
    mut numdistinct: f64,
    in_rows: f64,
    rows: Option<f64>,
) -> f64 {
    let in_rows = costs::clamp_rows(in_rows);
    let single = {
        let first = vars.first().map(|v| v.table.oid);
        vars.iter().all(|v| Some(v.table.oid) == first)
    };
    while !vars.is_empty() {
        let rel = vars[0].table.clone();
        let tuples = vars[0].tuples;
        let inh = vars[0].inh;
        // (RelOptInfo.rows of the relation; not known: the one relation's input rows.)
        let rel_rows = match vars[0].rows {
            r if r > 0.0 => Some(r),
            _ if single => rows,
            _ => None,
        };
        let (mut relvars, rest): (Vec<GroupVar>, Vec<GroupVar>) =
            vars.into_iter().partition(|v| v.table.oid == rel.oid);
        vars = rest;
        let mut reldistinct = 1.0;
        let mut relmax: f64 = 1.0;
        let mut relvarcount = 0;
        let stats: Vec<StatInfo> = if relvars.iter().filter(|v| v.expr.is_some()).count() > 1 {
            statlist(ctx, &rel, inh)
                .into_iter()
                .filter(|s| s.kinds.contains(&'d'))
                .collect()
        } else {
            Vec::new()
        };
        while !relvars.is_empty() {
            match multivariate_ndistinct(&stats, &mut relvars) {
                Some(nd) => {
                    reldistinct *= nd;
                    relmax = relmax.max(nd);
                    relvarcount += 1;
                }
                None => {
                    for v in relvars.drain(..) {
                        reldistinct *= v.ndistinct;
                        relmax = relmax.max(v.ndistinct);
                        relvarcount += 1;
                    }
                }
            }
        }
        if tuples > 0.0 {
            let mut clamp = tuples;
            if relvarcount > 1 {
                clamp *= 0.1;
                if clamp < relmax {
                    clamp = relmax.min(tuples);
                }
            }
            if reldistinct > clamp {
                reldistinct = clamp;
            }
            // The relation's rows after its restrictions: fewer of its values show up.
            // (Dell'Era's estimate of the distinct values a sample of the rows holds.)
            if let Some(rows) = rel_rows
                && reldistinct > 0.0
                && rows < tuples
            {
                reldistinct *= 1.0 - ((tuples - rows) / tuples).powf(tuples / reldistinct);
            }
            numdistinct *= costs::clamp_rows(reldistinct);
        }
    }
    numdistinct.ceil().min(in_rows).max(1.0)
}

/// estimate_multivariate_bucketsize for the hash keys `vars` of one relation's columns:
/// the largest n-distinct coefficient its statistics objects give for them (1 when none
/// does), and the positions of the keys left to estimate one by one (exact duplicates
/// dropped).
pub fn bucket_ndistinct(ctx: &ExecCtx, vars: Vec<GroupVar>) -> (f64, Vec<usize>) {
    let mut uniq: Vec<(usize, GroupVar)> = Vec::new();
    for (i, v) in vars.into_iter().enumerate() {
        if !uniq.iter().any(|(_, x)| x.same(&v)) {
            uniq.push((i, v));
        }
    }
    if uniq.len() < 2 {
        return (1.0, uniq.into_iter().map(|(i, _)| i).collect());
    }
    let rel = uniq[0].1.table.clone();
    let stats: Vec<StatInfo> = statlist(ctx, &rel, uniq[0].1.inh)
        .into_iter()
        .filter(|s| s.kinds.contains(&'d'))
        .collect();
    let keys: Vec<(usize, (u32, usize))> = uniq.iter().map(|(i, v)| (*i, v.key)).collect();
    let mut vars: Vec<GroupVar> = uniq.into_iter().map(|(_, v)| v).collect();
    let mut nd: f64 = 1.0;
    while let Some(d) = multivariate_ndistinct(&stats, &mut vars) {
        nd = nd.max(d);
    }
    let left = keys
        .into_iter()
        .filter(|(_, k)| vars.iter().any(|v| v.key == *k))
        .map(|(i, _)| i)
        .collect();
    (nd, left)
}

/// estimate_multivariate_ndistinct: the n-distinct coefficient of the statistics object
/// covering the most of `vars` (expressions first), taking those out of `vars`.
fn multivariate_ndistinct(stats: &[StatInfo], vars: &mut Vec<GroupVar>) -> Option<f64> {
    let mut best: Option<(&StatInfo, usize, usize)> = None;
    for info in stats {
        let (mut nvars, mut nexprs) = (0, 0);
        for v in vars.iter() {
            match &v.expr {
                Some(Expr::Col(c, _)) => {
                    if info.keys.contains(c) {
                        nvars += 1;
                    }
                }
                Some(e) if info.exprs.iter().any(|x| x.same_as(e)) => nexprs += 1,
                _ => {}
            }
        }
        if nvars + nexprs < 2 {
            continue;
        }
        let better = match best {
            None => true,
            Some((_, bv, be)) => nexprs > be || (nexprs == be && nvars > bv),
        };
        if better {
            best = Some((info, nvars, nexprs));
        }
    }
    let (info, ..) = best?;
    let items = crate::types::extstats::ndistinct_items(info.data.ndistinct.as_deref()?)?;
    let offset: i32 = if info.exprs.is_empty() {
        0
    } else {
        info.exprs.len() as i32 + 1
    };
    let attnum = |v: &GroupVar| -> Option<i32> {
        match &v.expr {
            Some(Expr::Col(c, _)) => info.keys.contains(c).then_some(*c as i32 + 1 + offset),
            Some(e) => info
                .exprs
                .iter()
                .position(|x| x.same_as(e))
                .map(|i| -(i as i32 + 1) + offset),
            None => None,
        }
    };
    let matched: BTreeSet<i32> = vars.iter().filter_map(attnum).collect();
    let item = items.iter().find(|(attrs, _)| {
        attrs.len() == matched.len()
            && attrs.iter().all(|a| matched.contains(&(*a as i32 + offset)))
    })?;
    vars.retain(|v| attnum(v).is_none());
    Some(item.1)
}

// ---- Clause selectivity (clausesel.c with extended_stats.c, mcv.c and dependencies.c).

/// Restriction estimators (pg_operator.oprrest) extended statistics handle.
const F_EQSEL: u32 = 101;
const SCALAR_RESTS: [u32; 6] = [101, 102, 103, 104, 336, 337];

fn oprrest(d: &crate::sql::func::FuncDef) -> u32 {
    crate::sql::func::oper_row(d.name, d).rest
}

/// A call of an operator (rather than of a function).
fn is_operator(d: &crate::sql::func::FuncDef) -> bool {
    d.name.chars().next().is_some_and(|c| !c.is_alphanumeric() && c != '_')
}

/// is_pseudo_constant_clause: no columns and nothing volatile.
fn pseudo_const(e: &Expr) -> bool {
    crate::sql::binder::optimize::pseudo_constant(e)
}

/// examine_opclause_args: (the non-constant side, the constant, its type, is the constant
/// on the right).
fn opclause_args(args: &[Expr]) -> Option<(&Expr, &Value, &DataType, bool)> {
    let l = strip_relabel(&args[0]);
    let r = strip_relabel(&args[1]);
    match (l, r) {
        (_, Expr::Const(v, t)) => Some((l, v, t, true)),
        (Expr::Const(v, t), _) => Some((r, v, t, false)),
        _ => None,
    }
}

/// A constant array's elements (with their type).
fn array_consts(e: &Expr) -> Option<Vec<(Value, DataType)>> {
    match strip_relabel(e) {
        Expr::Array(items, _) => items
            .iter()
            .map(|i| match strip_relabel(i) {
                Expr::Const(v, t) => Some((v.clone(), t.clone())),
                _ => None,
            })
            .collect(),
        Expr::Const(Value::Array(a), ty) => {
            let et = ty.elem().cloned().unwrap_or(DataType::Unknown);
            Some(a.items.iter().map(|v| (v.clone(), et.clone())).collect())
        }
        Expr::Const(Value::Null, _) => Some(Vec::new()),
        _ => None,
    }
}

fn clamp_probability(s: f64) -> f64 {
    s.clamp(0.0, 1.0)
}

/// mcv_combine_selectivities.
fn mcv_combine(simple: f64, mcv: f64, basesel: f64, totalsel: f64) -> f64 {
    let mut other = clamp_probability(simple - basesel);
    if other > 1.0 - totalsel {
        other = 1.0 - totalsel;
    }
    clamp_probability(mcv + other)
}

type Mcv = (Vec<DataType>, Vec<crate::types::extstats::McvItem>);

/// clauselist_selectivity for one base relation's restrictions, with its extended
/// statistics (MCV lists and functional dependencies) where they cover the clauses.
pub struct ExtSel<'s, 'c> {
    ctx: &'s ExecCtx<'c>,
    est: &'s costs::Est<'s>,
    ncols: usize,
    stats: Vec<StatInfo>,
}

impl<'s, 'c> ExtSel<'s, 'c> {
    pub fn new(ctx: &'s ExecCtx<'c>, est: &'s costs::Est<'s>, t: &Arc<Table>) -> Self {
        let any = ctx.catalog().statistics.values().any(|s| s.table == t.oid);
        ExtSel {
            ctx,
            est,
            ncols: t.columns.len(),
            stats: if any {
                statlist(ctx, t, false)
                    .into_iter()
                    .filter(|s| s.kinds.contains(&'m') || s.kinds.contains(&'f'))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// clauselist_selectivity_ext.
    pub fn clauses(&self, list: &[Expr], ext: bool) -> f64 {
        if self.stats.is_empty() {
            return self.est.clauses(list);
        }
        if list.len() == 1 {
            return self.clause(&list[0], ext);
        }
        let mut done = vec![false; list.len()];
        let mut s = 1.0;
        if ext && list.iter().any(|e| !e.columns().is_empty()) {
            s = self.statext(list, &mut done, false);
        }
        let mut plain = Vec::new();
        for (e, d) in list.iter().zip(&done) {
            if *d {
                continue;
            }
            match e {
                Expr::And(_) | Expr::Or(_) => s *= self.clause(e, ext),
                e => plain.push(e.clone()),
            }
        }
        if !plain.is_empty() {
            s *= self.est.clauses(&plain);
        }
        s
    }

    /// clause_selectivity_ext.
    pub fn clause(&self, e: &Expr, ext: bool) -> f64 {
        match e {
            Expr::And(list) if !self.stats.is_empty() => self.clauses(list, ext),
            Expr::Or(list) if !self.stats.is_empty() => self.clauses_or(list, ext),
            e => self.est.clause(e),
        }
    }

    /// clauselist_selectivity_or.
    fn clauses_or(&self, list: &[Expr], ext: bool) -> f64 {
        let mut done = vec![false; list.len()];
        let mut s = 0.0;
        if ext && list.iter().any(|e| !e.columns().is_empty()) {
            s = self.statext(list, &mut done, true);
        }
        for (e, d) in list.iter().zip(&done) {
            if *d {
                continue;
            }
            let s2 = self.clause(e, ext);
            s = s + s2 - s * s2;
        }
        s
    }

    /// statext_clauselist_selectivity: MCV lists first, then (for ANDs) dependencies.
    fn statext(&self, list: &[Expr], done: &mut [bool], is_or: bool) -> f64 {
        let s = self.mcv_clauselist(list, done, is_or);
        if is_or {
            return s;
        }
        s * self.dependencies_clauselist(list, done)
    }

    /// statext_is_compatible_clause: the columns and expressions of a clause an MCV list
    /// can evaluate.
    fn compatible(&self, e: &Expr, cols: &mut BTreeSet<usize>, exprs: &mut Vec<Expr>) -> bool {
        if let Expr::And(list) = e {
            return list.iter().all(|c| self.compatible(c, cols, exprs));
        }
        if e.columns().is_empty() {
            return false;
        }
        self.compatible_internal(e, cols, exprs)
    }

    fn compatible_internal(
        &self,
        e: &Expr,
        cols: &mut BTreeSet<usize>,
        exprs: &mut Vec<Expr>,
    ) -> bool {
        let e = strip_relabel(e);
        let operand = |x: &Expr, cols: &mut BTreeSet<usize>, exprs: &mut Vec<Expr>| {
            if matches!(x, Expr::Col(..)) {
                self.compatible_internal(x, cols, exprs)
            } else {
                exprs.push(x.clone());
                true
            }
        };
        match e {
            Expr::Col(c, _) => {
                if *c >= self.ncols {
                    return false;
                }
                cols.insert(*c);
                true
            }
            Expr::Call(d, args, _) if args.len() == 2 && is_operator(d) => {
                let Some((x, ..)) = opclause_args(args) else {
                    return false;
                };
                if !SCALAR_RESTS.contains(&oprrest(d)) {
                    return false;
                }
                operand(x, cols, exprs)
            }
            Expr::ArrayOp { left, right, op, .. } => {
                if array_consts(right).is_none() || !SCALAR_RESTS.contains(&oprrest(op)) {
                    return false;
                }
                operand(strip_relabel(left), cols, exprs)
            }
            Expr::InList(x, items, false) => {
                if items.is_empty()
                    || !items.iter().all(|(i, _)| matches!(strip_relabel(i), Expr::Const(..)))
                    || !SCALAR_RESTS.contains(&oprrest(&items[0].1))
                {
                    return false;
                }
                operand(strip_relabel(x), cols, exprs)
            }
            Expr::InList(..) => false,
            Expr::And(list) | Expr::Or(list) => {
                list.iter().all(|c| self.compatible_internal(c, cols, exprs))
            }
            Expr::Not(x) => self.compatible_internal(x, cols, exprs),
            Expr::IsNull(x, _) => operand(x, cols, exprs),
            other => {
                exprs.push(other.clone());
                true
            }
        }
    }

    fn mcv_of(&self, st: &StatInfo) -> Option<Mcv> {
        let cat = self.ctx.catalog();
        crate::catalog::system::mcv_items(&cat, st.data.mcv.as_deref()?)
    }

    /// statext_mcv_clauselist_selectivity.
    fn mcv_clauselist(&self, list: &[Expr], done: &mut [bool], is_or: bool) -> f64 {
        let mut sel = if is_or { 0.0 } else { 1.0 };
        if !self.stats.iter().any(|s| s.kinds.contains(&'m')) {
            return sel;
        }
        let mut info: Vec<Option<(BTreeSet<usize>, Vec<Expr>)>> = list
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if done[i] {
                    return None;
                }
                let mut cols = BTreeSet::new();
                let mut exprs = Vec::new();
                self.compatible(e, &mut cols, &mut exprs)
                    .then_some((cols, exprs))
            })
            .collect();
        let covers = |st: &StatInfo, cols: &BTreeSet<usize>, exprs: &[Expr]| {
            cols.iter().all(|c| st.keys.contains(c))
                && exprs.iter().all(|e| st.exprs.iter().any(|x| x.same_as(e)))
        };
        loop {
            // choose_best_statistics: the most clause columns and expressions, then the
            // fewest keys.
            let mut best: Option<&StatInfo> = None;
            let mut best_matched = 2;
            let mut best_keys = usize::MAX;
            for st in self.stats.iter().filter(|s| s.kinds.contains(&'m')) {
                let mut cols = BTreeSet::new();
                let mut exprs = BTreeSet::new();
                for (c, x) in info.iter().flatten() {
                    if !covers(st, c, x) {
                        continue;
                    }
                    cols.extend(c.iter().copied());
                    for e in x {
                        if let Some(p) = st.exprs.iter().position(|y| y.same_as(e)) {
                            exprs.insert(p);
                        }
                    }
                }
                let matched = cols.len() + exprs.len();
                let keys = st.keys.len() + st.exprs.len();
                if matched > best_matched || (matched == best_matched && keys < best_keys) {
                    best = Some(st);
                    best_matched = matched;
                    best_keys = keys;
                }
            }
            let Some(st) = best else {
                break;
            };
            let mut stat_clauses = Vec::new();
            let mut simple = Vec::new();
            for (i, slot) in info.iter_mut().enumerate() {
                let Some((c, x)) = slot else {
                    continue;
                };
                if !covers(st, c, x) {
                    continue;
                }
                if (c.is_empty() && x.len() == 1) || (x.is_empty() && c.len() == 1) {
                    simple.push(stat_clauses.len());
                }
                stat_clauses.push(list[i].clone());
                done[i] = true;
                *slot = None;
            }
            let Some(mcv) = self.mcv_of(st) else {
                continue;
            };
            if is_or {
                let mut or_matches = vec![false; mcv.1.len()];
                let mut simple_or = 0.0;
                let mut stat_sel = 0.0;
                for (k, c) in stat_clauses.iter().enumerate() {
                    let simple_sel = self.clause(c, false);
                    let overlap_simple = simple_or * simple_sel;
                    simple_or = clamp_probability(simple_or + simple_sel - overlap_simple);
                    let m = self.match_bitmap(std::slice::from_ref(c), st, &mcv, false);
                    let (mut mcv_sel, mut basesel, mut ov_mcv, mut ov_base, mut total) =
                        (0.0, 0.0, 0.0, 0.0, 0.0);
                    for (i, item) in mcv.1.iter().enumerate() {
                        total += item.frequency;
                        if m[i] {
                            mcv_sel += item.frequency;
                            basesel += item.base_frequency;
                            if or_matches[i] {
                                ov_mcv += item.frequency;
                                ov_base += item.base_frequency;
                            }
                        }
                        or_matches[i] |= m[i];
                    }
                    let clause_sel = if simple.contains(&k) {
                        simple_sel
                    } else {
                        mcv_combine(simple_sel, mcv_sel, basesel, total)
                    };
                    let overlap_sel = mcv_combine(overlap_simple, ov_mcv, ov_base, total);
                    stat_sel = clamp_probability(stat_sel + clause_sel - overlap_sel);
                }
                sel = sel + stat_sel - sel * stat_sel;
            } else {
                let simple_sel = self.clauses(&stat_clauses, false);
                let m = self.match_bitmap(&stat_clauses, st, &mcv, false);
                let (mut mcv_sel, mut basesel, mut total) = (0.0, 0.0, 0.0);
                for (i, item) in mcv.1.iter().enumerate() {
                    total += item.frequency;
                    if m[i] {
                        basesel += item.base_frequency;
                        mcv_sel += item.frequency;
                    }
                }
                sel *= mcv_combine(simple_sel, mcv_sel, basesel, total);
            }
        }
        sel
    }

    /// mcv_match_expression: the MCV list dimension of a column or expression.
    fn dim(st: &StatInfo, e: &Expr) -> Option<usize> {
        match strip_relabel(e) {
            Expr::Col(c, _) => st.keys.iter().position(|k| k == c),
            e => st
                .exprs
                .iter()
                .position(|x| x.same_as(e))
                .map(|p| st.keys.len() + p),
        }
    }

    /// `a op b` on two values.
    fn op_holds(
        &self,
        d: &Arc<crate::sql::func::FuncDef>,
        a: (&Value, &DataType),
        b: (&Value, &DataType),
    ) -> bool {
        let e = Expr::Call(
            d.clone(),
            vec![
                Expr::Const(a.0.clone(), a.1.clone()),
                Expr::Const(b.0.clone(), b.1.clone()),
            ],
            DataType::Bool,
        );
        matches!(
            crate::sql::expr::eval(&e, &[], self.ctx),
            Ok(Value::Bool(true))
        )
    }

    /// mcv_get_match_bitmap: which MCV items the clauses (ANDed, or ORed) hold for.
    fn match_bitmap(&self, clauses: &[Expr], st: &StatInfo, mcv: &Mcv, is_or: bool) -> Vec<bool> {
        let (types, items) = mcv;
        let merge = |v: bool, m: bool| if is_or { v || m } else { v && m };
        let is_final = |v: bool| if is_or { v } else { !v };
        let mut matches = vec![!is_or; items.len()];
        for c in clauses {
            match c {
                Expr::Call(d, args, _) if args.len() == 2 && is_operator(d) => {
                    let Some((x, cst, cty, left)) = opclause_args(args) else {
                        continue;
                    };
                    let Some(idx) = Self::dim(st, x) else {
                        continue;
                    };
                    for (i, item) in items.iter().enumerate() {
                        if item.nulls[idx] || cst.is_null() {
                            matches[i] = merge(matches[i], false);
                            continue;
                        }
                        if is_final(matches[i]) {
                            continue;
                        }
                        let v = (&item.values[idx], &types[idx]);
                        let m = if left {
                            self.op_holds(d, v, (cst, cty))
                        } else {
                            self.op_holds(d, (cst, cty), v)
                        };
                        matches[i] = merge(matches[i], m);
                    }
                }
                Expr::ArrayOp { left, right, op, all } => {
                    let Some(idx) = Self::dim(st, left) else {
                        continue;
                    };
                    let null_array = matches!(strip_relabel(right), Expr::Const(Value::Null, _));
                    let elems = array_consts(right).unwrap_or_default();
                    let ops: Vec<_> = elems.iter().map(|_| op.clone()).collect();
                    self.match_elems(&mut matches, items, types, idx, &elems, &ops, !*all, null_array, is_or);
                }
                Expr::InList(x, list, false) => {
                    let Some(idx) = Self::dim(st, x) else {
                        continue;
                    };
                    let elems: Vec<(Value, DataType)> = list
                        .iter()
                        .filter_map(|(i, _)| match strip_relabel(i) {
                            Expr::Const(v, t) => Some((v.clone(), t.clone())),
                            _ => None,
                        })
                        .collect();
                    let ops: Vec<_> = list.iter().map(|(_, d)| d.clone()).collect();
                    self.match_elems(&mut matches, items, types, idx, &elems, &ops, true, false, is_or);
                }
                Expr::IsNull(x, not) => {
                    let Some(idx) = Self::dim(st, x) else {
                        continue;
                    };
                    for (i, item) in items.iter().enumerate() {
                        let m = item.nulls[idx] != *not;
                        matches[i] = merge(matches[i], m);
                    }
                }
                Expr::Or(list) | Expr::And(list) => {
                    let sub = self.match_bitmap(list, st, mcv, matches!(c, Expr::Or(_)));
                    for (m, s) in matches.iter_mut().zip(sub) {
                        *m = merge(*m, s);
                    }
                }
                Expr::Not(x) => {
                    let sub = self.match_bitmap(std::slice::from_ref(&**x), st, mcv, false);
                    for (m, s) in matches.iter_mut().zip(sub) {
                        *m = merge(*m, !s);
                    }
                }
                other => {
                    let Some(idx) = Self::dim(st, other) else {
                        continue;
                    };
                    for (i, item) in items.iter().enumerate() {
                        let m = !item.nulls[idx] && matches!(item.values[idx], Value::Bool(true));
                        matches[i] = merge(matches[i], m);
                    }
                }
            }
        }
        matches
    }

    /// A ScalarArrayOpExpr's matches: `elems` tried in turn, ORed (`use_or`) or ANDed.
    #[allow(clippy::too_many_arguments)]
    fn match_elems(
        &self,
        matches: &mut [bool],
        items: &[crate::types::extstats::McvItem],
        types: &[DataType],
        idx: usize,
        elems: &[(Value, DataType)],
        ops: &[Arc<crate::sql::func::FuncDef>],
        use_or: bool,
        null_array: bool,
        is_or: bool,
    ) {
        let merge = |v: bool, m: bool, or: bool| if or { v || m } else { v && m };
        let is_final = |v: bool, or: bool| if or { v } else { !v };
        for (i, item) in items.iter().enumerate() {
            if item.nulls[idx] || null_array {
                matches[i] = merge(matches[i], false, is_or);
                continue;
            }
            if is_final(matches[i], is_or) {
                continue;
            }
            let mut m = !use_or;
            for ((v, t), op) in elems.iter().zip(ops) {
                if v.is_null() {
                    m = merge(m, false, use_or);
                    continue;
                }
                if is_final(m, use_or) {
                    break;
                }
                let e = self.op_holds(op, (&item.values[idx], &types[idx]), (v, t));
                m = merge(m, e, use_or);
            }
            matches[i] = merge(matches[i], m, is_or);
        }
    }

    /// The column or expression an equality-like clause tests (dependency_is_compatible_*,
    /// before looking at what it is).
    fn dep_operand<'e>(&self, e: &'e Expr) -> Option<&'e Expr> {
        let x = match e {
            Expr::Call(d, args, _) if args.len() == 2 && is_operator(d) => {
                let x = if pseudo_const(&args[1]) {
                    &args[0]
                } else if pseudo_const(&args[0]) {
                    &args[1]
                } else {
                    return None;
                };
                if oprrest(d) != F_EQSEL {
                    return None;
                }
                x
            }
            Expr::ArrayOp { left, right, op, all } => {
                if *all || !pseudo_const(right) || oprrest(op) != F_EQSEL {
                    return None;
                }
                left
            }
            Expr::InList(x, items, false) => {
                if items.is_empty()
                    || !items.iter().all(|(i, _)| pseudo_const(i))
                    || oprrest(&items[0].1) != F_EQSEL
                {
                    return None;
                }
                x
            }
            Expr::Not(x) => x,
            e => e,
        };
        Some(strip_relabel(x))
    }

    /// dependency_is_compatible_clause: the column (index) the clause is on.
    fn dep_column(&self, e: &Expr) -> Option<usize> {
        if e.columns().is_empty() {
            return None;
        }
        if let Expr::Or(list) = e {
            let mut col = None;
            for c in list {
                let k = self.dep_column(c)?;
                if col.is_some_and(|x| x != k) {
                    return None;
                }
                col = Some(k);
            }
            return col;
        }
        if matches!(e, Expr::And(_)) {
            return None;
        }
        match self.dep_operand(e)? {
            Expr::Col(c, _) if *c < self.ncols => Some(*c),
            _ => None,
        }
    }

    /// dependency_is_compatible_expression: the statistics expression the clause is on.
    fn dep_expression(&self, e: &Expr) -> Option<Expr> {
        if e.columns().is_empty() {
            return None;
        }
        if let Expr::Or(list) = e {
            let mut out: Option<Expr> = None;
            for c in list {
                let k = self.dep_expression(c)?;
                match &out {
                    Some(p) if !p.same_as(&k) => return None,
                    Some(_) => {}
                    None => out = Some(k),
                }
            }
            return out;
        }
        if matches!(e, Expr::And(_)) {
            return None;
        }
        let x = self.dep_operand(e)?;
        self.stats
            .iter()
            .filter(|s| s.kinds.contains(&'f'))
            .flat_map(|s| s.exprs.iter())
            .find(|se| x.same_as(se))
            .cloned()
    }

    /// dependencies_clauselist_selectivity.
    fn dependencies_clauselist(&self, list: &[Expr], done: &mut [bool]) -> f64 {
        if !self.stats.iter().any(|s| s.kinds.contains(&'f')) {
            return 1.0;
        }
        let mut unique: Vec<Expr> = Vec::new();
        let mut attnums: Vec<Option<i32>> = vec![None; list.len()];
        for (i, e) in list.iter().enumerate() {
            if done[i] {
                continue;
            }
            if let Some(c) = self.dep_column(e) {
                attnums[i] = Some(c as i32 + 1);
            } else if let Some(x) = self.dep_expression(e) {
                let k = match unique.iter().position(|u| u.same_as(&x)) {
                    Some(k) => k,
                    None => {
                        unique.push(x);
                        unique.len() - 1
                    }
                };
                attnums[i] = Some(-(k as i32 + 1));
            }
        }
        let offset = if unique.is_empty() {
            0
        } else {
            unique.len() as i32 + 1
        };
        let mut clause_attnums = BTreeSet::new();
        for a in attnums.iter_mut().flatten() {
            *a += offset;
            clause_attnums.insert(*a);
        }
        if clause_attnums.len() < 2 {
            return 1.0;
        }
        let mut func_deps: Vec<Vec<(Vec<i32>, f64)>> = Vec::new();
        for st in self.stats.iter().filter(|s| s.kinds.contains(&'f')) {
            let nmatched = st
                .keys
                .iter()
                .filter(|k| clause_attnums.contains(&(**k as i32 + 1 + offset)))
                .count();
            let nexprs: usize = unique
                .iter()
                .map(|u| st.exprs.iter().filter(|x| x.same_as(u)).count())
                .sum();
            if nmatched + nexprs < 2 {
                continue;
            }
            let Some(items) = st
                .data
                .dependencies
                .as_deref()
                .and_then(crate::types::extstats::dependency_items)
            else {
                continue;
            };
            let mut deps = Vec::new();
            for (attrs, degree) in items {
                if unique.is_empty() && st.exprs.is_empty() {
                    deps.push((attrs.iter().map(|a| *a as i32).collect(), degree));
                    continue;
                }
                let mut out = Vec::with_capacity(attrs.len());
                let mut skip = false;
                for a in attrs {
                    let a = a as i32;
                    if a > 0 {
                        if !clause_attnums.contains(&(a + offset)) {
                            skip = true;
                            break;
                        }
                        out.push(a + offset);
                    } else {
                        let idx = (-(1 + a)) as usize;
                        match st
                            .exprs
                            .get(idx)
                            .and_then(|x| unique.iter().position(|u| u.same_as(x)))
                        {
                            Some(m) => out.push(-(m as i32 + 1) + offset),
                            None => {
                                skip = true;
                                break;
                            }
                        }
                    }
                }
                if !skip {
                    deps.push((out, degree));
                }
            }
            if !deps.is_empty() {
                func_deps.push(deps);
            }
        }
        if func_deps.is_empty() {
            return 1.0;
        }
        // find_strongest_dependency, repeatedly: the most attributes, then the highest
        // degree, fully matched by the clauses' remaining attributes.
        let mut chosen: Vec<(Vec<i32>, f64)> = Vec::new();
        loop {
            let n = clause_attnums.len();
            let mut strongest: Option<&(Vec<i32>, f64)> = None;
            for d in func_deps.iter().flatten() {
                if d.0.len() > n {
                    continue;
                }
                if let Some(s) = strongest {
                    if d.0.len() < s.0.len() {
                        continue;
                    }
                    if s.0.len() == d.0.len() && s.1 > d.1 {
                        continue;
                    }
                }
                if d.0.iter().all(|a| clause_attnums.contains(a)) {
                    strongest = Some(d);
                }
            }
            let Some(d) = strongest else {
                break;
            };
            if let Some(last) = d.0.last() {
                clause_attnums.remove(last);
            }
            chosen.push(d.clone());
        }
        if chosen.is_empty() {
            return 1.0;
        }
        // clauselist_apply_dependencies.
        let all: BTreeSet<i32> = chosen.iter().flat_map(|d| d.0.iter().copied()).collect();
        let order: Vec<i32> = all.into_iter().collect();
        let mut attr_sel: Vec<f64> = Vec::with_capacity(order.len());
        for a in &order {
            let mut cl = Vec::new();
            for (i, e) in list.iter().enumerate() {
                if attnums[i] == Some(*a) {
                    cl.push(e.clone());
                    done[i] = true;
                }
            }
            attr_sel.push(self.clauses(&cl, false));
        }
        let pos = |a: &i32| order.iter().position(|x| x == a).unwrap_or(0);
        for d in chosen.iter().rev() {
            let (implied, given) = d.0.split_last().expect("a dependency");
            let s1: f64 = given.iter().map(|a| attr_sel[pos(a)]).product();
            let k = pos(implied);
            let s2 = attr_sel[k];
            let f = d.1;
            attr_sel[k] = if s1 <= s2 {
                f + (1.0 - f) * s2
            } else {
                f * s2 / s1 + (1.0 - f) * s2
            };
        }
        clamp_probability(attr_sel.iter().product())
    }
}
