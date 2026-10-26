//! SELECT: FROM and joins, WHERE, GROUP BY / grouping sets, HAVING, windows, the select
//! list, DISTINCT, ORDER BY, LIMIT, set operations, VALUES, CTEs and row locking.

use super::ast::{self, inner, names, with_pos};
use super::optimize::{self, width};
use super::{Binder, Clause, CteDef, Scope, ScopeCol, ScopeRel, derived_rel, table_rel};
use crate::catalog::Relation;
use crate::catalog::system::SysTable;
use crate::error::{Error, Result, code};
use crate::sql::expr::Expr;
use crate::sql::func::FuncDef;
use crate::sql::plan::{
    JoinKind, OutCol, Plan, RowMark, ScanPlan, SetOpKind, SortKey, SrfCall, Wait,
};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::sync::Arc;

/// A select-list entry after `*` expansion.
pub(crate) struct Target {
    pub name: String,
    pub expr: Expr,
    pub table: u32,
    pub attnum: i16,
    /// The domain (or array-of-domain) type the target is declared as (`declared_domain`,
    /// worked out while its columns still resolve).
    pub domain: Option<u32>,
}

/// Row locking requested by a query level's FOR UPDATE / FOR SHARE clauses, in order (and a
/// lock pushed down from the level above, last).
#[derive(Clone, Debug)]
pub(crate) struct Locking {
    pub clauses: Vec<LockClause>,
}

/// One FOR UPDATE / SHARE clause.
#[derive(Clone, Debug)]
pub(crate) struct LockClause {
    /// The relations it names (none: every relation of the level).
    pub rels: Vec<String>,
    /// Where each of `rels` is named (for errors).
    pub locs: Vec<i32>,
    pub mark: RowMark,
    pub wait: Wait,
}

impl Locking {
    /// The lock the relation `name` takes (applyLockingClause over every clause naming it,
    /// or naming none: the strongest strength, the strictest wait policy), and its place in
    /// the level's rowMarks list (the clause first naming it, then its place in that
    /// clause's list; a clause naming none leaves the range table's order).
    pub(crate) fn mark_for(&self, name: &str) -> Option<(RowMark, Wait, u32)> {
        let strength = |m: RowMark| match m {
            RowMark::KeyShare => 0,
            RowMark::Share => 1,
            RowMark::NoKeyUpdate => 2,
            RowMark::Update => 3,
        };
        // (LockWaitPolicy's order: block, skip locked, error.)
        let strict = |w: Wait| match w {
            Wait::Block => 0,
            Wait::SkipLocked => 1,
            Wait::NoWait => 2,
        };
        let mut out: Option<(RowMark, Wait, u32)> = None;
        for (i, c) in self.clauses.iter().enumerate() {
            let pos = if c.rels.is_empty() {
                Some(0)
            } else {
                c.rels.iter().position(|r| r == name)
            };
            let Some(pos) = pos else { continue };
            out = Some(match out {
                None => (c.mark, c.wait, ((i as u32) << 16) | (pos as u32).min(0xffff)),
                Some((m, w, o)) => (
                    if strength(c.mark) > strength(m) { c.mark } else { m },
                    if strict(c.wait) > strict(w) { c.wait } else { w },
                    o,
                ),
            });
        }
        out
    }

    /// The strength of the first clause (LCS_asString in CheckSelectLocking's errors).
    pub(crate) fn first_mark(&self) -> RowMark {
        self.clauses.first().map_or(RowMark::Update, |c| c.mark)
    }
}

impl Binder<'_, '_> {
    pub(crate) fn bind_select_node(&mut self, n: &Node) -> Result<(Plan, Vec<OutCol>)> {
        match inner(n) {
            Some(NodeEnum::SelectStmt(s)) => self.bind_select_stmt(s),
            Some(NodeEnum::InsertStmt(_) | NodeEnum::UpdateStmt(_) | NodeEnum::DeleteStmt(_))
                if let Some(r) = self.cte_rewritten(n)? =>
            {
                Ok(r)
            }
            Some(NodeEnum::InsertStmt(s)) => self.bind_insert(s),
            Some(NodeEnum::UpdateStmt(s)) => self.bind_update(s),
            Some(NodeEnum::DeleteStmt(s)) => self.bind_delete(s),
            Some(NodeEnum::MergeStmt(s)) => self.bind_merge(s),
            _ => Err(crate::error::unsupported("this kind of subquery")),
        }
    }

    pub(crate) fn bind_select_stmt(&mut self, s: &pb::SelectStmt) -> Result<(Plan, Vec<OutCol>)> {
        if let Some(into) = &s.into_clause {
            let at = into.rel.as_ref().map_or(-1, |r| r.location);
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "SELECT ... INTO is not allowed here"),
                at,
            ));
        }
        let keep = std::mem::take(&mut self.keep_unknown);
        if s.with_clause.is_some() {
            self.count_cte_refs(NodeEnum::SelectStmt(Box::new(s.clone())));
        }
        let r = match &s.with_clause {
            Some(w) => self.with_ctes(w, |b| {
                b.keep_unknown = keep;
                b.bind_select_body(s)
            }),
            None => {
                self.keep_unknown = keep;
                self.bind_select_body(s)
            }
        }?;
        // free_parsestate: MaxTupleAttributeNumber, once the level is analyzed.
        if r.1.len() > 1664 {
            return Err(Error::new(
                code::TOO_MANY_COLUMNS,
                "target lists can have at most 1664 entries",
            ));
        }
        Ok(r)
    }

    fn bind_select_body(&mut self, s: &pb::SelectStmt) -> Result<(Plan, Vec<OutCol>)> {
        let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
        if !matches!(
            op,
            pb::SetOperation::SetopNone | pb::SetOperation::Undefined
        ) {
            self.keep_unknown = false;
            // (A view's set operation locked from the query above locks nothing:
            // markQueryForLocking finds no relation in its empty jointree.)
            if self.pushed_lock.as_ref().is_some_and(|(l, _)| *l == self.levels.len()) {
                self.pushed_lock = None;
            }
            // (UNION ALL under a constant LIMIT and no ORDER BY: the arms planned for it;
            // nested UNION ALLs keep the outer one's.)
            let union_all = op == pb::SetOperation::SetopUnion && s.all;
            let own = s
                .limit_count
                .as_deref()
                .and_then(ast::ival)
                .filter(|_| union_all && s.sort_clause.is_empty())
                .map(|l| {
                    let off = s.limit_offset.as_deref().and_then(ast::ival).unwrap_or(0);
                    (self.levels.len(), l as f64, off as f64)
                });
            let saved = self.arm_limit;
            self.arm_limit = match own {
                Some(o) => Some(o),
                None if union_all && s.limit_count.is_none() && s.sort_clause.is_empty() => saved,
                None => None,
            };
            let top = self.setop_leaves.is_none();
            let saved_coerced = std::mem::replace(&mut self.setop_coerced, false);
            if top {
                self.setop_leaves = Some(0);
            }
            let r = self.bind_set_op(s, op);
            if top {
                self.setop_leaves = None;
            }
            let coerced = self.setop_coerced;
            self.setop_coerced = saved_coerced || (!top && coerced);
            self.arm_limit = saved;
            let (mut plan, cols) = r?;
            // flatten_simple_union_all: a query level that is a UNION ALL of leaves of its
            // own types is an appendrel, its parent the leftmost leaf's entry (`*SELECT* 1`).
            if top
                && union_all
                && !coerced
                && let Plan::Append { inh: None, alias, .. } = &mut plan
            {
                *alias = Some("*SELECT* 1".to_string());
            }
            return self.finish_over_output(plan, cols, s);
        }
        if !s.values_lists.is_empty() {
            self.keep_unknown = false;
            let (plan, cols) = self.bind_values(&s.values_lists)?;
            return self.finish_over_output(plan, cols, s);
        }
        self.bind_simple_select(s)
    }

    // ── WITH ──

    /// Before binding statement `stmt`'s WITH: how often it names each relation without a
    /// schema (its items' cterefcount).
    pub(crate) fn count_cte_refs(&mut self, stmt: NodeEnum) {
        let mut out = std::collections::HashMap::new();
        for (n, ..) in stmt.nodes() {
            if let pg_query::NodeRef::RangeVar(rv) = n
                && rv.schemaname.is_empty()
            {
                *out.entry(rv.relname.clone()).or_insert(0usize) += 1;
            }
        }
        self.cte_refs = Some(out);
    }

    pub(crate) fn with_ctes(
        &mut self,
        w: &pb::WithClause,
        body: impl FnOnce(&mut Self) -> Result<(Plan, Vec<OutCol>)>,
    ) -> Result<(Plan, Vec<OutCol>)> {
        self.ctes.push(Vec::new());
        self.cte_levels.push(self.levels.len() - 1);
        let r = self.with_ctes_inner(w, body);
        self.cte_levels.pop();
        self.ctes.pop();
        r
    }

    fn with_ctes_inner(
        &mut self,
        w: &pb::WithClause,
        body: impl FnOnce(&mut Self) -> Result<(Plan, Vec<OutCol>)>,
    ) -> Result<(Plan, Vec<OutCol>)> {
        let (mut plans, mut cte_names) = self.define_ctes(w)?;
        // (The body is planned now: its CTE Scans' sizes are the queries' estimates.)
        let pushed = crate::sql::explain::CteEstimatesPushed(if self.deparse {
            0
        } else {
            crate::sql::explain::push_cte_estimates(&plans, &cte_names, self.ctx)
        });
        crate::scoped::checkpoint("explain::CTE_ESTIMATES");
        let r = body(self);
        drop(pushed);
        let (body_plan, cols) = r?;
        // (Those inlined are no CTEs of the plan; nor, SS_process_ctes, a SELECT nothing
        // references, which is never planned: nothing it would fold fails.)
        let inlined: Vec<usize> = self
            .ctes
            .last()
            .map(|l| {
                l.iter()
                    .filter(|c| {
                        c.inline.is_some()
                            || (!self.deparse
                                && !c.used.get()
                                && plans.iter().any(|(id, p)| *id == c.id && !p.writes()))
                    })
                    .map(|c| c.id)
                    .collect()
            })
            .unwrap_or_default();
        if !inlined.is_empty() {
            let keep: Vec<bool> = plans.iter().map(|(id, _)| !inlined.contains(id)).collect();
            let mut k = keep.iter();
            plans.retain(|_| *k.next().unwrap());
            let mut k = keep.iter();
            cte_names.retain(|_| *k.next().unwrap());
        }
        if plans.is_empty() {
            return Ok((body_plan, cols));
        }
        Ok((
            Plan::With {
                ctes: plans,
                body: Arc::new(body_plan),
                names: cte_names,
            },
            cols,
        ))
    }

    /// Bind a WITH clause's queries and make them visible by name (in the innermost CTE
    /// frame): their plans and names.
    pub(crate) fn define_ctes(
        &mut self,
        w: &pb::WithClause,
    ) -> Result<(Vec<(usize, Arc<Plan>)>, Vec<String>)> {
        let all: Vec<&pb::CommonTableExpr> = w
            .ctes
            .iter()
            .filter_map(|c| match inner(c) {
                Some(NodeEnum::CommonTableExpr(cte)) => Some(&**cte),
                _ => None,
            })
            .collect();
        if let Some(log) = self.ref_log() {
            log.ctes.extend(all.iter().map(|c| c.ctename.clone()));
        }
        for (i, cte) in all.iter().enumerate() {
            if all[..i].iter().any(|d| d.ctename == cte.ctename) {
                return Err(with_pos(
                    Error::new(
                        code::DUPLICATE_ALIAS,
                        format!(
                            "WITH query name \"{}\" specified more than once",
                            cte.ctename
                        ),
                    ),
                    cte.location,
                ));
            }
        }
        let is_dml = |c: &pb::CommonTableExpr| {
            matches!(
                c.ctequery.as_deref().and_then(inner),
                Some(
                    NodeEnum::InsertStmt(_)
                        | NodeEnum::UpdateStmt(_)
                        | NodeEnum::DeleteStmt(_)
                        | NodeEnum::MergeStmt(_)
                )
            )
        };
        // (transformWithClause: data-modifying statements only in the top query's WITH. A
        // stored query printed back, such as a rule action under its OLD / NEW level, was
        // checked when it was made.)
        if !self.deparse
            && self.levels.len() > 1
            && !self.levels.last().is_some_and(|l| l.barrier)
            && let Some(c) = all.iter().find(|c| is_dml(c))
        {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "WITH clause containing a data-modifying statement must be at the top level",
                ),
                c.location,
            ));
        }
        // checkWellFormedRecursion: a recursive item must be a SELECT.
        if w.recursive
            && let Some(c) = all
                .iter()
                .find(|c| is_dml(c) && c.ctequery.as_deref().is_some_and(|q| references_name(q, &c.ctename)))
        {
            return Err(with_pos(
                Error::new(
                    code::INVALID_RECURSION,
                    format!("recursive query \"{}\" must not contain data-modifying statements", c.ctename),
                ),
                c.location,
            ));
        }
        // A recursive WITH's items are analyzed after those they read.
        let order: Vec<usize> = if w.recursive {
            super::recursion::analysis_order(&all)?
        } else {
            (0..all.len()).collect()
        };
        let mut slots = Vec::new();
        // (Later items are planned seeing the earlier ones' estimates.)
        // (Popped when this returns, on an error too: a later statement must not see them.)
        let mut pushed = crate::sql::explain::CteEstimatesPushed(0);
        let future_mark = self.future_ctes.len();
        // (The statement's references to its items: taken before their queries' own WITHs.)
        let refs = self.cte_refs.take();
        for &k in &order {
            let cte = all[k];
            if !w.recursive {
                self.future_ctes.truncate(future_mark);
                self.future_ctes
                    .extend(all[k..].iter().map(|c| c.ctename.clone()));
            }
            let q = cte.ctequery.as_deref().unwrap();
            let id = self.fresh_id();
            let aliases = names(&cte.aliascolnames);
            let recursive_sel = match inner(q) {
                Some(NodeEnum::SelectStmt(s))
                    if w.recursive && references_name(q, &cte.ctename) =>
                {
                    Some(s)
                }
                _ => None,
            };
            let mut deferred = None;
            let (plan, mut cols) = match recursive_sel {
                // (A WITH on the recursive UNION itself: its queries are seen by both sides.)
                Some(s) => match &s.with_clause {
                    Some(inner_w) => {
                        if s.op == pb::SetOperation::SetopUnion as i32 {
                            super::recursion::check_well_formed(s, cte)?;
                        }
                        self.with_ctes(inner_w, |b| b.bind_recursive_cte(s, cte, id, &aliases))?
                    }
                    None => self.bind_recursive_cte(s, cte, id, &aliases)?,
                },
                None if cte.search_clause.is_some() || cte.cycle_clause.is_some() => {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, "WITH query is not recursive"),
                        cte.location,
                    ));
                }
                None if matches!(inner(q), Some(NodeEnum::SelectStmt(_))) && !self.deparse => {
                    // (Its plan-time errors wait for a reference: `defer_plan`.)
                    self.defer_plan.borrow_mut().push(None);
                    let r = self.in_sibling(|b| b.bind_select_node(q));
                    deferred = self.defer_plan.borrow_mut().pop().flatten();
                    r?
                }
                None => self.in_sibling(|b| b.bind_select_node(q))?,
            };
            if !aliases.is_empty() {
                if aliases.len() > cols.len() {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_COLUMN_REFERENCE,
                            format!(
                                "WITH query \"{}\" has {} columns available but {} columns specified",
                                cte.ctename,
                                cols.len(),
                                aliases.len()
                            ),
                        ),
                        cte.location,
                    ));
                }
                for (c, a) in cols.iter_mut().zip(&aliases) {
                    c.name = a.clone();
                }
            }
            // DML without RETURNING still runs; it just has nothing to read.
            let no_returning = matches!(
                inner(q),
                Some(
                    NodeEnum::InsertStmt(_)
                        | NodeEnum::UpdateStmt(_)
                        | NodeEnum::DeleteStmt(_)
                        | NodeEnum::MergeStmt(_)
                )
            ) && cols.is_empty();
            for c in &mut cols {
                if matches!(c.ty, DataType::Unknown) {
                    c.ty = DataType::Text;
                }
            }
            // inline_cte (subquery_planner): a SELECT that isn't recursive, locks no rows,
            // runs nothing volatile, and is NOT MATERIALIZED or (by default) read once,
            // goes in place of its reference as a subquery. (Not one reading an enclosing
            // query's values: its references may be at other levels.)
            let select = matches!(inner(q), Some(NodeEnum::SelectStmt(s)) if s.locking_clause.is_empty());
            let wanted = match pb::CteMaterialize::try_from(cte.ctematerialized) {
                Ok(pb::CteMaterialize::Never) => true,
                Ok(pb::CteMaterialize::Always) => false,
                _ => refs.as_ref().and_then(|r| r.get(&cte.ctename)) == Some(&1),
            };
            // (contain_dml: rows locked at any of its levels count as well.)
            fn locks(p: &Plan) -> bool {
                matches!(p, Plan::LockRows { .. })
                    || matches!(p, Plan::Scan(s) if s.mark.is_some())
                    || crate::sql::explain::plan_inputs(p).iter().any(|c| locks(c))
            }
            // (Nor one read more than once reading an enclosing recursive query's working
            // table: contain_outer_selfref.)
            fn self_ref(p: &Plan) -> bool {
                matches!(p, Plan::WorkScan { .. }) || crate::sql::explain::plan_inputs(p).iter().any(|c| self_ref(c))
            }
            let once = refs.as_ref().and_then(|r| r.get(&cte.ctename)).is_none_or(|&n| n <= 1);
            let inline = (!self.deparse
                && recursive_sel.is_none()
                && select
                && !locks(&plan)
                && (once || !self_ref(&plan))
                && wanted
                && !crate::sql::paths::plan_volatile_deep(&plan)
                && !crate::exec::reads_enclosing(&plan, 0))
            .then(|| Arc::new(plan.clone()));
            self.ctes.last_mut().unwrap().push(CteDef {
                name: cte.ctename.clone(),
                id,
                cols,
                working: false,
                aux: 0,
                used: std::cell::Cell::new(false),
                no_returning,
                inline,
                work_filter: None,
                deferred,
            });
            if !self.deparse && self.ctes.last().and_then(|l| l.last()).is_some_and(|c| c.inline.is_none()) {
                pushed.0 += crate::sql::explain::push_cte_estimates(
                    &[(id, Arc::new(plan.clone()))],
                    std::slice::from_ref(&cte.ctename),
                    self.ctx,
                );
            }
            slots.push((cte.ctename.clone(), id, plan));
        }
        drop(pushed);
        self.future_ctes.truncate(future_mark);
        // (Kept in the order they were analyzed in, so each one's inputs open first.)
        let mut plans = Vec::new();
        let mut cte_names = Vec::new();
        for (name, id, plan) in slots {
            cte_names.push(name);
            plans.push((id, Arc::new(plan)));
        }
        Ok((plans, cte_names))
    }

    fn bind_recursive_cte(
        &mut self,
        s: &pb::SelectStmt,
        cte: &pb::CommonTableExpr,
        id: usize,
        aliases: &[String],
    ) -> Result<(Plan, Vec<OutCol>)> {
        let (name, loc) = (cte.ctename.as_str(), cte.location);
        let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
        if op != pb::SetOperation::SetopUnion {
            return Err(with_pos(
                Error::new(
                    code::INVALID_RECURSION,
                    format!(
                        "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
                    ),
                ),
                loc,
            ));
        }
        super::recursion::check_well_formed(s, cte)?;
        // checkWellFormedRecursion: ORDER BY, OFFSET, LIMIT, FOR UPDATE on the whole query.
        let not_impl = |what: &str, at: i32| {
            with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{what} in a recursive query is not implemented"),
                ),
                at,
            )
        };
        if let Some(first) = s.sort_clause.first() {
            let at = match inner(first) {
                Some(NodeEnum::SortBy(sb)) => sb.node.as_deref().map_or(-1, ast::location),
                _ => -1,
            };
            return Err(not_impl("ORDER BY", at));
        }
        if let Some(o) = s.limit_offset.as_deref() {
            return Err(not_impl("OFFSET", ast::location(o)));
        }
        if let Some(l) = s.limit_count.as_deref() {
            return Err(not_impl("LIMIT", ast::location(l)));
        }
        if !s.locking_clause.is_empty() {
            return Err(not_impl("FOR UPDATE/SHARE", -1));
        }
        let larg = s.larg.as_deref().unwrap();
        let rarg = s.rarg.as_deref().unwrap();
        let (base, cols) = self.in_sibling(|b| b.bind_select_stmt(larg))?;
        let mut work_cols = cols.clone();
        for (c, a) in work_cols.iter_mut().zip(aliases) {
            c.name = a.clone();
        }
        let ncols = cols.len();
        let aux = self.search_cycle_columns(cte, &work_cols)?;
        work_cols.extend(aux.cols.iter().cloned());
        // The recursive term also passes the working row's SEARCH / CYCLE columns through.
        let rarg_aux;
        let rarg = if aux.cols.is_empty() {
            rarg
        } else {
            // (analyzeCTE: the UNION's sides must be plain SELECTs.)
            let plain = |x: &pb::SelectStmt| {
                matches!(
                    pb::SetOperation::try_from(x.op),
                    Ok(pb::SetOperation::SetopNone | pb::SetOperation::Undefined)
                )
            };
            for (side, x) in [("left", larg), ("right", rarg)] {
                if !plain(x) {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("with a SEARCH or CYCLE clause, the {side} side of the UNION must be a SELECT"),
                    ));
                }
            }
            // (rewriteSearchAndCycle: the recursive reference is the right side's FROM item.)
            if rarg.with_clause.as_ref().is_some_and(|w| {
                w.ctes.iter().any(|c| {
                    matches!(inner(c), Some(NodeEnum::CommonTableExpr(c)) if c.ctename == name)
                })
            }) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("with a SEARCH or CYCLE clause, the recursive reference to WITH query \"{name}\" must be at the top level of its right-hand SELECT"),
                ));
            }
            let mut r = rarg.clone();
            let from = r.from_clause.iter().find_map(|f| self_ref_alias(f, name));
            let Some(alias) = from.filter(|_| {
                matches!(
                    pb::SetOperation::try_from(r.op),
                    Ok(pb::SetOperation::SetopNone | pb::SetOperation::Undefined)
                )
            }) else {
                return Err(with_pos(
                    crate::error::unsupported("SEARCH / CYCLE with this recursive term"),
                    loc,
                ));
            };
            for c in &aux.cols {
                let str_node = |s: &str| {
                    ast::mk(NodeEnum::String(pb::String {
                        sval: s.to_string(),
                    }))
                };
                let col = ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
                    fields: vec![str_node(&alias), str_node(&c.name)],
                    location: -1,
                }));
                r.target_list
                    .push(ast::mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                        name: String::new(),
                        indirection: vec![],
                        val: Some(Box::new(col)),
                        location: -1,
                    }))));
            }
            rarg_aux = r;
            &rarg_aux
        };
        let aux_cols = aux.cols.len();
        // (CYCLE's check of the work table's mark: the recursive term's WHERE clause.)
        let work_passed: Vec<Expr> = aux
            .cols
            .iter()
            .enumerate()
            .map(|(k, c)| Expr::Col(ncols + k, c.ty.clone()))
            .collect();
        let work_filter = if self.deparse { None } else { aux.not_cycle(&work_passed) };
        self.ctes.last_mut().unwrap().push(CteDef {
            name: name.to_string(),
            id,
            cols: work_cols,
            working: true,
            aux: aux_cols,
            used: std::cell::Cell::new(false),
            no_returning: false,
            inline: None,
            work_filter,
            deferred: None,
        });
        // (The recursive term is planned seeing its worktable as ten times the non-recursive
        // term.)
        let work = !self.deparse;
        if work {
            crate::sql::explain::push_work_estimate(id, &Arc::new(base.clone()), self.ctx);
        }
        let r = self.in_sibling(|b| b.bind_select_stmt(rarg));
        if work {
            crate::sql::explain::pop_cte_estimates(1);
        }
        let def = self.ctes.last_mut().unwrap().pop().unwrap();
        let (rec, rcols) = r?;
        if !def.used.get() {
            return Err(with_pos(
                Error::new(
                    code::INVALID_RECURSION,
                    format!(
                        "recursive reference to query \"{name}\" must not appear within its non-recursive term"
                    ),
                ),
                loc,
            ));
        }
        if rcols.len() != cols.len() + aux_cols {
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    "each UNION query must have the same number of columns",
                ),
                loc,
            ));
        }
        // The recursive term's columns must have the non-recursive term's types.
        let llocs = {
            let mut first = larg;
            while let Some(l) = first.larg.as_deref() {
                first = l;
            }
            target_locations(&first.target_list, cols.len())
        };
        let mut exprs = Vec::new();
        for (i, (rc, c)) in rcols.iter().zip(&cols).enumerate() {
            let e = Expr::Col(i, rc.ty.clone());
            // The UNION's type (select_common_type, the non-recursive term's first) must be
            // the non-recursive term's.
            let overall = crate::types::cast::common_type(&[c.ty.clone(), rc.ty.clone()], "UNION")
                .ok();
            // And its typmod (the sides' when they agree, else none).
            let typmod_lost = rc.ty.same_base(&c.ty)
                && rc.ty != c.ty
                && c.ty != c.ty.without_typmod()
                && !matches!(rc.ty, DataType::Unknown);
            if typmod_lost {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "recursive query \"{name}\" column {} has type {} in non-recursive term but type {} overall",
                            i + 1,
                            c.ty.display(),
                            c.ty.without_typmod().display()
                        ),
                    )
                    .hint("Cast the output of the non-recursive term to the correct type."),
                    llocs.get(i).copied().unwrap_or(loc),
                ));
            }
            if rc.ty.same_base(&c.ty)
                || matches!(rc.ty, DataType::Unknown)
                || overall.as_ref().is_some_and(|o| o.same_base(&c.ty))
            {
                exprs.push(self.coerce_implicit(e, &c.ty)?);
            } else {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "recursive query \"{name}\" column {} has type {} in non-recursive term but type {} overall",
                            i + 1,
                            c.ty.display(),
                            overall.as_ref().unwrap_or(&rc.ty).display()
                        ),
                    )
                    .hint("Cast the output of the non-recursive term to the correct type."),
                    llocs.get(i).copied().unwrap_or(loc),
                ));
            }
        }
        // And the collations the UNION derives must be the non-recursive term's.
        for (i, (rc, c)) in rcols.iter().zip(&cols).enumerate() {
            if !c.ty.collatable() {
                continue;
            }
            let lc = self.out_col_collation(c, &c.ty);
            let overall = match (&lc, self.out_col_collation(rc, &c.ty)) {
                (super::Coll::Implicit(a), super::Coll::Implicit(b))
                    if *a != b && a != "default" && b != "default" =>
                {
                    if !s.all {
                        return Err(with_pos(
                            Error::new(
                                code::COLLATION_MISMATCH,
                                format!("collation mismatch between implicit collations \"{a}\" and \"{b}\""),
                            )
                            .hint("You can choose the collation by applying the COLLATE clause to one or both expressions."),
                            -1,
                        ));
                    }
                    super::Coll::None
                }
                (_, r) => lc.clone().merge(r)?,
            };
            let coll_name = |c: super::Coll| match c.implicit() {
                super::Coll::Implicit(n) => n,
                _ => "(null)".to_string(),
            };
            let (l, o) = (coll_name(lc), coll_name(overall));
            if l != o {
                return Err(with_pos(
                    Error::new(
                        code::COLLATION_MISMATCH,
                        format!(
                            "recursive query \"{name}\" column {} has collation \"{l}\" in non-recursive term but collation \"{o}\" overall",
                            i + 1
                        ),
                    )
                    .hint("Use the COLLATE clause to set the collation of the non-recursive term."),
                    llocs[i],
                ));
            }
        }
        // A recursive UNION (not ALL) is done by hashing (generate_recursion_path).
        // (makeSortGroupClauseForSetOp with require_hash: an anonymous record, or an array
        // of them, is taken to hash; it fails at execution if a field doesn't.)
        let anon = |t: &DataType| match t {
            DataType::Record => true,
            DataType::Row(rt) => rt.oid == 0,
            _ => false,
        };
        let hashes = |t: &DataType| {
            anon(t)
                || matches!(t, DataType::Array(e) if anon(e))
                || self.cat.type_hashable(t)
        };
        if !s.all && cols.iter().any(|c| !hashes(&c.ty)) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "could not implement recursive UNION",
            )
            .detail("All column datatypes must be hashable."));
        }
        if aux_cols == 0 {
            let rec = Plan::Project {
                input: Arc::new(rec),
                exprs,
            };
            return Ok((
                Plan::Recursive {
                    id,
                    base: Arc::new(base),
                    recursive: Arc::new(rec),
                    all: s.all,
                    ncols,
                },
                cols,
            ));
        }
        // SEARCH / CYCLE: the base term starts the extra columns, the recursive term extends them.
        let base_cols: Vec<Expr> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| Expr::Col(i, c.ty.clone()))
            .collect();
        let mut base_exprs = base_cols.clone();
        base_exprs.extend(aux.start(&base_cols));
        let passed: Vec<Expr> = aux
            .cols
            .iter()
            .enumerate()
            .map(|(k, c)| Expr::Col(ncols + k, c.ty.clone()))
            .collect();
        let next = aux.next(&exprs, &passed);
        exprs.extend(next);
        // (Deparsing, the CYCLE check is the recursive term's filter; planned, its work
        // table scan's.)
        let mut rec = rec;
        if self.deparse
            && let Some(pred) = aux.not_cycle(&passed)
        {
            rec = Plan::Filter {
                input: Arc::new(rec),
                pred,
            };
        }
        let rec = Plan::Project {
            input: Arc::new(rec),
            exprs,
        };
        // (Over a row of constants the added columns are constants too, as
        // eval_const_expressions folds them.)
        let (base, base_exprs) = match &base {
            Plan::Project { input, exprs: consts }
                if matches!(&**input, Plan::One) && consts.iter().all(|e| matches!(e, Expr::Const(..))) =>
            {
                let folded: Vec<Expr> = base_exprs
                    .iter()
                    .map(|e| {
                        let sub = e
                            .clone()
                            .transform(&mut |x| {
                                Ok(match x {
                                    Expr::Col(i, t) => consts.get(i).cloned().unwrap_or(Expr::Col(i, t)),
                                    x => x,
                                })
                            })
                            .unwrap_or_else(|_| e.clone());
                        match crate::sql::expr::eval(&sub, &[], self.ctx) {
                            Ok(v) if !matches!(e, Expr::Col(..)) => Expr::Const(v, sub.ty()),
                            _ => e.clone(),
                        }
                    })
                    .collect();
                (base.clone(), folded)
            }
            _ => (base, base_exprs),
        };
        let base = Plan::Project {
            input: Arc::new(base),
            exprs: base_exprs,
        };
        let mut cols = cols;
        cols.extend(aux.cols.iter().cloned());
        Ok((
            Plan::Recursive {
                id,
                base: Arc::new(base),
                recursive: Arc::new(rec),
                all: s.all,
                ncols: ncols + aux_cols,
            },
            cols,
        ))
    }

    /// Validate a CTE's SEARCH / CYCLE clauses against its columns and describe the columns they add.
    fn search_cycle_columns(
        &mut self,
        cte: &pb::CommonTableExpr,
        cols: &[OutCol],
    ) -> Result<SearchCycle> {
        let mut sc = SearchCycle::default();
        let lookup = |list: &[Node], what: &str, loc: i32| -> Result<Vec<usize>> {
            let mut out: Vec<usize> = Vec::new();
            for n in names(list) {
                let Some(i) = cols.iter().position(|c| c.name == n) else {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("{what} column \"{n}\" not in WITH query column list"),
                        ),
                        loc,
                    ));
                };
                if out.contains(&i) {
                    return Err(with_pos(
                        Error::new(
                            code::DUPLICATE_COLUMN,
                            format!("{what} column \"{n}\" specified more than once"),
                        ),
                        loc,
                    ));
                }
                out.push(i);
            }
            Ok(out)
        };
        let taken = |n: &str, what: &str, loc: i32| -> Result<()> {
            if cols.iter().any(|c| c.name == n) {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "{what} column name \"{n}\" already used in WITH query column list"
                        ),
                    ),
                    loc,
                ));
            }
            Ok(())
        };
        let record_array = DataType::array_of(DataType::Record);
        if let Some(search) = &cte.search_clause {
            let by = lookup(&search.search_col_list, "search", search.location)?;
            taken(
                &search.search_seq_column,
                "search sequence",
                search.location,
            )?;
            let ty = if search.search_breadth_first {
                breadth_row_type(
                    &by.iter().map(|&i| (cols[i].name.clone(), cols[i].ty.clone())).collect::<Vec<_>>(),
                )
            } else {
                record_array.clone()
            };
            sc.cols
                .push(OutCol::new(search.search_seq_column.clone(), ty));
            sc.search = Some((by, search.search_breadth_first));
        }
        if let Some(cycle) = &cte.cycle_clause {
            let by = lookup(&cycle.cycle_col_list, "cycle", cycle.location)?;
            taken(&cycle.cycle_mark_column, "cycle mark", cycle.location)?;
            taken(&cycle.cycle_path_column, "cycle path", cycle.location)?;
            let same = |a: &str, b: &str, msg: &str, loc: i32| -> Result<()> {
                if a == b {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, msg.to_string()),
                        loc,
                    ));
                }
                Ok(())
            };
            same(
                &cycle.cycle_mark_column,
                &cycle.cycle_path_column,
                "cycle mark column name and cycle path column name are the same",
                cycle.location,
            )?;
            if let Some(search) = &cte.search_clause {
                same(
                    &search.search_seq_column,
                    &cycle.cycle_mark_column,
                    "search sequence column name and cycle mark column name are the same",
                    search.location,
                )?;
                same(
                    &search.search_seq_column,
                    &cycle.cycle_path_column,
                    "search sequence column name and cycle path column name are the same",
                    search.location,
                )?;
            }
            let (to, default) = match (&cycle.cycle_mark_value, &cycle.cycle_mark_default) {
                (Some(t), Some(d)) => {
                    let dloc = ast::location(d);
                    let t = self.in_sibling(|b| b.bind_expr(t))?;
                    let d = self.in_sibling(|b| b.bind_expr(d))?;
                    let (v, _) = self
                        .unify(vec![t, d], "CYCLE")
                        .map_err(|e| if e.position.is_none() { with_pos(e, dloc) } else { e })?;
                    let mut v = v.into_iter();
                    (v.next().unwrap(), v.next().unwrap())
                }
                _ => (
                    Expr::Const(Value::Bool(true), DataType::Bool),
                    Expr::Const(Value::Bool(false), DataType::Bool),
                ),
            };
            let ty = to.ty();
            require_equality(&ty, -1)?;
            let ne = crate::sql::func::resolve_binop("<>", &ty, &ty)
                .map_err(|e| with_pos(e, cycle.location))?
                .def;
            sc.cols
                .push(OutCol::new(cycle.cycle_mark_column.clone(), ty));
            sc.cols.push(OutCol::new(
                cycle.cycle_path_column.clone(),
                record_array.clone(),
            ));
            sc.cycle = Some(Cycle {
                by,
                to,
                default,
                ne,
            });
        }
        if !sc.cols.is_empty() {
            // (rewriteSearchAndCycle: array_cat(path, ARRAY[ROW(...)]) and
            // int8inc(depth), function calls.)
            sc.append = Some(
                crate::sql::func::resolve_function("array_cat", &[record_array.clone(), record_array.clone()])?.def,
            );
            sc.eq = Some(
                crate::sql::func::resolve_binop("=", &DataType::Record, &DataType::Record)?.def,
            );
            sc.plus = Some(crate::sql::func::resolve_function("int8inc", &[DataType::Int8])?.def);
            sc.names = cols.iter().map(|c| c.name.clone()).collect();
        }
        Ok(sc)
    }

    /// Cast a plan's output columns to `types` where they differ.
    pub(crate) fn coerce_plan_types(
        &mut self,
        plan: Plan,
        cur: &[DataType],
        types: &[DataType],
    ) -> Result<Plan> {
        if cur.iter().zip(types).all(|(a, b)| a == b) {
            return Ok(plan);
        }
        // Coerce a projection's expressions in place (literals then fold, with their position).
        if let Plan::Project { input, exprs } = &plan
            && exprs.len() == types.len()
        {
            let mut out = Vec::with_capacity(exprs.len());
            for (e, t) in exprs.iter().zip(types) {
                out.push(if e.ty() == *t {
                    e.clone()
                } else {
                    self.coerce_implicit(e.clone(), t)?
                });
            }
            return Ok(Plan::Project {
                input: input.clone(),
                exprs: out,
            });
        }
        let mut exprs = Vec::new();
        for (ct, t) in cur.iter().zip(types) {
            let e = Expr::Col(exprs.len(), ct.clone());
            exprs.push(self.coerce_implicit(e, t)?);
        }
        Ok(Plan::Project {
            input: Arc::new(plan),
            exprs,
        })
    }

    // ── set operations ──

    fn bind_set_op(
        &mut self,
        s: &pb::SelectStmt,
        op: pb::SetOperation,
    ) -> Result<(Plan, Vec<OutCol>)> {
        let (kind, what) = match op {
            pb::SetOperation::SetopUnion => (SetOpKind::Union, "UNION"),
            pb::SetOperation::SetopIntersect => (SetOpKind::Intersect, "INTERSECT"),
            _ => (SetOpKind::Except, "EXCEPT"),
        };
        let union_all = kind == SetOpKind::Union && s.all;
        let leaf = |b: &mut Self, n: &pb::SelectStmt| {
            let nested = !matches!(
                pb::SetOperation::try_from(n.op).unwrap_or(pb::SetOperation::SetopNone),
                pb::SetOperation::SetopNone | pb::SetOperation::Undefined
            );
            let index = if nested {
                None
            } else {
                b.setop_leaves.map(|i| {
                    b.setop_leaves = Some(i + 1);
                    i + 1
                })
            };
            // (Its own set operation's leaves are numbered apart.)
            let outer = if nested { None } else { b.setop_leaves.take() };
            b.keep_unknown = true;
            let r = b.in_sibling(|b| b.bind_select_stmt(n));
            b.keep_unknown = false;
            if outer.is_some() {
                b.setop_leaves = outer;
            }
            // A UNION ALL's leaf query Postgres keeps a query level (`*SELECT* n`, a
            // Subquery Scan when not trivial) rather than pulling it up.
            let (plan, cols) = r?;
            let plan = match index {
                Some(i)
                    if union_all
                        && !b.deparse
                        && !b.unplanned
                        && b.deps.is_none()
                        && b.refs.is_none()
                        && (own_query_level_stmt(n, &plan) || !safe_append_member(n)) =>
                {
                    Plan::Barrier {
                        input: Arc::new(plan),
                        alias: format!("*SELECT* {i}"),
                        cols: cols.iter().map(|c| c.name.clone()).collect(),
                        secure: false,
                        path: None,
                        sorted: (!n.sort_clause.is_empty()).then_some(n.sort_clause.len()),
                        rti: Default::default(),
                        copies: Vec::new(),
                    }
                }
                _ => plan,
            };
            Ok::<_, Error>((plan, cols))
        };
        let (lp, lcols) = leaf(self, s.larg.as_deref().unwrap())?;
        let (rp, rcols) = leaf(self, s.rarg.as_deref().unwrap())?;
        if lcols.len() != rcols.len() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!("each {what} query must have the same number of columns"),
            ));
        }
        let mut types = Vec::new();
        for (l, r) in lcols.iter().zip(&rcols) {
            types.push(super::unify_types(&[l.ty.clone(), r.ty.clone()], what)?);
        }
        // All but UNION ALL compare rows; errors point at the leftmost query's column.
        if kind != SetOpKind::Union || !s.all {
            let mut first = s;
            while let Some(l) = first.larg.as_deref() {
                first = l;
            }
            let locs = target_locations(&first.target_list, types.len());
            for (t, loc) in types.iter().zip(locs) {
                require_equality(t, loc)?;
            }
        }
        let same = |c: &OutCol, t: &DataType| matches!(c.ty, DataType::Unknown) || c.ty.same_base(t);
        if !lcols.iter().zip(&types).all(|(c, t)| same(c, t)) || !rcols.iter().zip(&types).all(|(c, t)| same(c, t)) {
            self.setop_coerced = true;
        }
        let lp = self.coerce_plan_types(
            lp,
            &lcols.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(),
            &types,
        )?;
        let rp = self.coerce_plan_types(
            rp,
            &rcols.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(),
            &types,
        )?;
        // The output collations (select_common_collation): differing explicit ones are an
        // error, and so are differing implicit ones when rows are compared.
        let none_ok = kind == SetOpKind::Union && s.all;
        // (At the right query's column: the column reference in it, which brings its
        // collation in.)
        let rlocs = {
            let mut first = s.rarg.as_deref().unwrap();
            while let Some(l) = first.larg.as_deref() {
                first = l;
            }
            let mut locs = target_locations(&first.target_list, types.len());
            if first.target_list.len() == types.len() {
                for (loc, t) in locs.iter_mut().zip(&first.target_list) {
                    if let Some(NodeEnum::ResTarget(r)) = inner(t)
                        && let Some(ne) = r.val.as_deref().and_then(|v| v.node.as_ref())
                        && let Some(at) = ne
                            .nodes()
                            .into_iter()
                            .filter_map(|(n, ..)| match n {
                                pg_query::NodeRef::ColumnRef(c) => Some(c.location),
                                _ => None,
                            })
                            .min()
                    {
                        *loc = at;
                    }
                }
            }
            locs
        };
        let mut colls = Vec::with_capacity(types.len());
        for (i, ((l, r), t)) in lcols.iter().zip(&rcols).zip(&types).enumerate() {
            if !t.collatable() {
                colls.push(None);
                continue;
            }
            let (lc, rc) = (self.out_col_collation(l, t), self.out_col_collation(r, t));
            let merged = match (&lc, &rc) {
                (super::Coll::Implicit(a), super::Coll::Implicit(b))
                    if a != b && a != "default" && b != "default" =>
                {
                    if !none_ok {
                        return Err(with_pos(
                            Error::new(
                                code::COLLATION_MISMATCH,
                                format!(
                                    "collation mismatch between implicit collations \"{a}\" and \"{b}\""
                                ),
                            )
                            .hint("You can choose the collation by applying the COLLATE clause to one or both expressions."),
                            rlocs[i],
                        ));
                    }
                    super::Coll::None
                }
                _ => lc.merge(rc)?,
            };
            colls.push(Some(merged.implicit()));
        }
        let cols = lcols
            .iter()
            .zip(&rcols)
            .zip(&types)
            .zip(colls)
            .map(|(((c, r), t), collation)| {
                // (select_common_type keeps a domain both sides are declared as.)
                let domain = c.domain.filter(|d| r.domain == Some(*d) && c.ty.same_base(t));
                OutCol {
                    name: c.name.clone(),
                    ty: t.clone(),
                    table: 0,
                    attnum: 0,
                    domain_array: self.domain_array(domain),
                    domain,
                    collation,
                }
            })
            .collect();
        let plan = if kind == SetOpKind::Union && s.all {
            Plan::Append {
                inputs: vec![Arc::new(lp), Arc::new(rp)],
                inh: None,
                alias: None,
                pw: None,
            }
        } else {
            // Columns under a nondeterministic collation compare by it.
            let nondet: Vec<Option<String>> = (&cols as &Vec<OutCol>)
                .iter()
                .map(|c| match self.out_col_collation(c, &c.ty) {
                    super::Coll::Implicit(n) | super::Coll::Explicit(n, _)
                        if c.ty.collatable()
                            && crate::types::collation::by_name(&self.cat, &n)
                                .is_some_and(|k| !k.deterministic) =>
                    {
                        Some(n)
                    }
                    _ => None,
                })
                .collect();
            let (mut left, mut right) = (Arc::new(lp), Arc::new(rp));
            // generate_nonunion_paths: INTERSECT puts the input with fewer groups first.
            if kind == SetOpKind::Intersect
                && !self.deparse
                && crate::sql::explain::setop_shape(&left, &right, true, self.ctx).0
            {
                std::mem::swap(&mut left, &mut right);
            }
            Plan::SetOp {
                kind,
                all: s.all,
                left,
                right,
                nondet: if nondet.iter().any(Option::is_some) {
                    nondet
                } else {
                    Vec::new()
                },
                sub: None,
            }
        };
        Ok((plan, cols))
    }

    /// The collation a table or view made from a query's output column `c` gets, when not
    /// its type's default; an error when it derives none. `what` names the column
    /// (`view column "x"`), `ctas` picks CREATE TABLE AS's message.
    pub(crate) fn stored_collation(&self, c: &OutCol, ty: &DataType, ctas: bool) -> Result<Option<String>> {
        if !ty.collatable() {
            return Ok(None);
        }
        match self.out_col_collation(c, ty) {
            super::Coll::Implicit(n) | super::Coll::Explicit(n, _) => {
                Ok((ty.default_collation() != Some(n.as_str())).then_some(n))
            }
            _ if ctas => Err(Error::new(
                code::INDETERMINATE_COLLATION,
                format!(
                    "no collation was derived for column \"{}\" with collatable type {}",
                    c.name,
                    ty.display()
                ),
            )
            .hint("Use the COLLATE clause to set the collation explicitly.")),
            _ => Err(Error::new(
                code::INDETERMINATE_COLLATION,
                format!(
                    "could not determine which collation to use for view column \"{}\"",
                    c.name
                ),
            )
            .hint("Use the COLLATE clause to set the collation explicitly.")),
        }
    }

    /// The collation the column `e` of `scope` derives.
    fn scope_col_collation(&self, scope: &Scope, e: &Expr) -> super::Coll {
        if !e.ty().collatable() {
            return super::Coll::None;
        }
        let sc = scope
            .rels
            .iter()
            .flat_map(|r| &r.cols)
            .find(|c| c.expr.same_as(e));
        if let Some(c) = sc.and_then(|sc| sc.collation.clone()) {
            return c;
        }
        let declared = sc.and_then(|sc| self.declared_collation(sc.table, sc.attnum));
        super::Coll::Implicit(
            declared.unwrap_or_else(|| e.ty().default_collation().unwrap_or_default().to_string()),
        )
    }

    /// A sort / group key (`e`, written as `n`, an output position or expression) whose
    /// implicit collations conflict is an error, at the expression bringing in the second.
    pub(crate) fn check_key_collation(&self, e: &Expr, n: &Node, targets: &[Node]) -> Result<()> {
        if !e.ty().collatable() {
            return Ok(());
        }
        let super::Coll::Conflict(a, b) = self.collation_of(e)? else {
            return Ok(());
        };
        let n = match ast::ival(n).filter(|_| matches!(inner(n), Some(NodeEnum::AConst(_)))) {
            Some(p) => match usize::try_from(p - 1).ok().and_then(|i| targets.get(i)).and_then(inner) {
                Some(NodeEnum::ResTarget(r)) => r.val.as_deref().unwrap_or(n),
                _ => n,
            },
            None => n,
        };
        // The leftmost column reference deriving the second collation.
        let mut at = -1;
        if let Some(ne) = n.node.as_ref() {
            for (node, ..) in ne.nodes() {
                let pg_query::NodeRef::ColumnRef(c) = node else {
                    continue;
                };
                let parts: Vec<&str> = c.fields.iter().filter_map(ast::sval).collect();
                let Some((col, rel)) = parts.split_last() else {
                    continue;
                };
                let found = self
                    .scope()
                    .rels
                    .iter()
                    .filter(|r| rel.last().is_none_or(|q| r.alias == *q))
                    .flat_map(|r| &r.cols)
                    .find(|sc| !sc.hidden && sc.name == *col);
                if let Some(sc) = found
                    && matches!(self.collation_of(&sc.expr), Ok(super::Coll::Implicit(x)) if x == b)
                    && (at < 0 || c.location < at)
                {
                    at = c.location;
                }
            }
        }
        Err(with_pos(
            Error::new(
                code::COLLATION_MISMATCH,
                format!("collation mismatch between implicit collations \"{a}\" and \"{b}\""),
            )
            .hint("You can choose the collation by applying the COLLATE clause to one or both expressions."),
            at,
        ))
    }

    /// The collation an output column derives.
    pub(crate) fn out_col_collation(&self, c: &OutCol, ty: &DataType) -> super::Coll {
        if let Some(coll) = &c.collation {
            return coll.clone();
        }
        let declared = self.declared_collation(c.table, c.attnum);
        super::Coll::Implicit(
            declared.unwrap_or_else(|| ty.default_collation().unwrap_or_default().to_string()),
        )
    }

    // ── VALUES ──

    pub(crate) fn bind_values(&mut self, lists: &[Node]) -> Result<(Plan, Vec<OutCol>)> {
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        for l in lists {
            let Some(NodeEnum::List(items)) = inner(l) else {
                continue;
            };
            let row = self.in_clause(Clause::Values, |b| {
                let mut row = Vec::new();
                for i in &items.items {
                    row.extend(b.bind_values_item(i)?);
                }
                Ok(row)
            })?;
            if let Some(first) = rows.first()
                && first.len() != row.len()
            {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "VALUES lists must all be the same length",
                ));
            }
            rows.push(row);
        }
        let ncols = rows.first().map(|r| r.len()).unwrap_or(0);
        let mut types = Vec::new();
        for c in 0..ncols {
            let col: Vec<DataType> = rows.iter().map(|r| r[c].ty()).collect();
            types.push(super::unify_types(&col, "VALUES")?);
        }
        // (select_common_type keeps a domain all the rows' values have.)
        let domains: Vec<Option<u32>> = (0..ncols)
            .map(|c| {
                let col: Vec<Expr> = rows.iter().map(|r| r[c].clone()).collect();
                self.common_domain(&col)
            })
            .collect();
        for r in &mut rows {
            for (c, t) in types.iter().enumerate() {
                let e = std::mem::replace(&mut r[c], Expr::Const(Value::Null, DataType::Unknown));
                r[c] = if matches!(e, Expr::Default(_)) {
                    e
                } else {
                    self.coerce_implicit(e, t)?
                };
            }
        }
        let mut cols: Vec<OutCol> = types
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut c = OutCol::new(format!("column{}", i + 1), t.clone());
                c.domain = domains[i];
                c.domain_array = self.domain_array(c.domain);
                c
            })
            .collect();
        // Each column's collation, from all the rows' (none when implicit ones conflict).
        for (c, col) in cols.iter_mut().enumerate() {
            if !col.ty.collatable() {
                continue;
            }
            let mut merged = super::Coll::None;
            for r in &rows {
                if !matches!(r[c], Expr::Default(_)) {
                    merged = merged.merge(self.collation_of(&r[c])?)?;
                }
            }
            col.collation = Some(match merged {
                super::Coll::None => super::Coll::Implicit(
                    col.ty.default_collation().unwrap_or_default().to_string(),
                ),
                m => m,
            });
        }
        let rti = self.next_rti();
        Ok((Plan::Values { rows, ncols, rti }, cols))
    }

    /// ORDER BY / LIMIT over a set operation or VALUES: keys are output columns.
    fn finish_over_output(
        &mut self,
        plan: Plan,
        cols: Vec<OutCol>,
        s: &pb::SelectStmt,
    ) -> Result<(Plan, Vec<OutCol>)> {
        if let Some(c) = s.locking_clause.first() {
            // (LCS_asString of the first locking clause.)
            let what = match inner(c) {
                Some(NodeEnum::LockingClause(lc)) => match pb::LockClauseStrength::try_from(lc.strength)
                    .unwrap_or(pb::LockClauseStrength::LcsForupdate)
                {
                    pb::LockClauseStrength::LcsForkeyshare => "FOR KEY SHARE",
                    pb::LockClauseStrength::LcsForshare => "FOR SHARE",
                    pb::LockClauseStrength::LcsFornokeyupdate => "FOR NO KEY UPDATE",
                    _ => "FOR UPDATE",
                },
                _ => "FOR UPDATE",
            };
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("{what} is not allowed with UNION/INTERSECT/EXCEPT"),
            ));
        }
        let mut plan = plan;
        if !s.sort_clause.is_empty() {
            // The set operation's leaf queries (`*SELECT* n`), which an ORDER BY can't
            // reference but a missing column's error names.
            fn leaves<'a>(s: &'a pb::SelectStmt, out: &mut Vec<&'a pb::SelectStmt>) {
                match (s.larg.as_deref(), s.rarg.as_deref()) {
                    (Some(l), Some(r)) => {
                        leaves(l, out);
                        leaves(r, out);
                    }
                    _ => out.push(s),
                }
            }
            let mut leaf_list = Vec::new();
            if s.larg.is_some() {
                leaves(s, &mut leaf_list);
            }
            let unreachable = self.unreachable.len();
            for (i, leaf) in leaf_list.iter().enumerate() {
                if !leaf.values_lists.is_empty() {
                    continue;
                }
                let names: Vec<OutCol> = leaf
                    .target_list
                    .iter()
                    .filter_map(|t| match inner(t) {
                        Some(NodeEnum::ResTarget(rt)) => Some(OutCol::new(
                            if rt.name.is_empty() {
                                rt.val.as_deref().map_or_else(|| "?column?".into(), figure_colname)
                            } else {
                                rt.name.clone()
                            },
                            DataType::Unknown,
                        )),
                        _ => None,
                    })
                    .collect();
                self.unreachable.push(derived_rel(&format!("*SELECT* {}", i + 1), &names, 0));
            }
            // (A VALUES list's ORDER BY: its `*VALUES*` relation.)
            if s.larg.is_none() && !s.values_lists.is_empty() {
                self.unreachable.push(derived_rel("*VALUES*", &cols, 0));
            }
            let rel = derived_rel("", &cols, 0);
            let saved = std::mem::replace(
                &mut self.level().scope,
                Scope {
                    rels: vec![ScopeRel {
                        anonymous: false,
                        ..rel
                    }],
                },
            );
            let mut keys = Vec::new();
            let r = (|| -> Result<()> {
                for sb in &s.sort_clause {
                    let Some(NodeEnum::SortBy(sb)) = inner(sb) else {
                        continue;
                    };
                    let node = sb.node.as_deref().unwrap();
                    non_integer_constant(node, "ORDER BY")?;
                    let e = match ast::ival(node)
                        .filter(|_| matches!(inner(node), Some(NodeEnum::AConst(_))))
                    {
                        Some(p) => {
                            if p < 1 || p as usize > cols.len() {
                                return Err(Error::new(
                                    code::INVALID_COLUMN_REFERENCE,
                                    format!("ORDER BY position {p} is not in select list"),
                                ));
                            }
                            Expr::Col(p as usize - 1, cols[p as usize - 1].ty.clone())
                        }
                        None => self.in_clause(Clause::OrderBy, |b| b.bind_expr(node))?,
                    };
                    keys.push(self.sort_key_for(e, sb)?);
                }
                Ok(())
            })();
            self.level().scope = saved;
            self.unreachable.truncate(unreachable);
            r?;
            plan = Plan::Sort {
                input: Arc::new(plan),
                keys,
            };
        }
        plan = self.apply_limit(plan, s, None)?;
        // (A UNION ALL's arms are planned; its order may merge them.)
        if !self.deparse {
            crate::sql::paths::plan_ordered_append(self.ctx, &mut plan);
        }
        Ok((plan, cols))
    }

    fn apply_limit(
        &mut self,
        plan: Plan,
        s: &pb::SelectStmt,
        ties: Option<Vec<SortKey>>,
    ) -> Result<Plan> {
        if s.limit_count.is_none() && s.limit_offset.is_none() {
            return Ok(plan);
        }
        // checkExprIsVarFree: no column of this query level.
        let var_free = |e: &Expr, n: &Node, what: &str| -> Result<()> {
            if e.columns().is_empty() {
                return Ok(());
            }
            let mut at = -1;
            if let Some(ne) = n.node.as_ref() {
                for (x, ..) in ne.nodes() {
                    if let pg_query::NodeRef::ColumnRef(c) = x
                        && (at < 0 || c.location < at)
                    {
                        at = c.location;
                    }
                }
            }
            Err(with_pos(
                Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    format!("argument of {what} must not contain variables"),
                ),
                at,
            ))
        };
        let limit = match s.limit_count.as_deref() {
            Some(n) => {
                let e = self.in_clause(Clause::Limit, |b| b.bind_expr(n))?;
                var_free(&e, n, "LIMIT")?;
                if matches!(e, Expr::Const(Value::Null, _)) {
                    None
                } else {
                    Some(self.coerce_implicit(e, &DataType::Int8)?)
                }
            }
            None => None,
        };
        let offset = match s.limit_offset.as_deref() {
            Some(n) => {
                let e = self.in_clause(Clause::Offset, |b| b.bind_expr(n))?;
                var_free(&e, n, "OFFSET")?;
                Some(self.coerce_implicit(e, &DataType::Int8)?)
            }
            None => None,
        };
        let with_ties = if s.limit_option == pb::LimitOption::WithTies as i32 {
            // (transformLimitClause: a NULL constant can't be the count of rows with ties.)
            if let Some(NodeEnum::AConst(c)) = s.limit_count.as_deref().and_then(inner)
                && c.isnull
            {
                return Err(Error::new(
                    code::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                    "row count cannot be null in FETCH FIRST ... WITH TIES clause",
                ));
            }
            match ties {
                Some(t) if !t.is_empty() => Some(t),
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "WITH TIES cannot be specified without ORDER BY clause",
                    ));
                }
            }
        } else {
            None
        };
        let mut plan = plan;
        if with_ties.is_none()
            && let Some(Expr::Const(Value::Int8(l), _)) = &limit
        {
            let off = match &offset {
                None => Some(0),
                Some(Expr::Const(Value::Int8(o), _)) => Some(*o),
                _ => None,
            };
            if let Some(o) = off
                && *l >= 0
                && o >= 0
            {
                optimize::push_limit(&mut plan, (*l + o) as usize);
            }
        }
        Ok(Plan::Limit {
            input: Arc::new(plan),
            limit,
            offset,
            with_ties,
        })
    }

    // ── the main SELECT ──

    fn bind_simple_select(&mut self, s: &pb::SelectStmt) -> Result<(Plan, Vec<OutCol>)> {
        let keep_unknown = std::mem::take(&mut self.keep_unknown);
        // Named windows.
        for w in &s.window_clause {
            if let Some(NodeEnum::WindowDef(wd)) = inner(w) {
                if self
                    .level()
                    .st
                    .named_windows
                    .iter()
                    .any(|(n, _)| *n == wd.name)
                {
                    return Err(with_pos(
                        Error::new(
                            code::WINDOWING_ERROR,
                            format!("window \"{}\" is already defined", wd.name),
                        ),
                        wd.location,
                    ));
                }
                self.level()
                    .st
                    .named_windows
                    .push((wd.name.clone(), (**wd).clone()));
            }
        }
        let locking = self.locking_spec(&s.locking_clause)?;
        // FROM
        let mut plan = self.bind_from(&s.from_clause, locking.as_ref())?;
        // Grouping sets null the columns a set leaves out: virtual generated columns that
        // wouldn't go NULL with them are computed below (their PlaceHolderVars).
        if !self.deparse
            && s
                .group_clause
                .iter()
                .any(|g| matches!(inner(g), Some(NodeEnum::GroupingSet(_))))
        {
            let scope = std::mem::take(&mut self.level().scope);
            let (p, scope) = materialize_virtuals(plan, scope);
            plan = p;
            self.level().scope = scope;
        }
        // WHERE
        if let Some(w) = s.where_clause.as_deref() {
            let pred = match self.in_clause(Clause::Where, |b| b.bind_expr(w)) {
                Ok(p) => self.coerce_bool_at(p, "WHERE", w),
                Err(e) => Err(e),
            };
            let pred = match pred {
                Ok(p) => p,
                // Postgres analyses the select list before WHERE: an error there comes
                // first (grouping is checked only once the whole query is analysed).
                Err(e) => {
                    return Err(
                        match self.in_clause(Clause::Select, |b| b.bind_targets(&s.target_list)) {
                            Err(t) if t.code != code::GROUPING_ERROR => t,
                            _ => e,
                        },
                    );
                }
            };
            let pred = self.simplify_where(pred);
            let (src, parts) = if self.deparse {
                (Vec::new(), Vec::new())
            } else {
                let mut parts = Vec::new();
                optimize::conjuncts(pred.clone(), &mut parts);
                (row_sources(&plan), parts)
            };
            // (process_equivalence meets the WHERE's equalities after the FROM list's ON
            // clauses, in their order: `JoinMeta::ec_written` of the FROM list's top join.)
            if let Plan::Join { meta, .. } = &mut plan {
                meta.ec_written.extend(optimize::written_equalities_with(&parts, true));
            }
            // EXISTS / IN sublinks become semi / anti joins (pull_up_sublinks); with FOR
            // UPDATE too, its rows locked above the joins (`paths::rowmarks`).
            let pred = if self.deparse {
                Some(pred)
            } else {
                let (p, rest) = super::sublink::pull_up(&self.cat, self.ctx, plan, pred, &self.unplanned_subs);
                plan = p;
                // (The WHERE's join clauses' columns, a sublink's outer values too, then
                // the semi / anti joins its pulled-up sublinks became: those wrap the FROM
                // list, deconstruct_jointree meets them after.)
                let mut left = Vec::new();
                if let Some(r) = &rest {
                    optimize::conjuncts(r.clone(), &mut left);
                }
                for e in &parts {
                    let shaped = match e {
                        Expr::Subquery(_) => true,
                        Expr::Not(x) => matches!(&**x, Expr::Subquery(_)),
                        _ => false,
                    };
                    if !shaped || left.iter().any(|x| x.same_as(e)) {
                        self.note_join_clause(&e.columns(), &src, false);
                    }
                }
                self.note_semi_joins(&plan);
                rest
            };
            if let Some(pred) = pred {
                plan = optimize::add_filter(&self.cat, plan, pred);
            }
        }
        if !self.deparse {
            self.apply_rel_vars(&mut plan);
        }
        // GROUP BY (before the select list: it decides what the select list may reference).
        let (groups, sets) = match self.bind_group_by(s) {
            Ok(g) => g,
            Err(e) if e.code == code::AMBIGUOUS_COLUMN => {
                return Err(self.order_by_ambiguity(s).unwrap_or(e));
            }
            Err(e) => return Err(e),
        };
        if sets.is_none() && groups.len() == s.group_clause.len() {
            for (g, n) in groups.iter().zip(&s.group_clause) {
                self.check_key_collation(g, n, &s.target_list)?;
            }
        }
        let grouped = !s.group_clause.is_empty();
        self.level().st.group_srfs = self.level().st.srfs.len();
        self.level().st.group_exprs = groups.clone();
        self.level().st.grouping_sets = sets.is_some();
        // (Grouping sets: the occurrences of keys without columns in the select list and
        // HAVING are kept apart from their surroundings while bound.)
        if sets.is_some() && !self.deparse {
            let st = &self.level().st;
            let marks: Vec<String> = groups
                .iter()
                .zip(&st.group_text)
                .filter(|(g, _)| g.columns().is_empty() && !g.is_volatile() && !g.has_outer_refs())
                .filter_map(|(_, t)| t.clone())
                .collect();
            self.level().st.pass_marks = marks;
        }
        // Select list.
        let targets = self.in_clause(Clause::Select, |b| b.bind_targets(&s.target_list))?;
        // The collations the outputs derive, while the targets are in the FROM row's terms.
        let tgt_colls = targets
            .iter()
            .map(|t| self.output_collation(&t.expr))
            .collect::<Result<Vec<_>>>()?;
        // HAVING
        let having = match s.having_clause.as_deref() {
            Some(h) => {
                let e = self.in_clause(Clause::Having, |b| b.bind_expr(h))?;
                Some(self.coerce_bool_at(e, "HAVING", h)?)
            }
            None => None,
        };
        self.level().st.pass_marks.clear();
        // DISTINCT / DISTINCT ON
        let distinct_all = s.distinct_clause.len() == 1 && s.distinct_clause[0].node.is_none();
        let mut distinct_on: Vec<Expr> = Vec::new();
        if !distinct_all {
            for d in &s.distinct_clause {
                let e = self.in_clause(Clause::DistinctOn, |b| {
                    b.resolve_output_ref(d, &targets, "DISTINCT ON")
                })?;
                require_equality(&e.ty(), ast::location(d))?;
                distinct_on.push(e);
            }
        }
        // ORDER BY
        let mut order: Vec<SortKey> = Vec::new();
        for sb in &s.sort_clause {
            let Some(NodeEnum::SortBy(sb)) = inner(sb) else {
                continue;
            };
            let node = sb.node.as_deref().unwrap();
            let e = match repeated_target(node, &s.target_list, &targets) {
                Some(e) => e,
                None => self.in_clause(Clause::OrderBy, |b| {
                    b.resolve_output_ref(node, &targets, "ORDER BY")
                })?,
            };
            let written = match inner(node) {
                Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => s
                    .target_list
                    .iter()
                    .find_map(|t| match inner(t) {
                        Some(NodeEnum::ResTarget(r))
                            if Some(r.name.as_str()) == ast::sval(&c.fields[0]) =>
                        {
                            r.val.as_deref()
                        }
                        _ => None,
                    })
                    .unwrap_or(node),
                // An expression in the list is that entry.
                _ => targets
                    .iter()
                    .position(|t| t.expr.same_as(&e))
                    .filter(|_| targets.len() == s.target_list.len())
                    .and_then(|i| match inner(&s.target_list[i]) {
                        Some(NodeEnum::ResTarget(r)) => r.val.as_deref(),
                        _ => None,
                    })
                    .unwrap_or(node),
            };
            self.check_key_collation(&e, written, &s.target_list)?;
            order.push(self.sort_key_for(e, sb)?);
        }
        if distinct_all {
            let locs = target_locations(&s.target_list, targets.len());
            for (t, loc) in targets.iter().zip(locs) {
                require_equality(&t.expr.ty(), loc)?;
            }
            if s.target_list.len() == targets.len() {
                for (t, n) in targets.iter().zip(&s.target_list) {
                    let n = match inner(n) {
                        Some(NodeEnum::ResTarget(r)) => r.val.as_deref().unwrap_or(n),
                        _ => n,
                    };
                    self.check_key_collation(&t.expr, n, &s.target_list)?;
                }
            }
            for k in &order {
                if !targets.iter().any(|t| t.expr.same_as(&k.expr)) {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    ));
                }
            }
        }
        if !distinct_on.is_empty() && !order.is_empty() {
            // transformDistinctOnClause: DISTINCT ON expressions must match the leftmost
            // ORDER BY expressions (the error at the DISTINCT ON item found out of place).
            let mismatch = |j: usize| {
                let loc = if s.distinct_clause.len() == distinct_on.len() {
                    ast::location(&s.distinct_clause[j])
                } else {
                    -1
                };
                with_pos(
                    Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                    ),
                    loc,
                )
            };
            // (Items are the same when they name the same target-list entry, by position or
            // as the first entry equal to them (findTargetlistEntrySQL99), else when equal:
            // `ORDER BY 3` is entry 3 even if entry 1 is written alike.)
            let entry = |n: &Node, e: &Expr| -> Option<usize> {
                if let Some(NodeEnum::AConst(c)) = inner(n)
                    && let Some(pb::a_const::Val::Ival(i)) = &c.val
                {
                    return usize::try_from(i.ival - 1).ok();
                }
                targets.iter().position(|t| t.expr.same_as(e))
            };
            let on_entries: Vec<Option<usize>> =
                s.distinct_clause.iter().zip(&distinct_on).map(|(n, e)| entry(n, e)).collect();
            let order_entries: Vec<Option<usize>> = s
                .sort_clause
                .iter()
                .filter_map(|sb| match inner(sb) {
                    Some(NodeEnum::SortBy(sb)) => sb.node.as_deref(),
                    _ => None,
                })
                .zip(&order)
                .map(|(n, k)| entry(n, &k.expr))
                .collect();
            let same = |j: usize, i: usize| match (on_entries.get(j), order_entries.get(i)) {
                (Some(Some(a)), Some(Some(b))) => a == b,
                (Some(None), Some(None)) | (None, _) | (_, None) => distinct_on[j].same_as(&order[i].expr),
                _ => false,
            };
            let mut skipped = false;
            for i in 0..order.len() {
                // (An item naming an entry the sort list has already isn't added again.)
                let dup = (0..i).any(|p| match (order_entries.get(p), order_entries.get(i)) {
                    (Some(Some(a)), Some(Some(b))) => a == b,
                    (Some(None), Some(None)) => order[p].expr.same_as(&order[i].expr),
                    _ => false,
                });
                if dup {
                    continue;
                }
                match (0..distinct_on.len()).find(|&j| same(j, i)) {
                    Some(j) if skipped => return Err(mismatch(j)),
                    Some(_) => {}
                    None => skipped = true,
                }
            }
            if skipped
                && let Some(j) = (0..distinct_on.len()).find(|&j| !(0..order.len()).any(|i| same(j, i)))
            {
                return Err(mismatch(j));
            }
        }

        let mut st = std::mem::take(&mut self.level().st);
        // Per spec, aggregates can't appear in a recursive term (parseCheckAggregates).
        if st.self_ref && !st.aggs.is_empty() {
            return Err(with_pos(
                Error::new(
                    code::INVALID_RECURSION,
                    "aggregate functions are not allowed in a recursive query's recursive term",
                ),
                st.agg_loc.unwrap_or(-1),
            ));
        }
        let has_aggs = !st.aggs.is_empty() || grouped || having.is_some();
        // CheckSelectLocking, in its order; LCS_asString of the first locking clause.
        if locking.is_some() {
            let what = if distinct_all || !distinct_on.is_empty() {
                Some("DISTINCT clause")
            } else if grouped {
                Some("GROUP BY clause")
            } else if having.is_some() {
                Some("HAVING clause")
            } else if !st.aggs.is_empty() {
                Some("aggregate functions")
            } else if !st.windows.is_empty() {
                Some("window functions")
            } else if !st.srfs.is_empty() {
                Some("set-returning functions in the target list")
            } else {
                None
            };
            if let Some(what) = what {
                let strength = match s.locking_clause.first().and_then(inner) {
                    Some(NodeEnum::LockingClause(lc)) => {
                        match pb::LockClauseStrength::try_from(lc.strength)
                            .unwrap_or(pb::LockClauseStrength::LcsForupdate)
                        {
                            pb::LockClauseStrength::LcsForkeyshare => "FOR KEY SHARE",
                            pb::LockClauseStrength::LcsForshare => "FOR SHARE",
                            pb::LockClauseStrength::LcsFornokeyupdate => "FOR NO KEY UPDATE",
                            _ => "FOR UPDATE",
                        }
                    }
                    // (Pushed down from the query above: its clause's.)
                    _ => match locking.as_ref().map(|l| l.first_mark()) {
                        Some(RowMark::KeyShare) => "FOR KEY SHARE",
                        Some(RowMark::Share) => "FOR SHARE",
                        Some(RowMark::NoKeyUpdate) => "FOR NO KEY UPDATE",
                        _ => "FOR UPDATE",
                    },
                };
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{strength} is not allowed with {what}"),
                ));
            }
        }

        // Expressions still in the FROM row's terms.
        let mut tgt_exprs: Vec<Expr> = targets.iter().map(|t| t.expr.clone()).collect();
        let mut having = having;
        let mut order_exprs: Vec<Expr> = order.iter().map(|k| k.expr.clone()).collect();
        let mut on_exprs = distinct_on;
        // Untyped literals in the select list are text (resolveTargetListUnknowns), unless a
        // set operation or the INSERT above resolves them (p_resolve_unknowns off); a GROUP BY,
        // ORDER BY or DISTINCT key that is one is text either way, and so is the entry it is
        // (addTargetToGroupList, addTargetToSortList).
        let mut groups = groups;
        let unknown = |e: &Expr| matches!(e.ty(), DataType::Unknown);
        for t in tgt_exprs.iter_mut() {
            if unknown(t)
                && (!keep_unknown
                    || distinct_all
                    || order_exprs.iter().chain(&on_exprs).chain(&groups).any(|k| k.same_as(t)))
            {
                let e = std::mem::replace(t, Expr::null(DataType::Unknown));
                *t = self.coerce_implicit(e, &DataType::Text)?;
            }
        }
        for e in order_exprs.iter_mut().chain(on_exprs.iter_mut()).chain(groups.iter_mut()) {
            if unknown(e) {
                let k = std::mem::replace(e, Expr::null(DataType::Unknown));
                *e = self.coerce_implicit(k, &DataType::Text)?;
            }
        }
        let mut windows = std::mem::take(&mut st.windows);
        let mut srfs = std::mem::take(&mut st.srfs);
        let sub_params = std::mem::take(&mut st.sub_params);

        // Aggregation.
        let mut sets = sets;
        // Set-returning functions in GROUP BY run below the aggregation (and the select
        // list's same calls are the grouped values).
        let srf_in_group = |e: &Expr| {
            let mut found = false;
            e.walk(&mut |x| found |= matches!(x, Expr::Srf(..)));
            found
        };
        if has_aggs && groups.iter().any(srf_in_group) {
            // The calls GROUP BY uses, with those their arguments use.
            let mut below = vec![false; srfs.len()];
            let mark = |e: &Expr, below: &mut Vec<bool>| {
                e.walk(&mut |x| {
                    if let Expr::Srf(j, _) = x {
                        below[*j] = true;
                    }
                })
            };
            for g in &groups {
                mark(g, &mut below);
            }
            for i in (0..srfs.len()).rev() {
                if below[i] {
                    let args = srfs[i].args.clone();
                    for a in &args {
                        mark(a, &mut below);
                    }
                }
            }
            let below_ids: Vec<usize> = (0..srfs.len()).filter(|&i| below[i]).collect();
            let rest_ids: Vec<usize> = (0..srfs.len()).filter(|&i| !below[i]).collect();
            let renum = |ids: &[usize], e: Expr| {
                replace_placeholders(e, &|x| match x {
                    Expr::Srf(j, t) => ids
                        .iter()
                        .position(|i| i == j)
                        .map(|k| Expr::Srf(k, t.clone())),
                    _ => None,
                })
            };
            let mut calls = Vec::new();
            for &i in &below_ids {
                let mut c = srfs[i].clone();
                c.args = std::mem::take(&mut c.args)
                    .into_iter()
                    .map(|a| renum(&below_ids, a))
                    .collect::<Result<_>>()?;
                calls.push(c);
            }
            let (p, pos, _) = srf_stages(plan, calls, true)?;
            plan = p;
            let tx = |e: Expr| -> Result<Expr> {
                let e = replace_placeholders(e, &|x| match x {
                    Expr::Srf(j, t) => below_ids
                        .iter()
                        .position(|i| i == j)
                        .map(|k| Expr::Col(pos[k], t.clone())),
                    _ => None,
                })?;
                renum(&rest_ids, e)
            };
            groups = groups.into_iter().map(tx).collect::<Result<_>>()?;
            tgt_exprs = tgt_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            if let Some(h) = having.take() {
                having = Some(tx(h)?);
            }
            order_exprs = order_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            on_exprs = on_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            for w in &mut windows {
                w.args = std::mem::take(&mut w.args).into_iter().map(tx).collect::<Result<_>>()?;
                w.partition_by = std::mem::take(&mut w.partition_by)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
                for k in &mut w.order_by {
                    k.expr = tx(k.expr.clone())?;
                }
            }
            for a in &mut st.aggs {
                a.args = std::mem::take(&mut a.args).into_iter().map(tx).collect::<Result<_>>()?;
                a.direct = std::mem::take(&mut a.direct).into_iter().map(tx).collect::<Result<_>>()?;
                if let Some(f) = a.filter.take() {
                    a.filter = Some(tx(f)?);
                }
                for k in &mut a.order_by {
                    k.expr = tx(k.expr.clone())?;
                }
            }
            let mut rest = Vec::new();
            for &i in &rest_ids {
                let mut c = srfs[i].clone();
                c.args = std::mem::take(&mut c.args).into_iter().map(tx).collect::<Result<_>>()?;
                rest.push(c);
            }
            srfs = rest;
        }
        if has_aggs && !groups.is_empty() {
            // Columns functionally dependent on a grouped primary key may appear ungrouped:
            // they group along (check_functional_grouping). With grouping sets, on the
            // columns every set has.
            let mut used: Vec<&Expr> = tgt_exprs.iter().collect();
            used.extend(having.iter());
            used.extend(order_exprs.iter());
            used.extend(on_exprs.iter());
            used.extend(sub_params.iter());
            match &mut sets {
                None => {
                    let extra = self.functionally_dependent(&groups, &groups, &used);
                    groups.extend(extra);
                }
                Some(ss) => {
                    let common: Vec<Expr> = (0..groups.len())
                        .filter(|i| ss.iter().all(|s| s.contains(i)))
                        .map(|i| groups[i].clone())
                        .collect();
                    if !common.is_empty() {
                        let extra = self.functionally_dependent(&common, &groups, &used);
                        for e in extra {
                            groups.push(e);
                            let i = groups.len() - 1;
                            for s in ss.iter_mut() {
                                s.push(i);
                            }
                        }
                    }
                }
            }
        }
        // preprocess_grouping_sets: a set with a column that can't be sorted must be
        // hashed, which a column that can't be hashed (or an ordered aggregate) rules out.
        if let Some(ss) = &sets
            && !self.deparse
        {
            let sortable = |t: &DataType| self.cat.default_opclass(403, t) != 0;
            let hashable = |t: &DataType| self.cat.type_hashable(t);
            let ordered_aggs = st.aggs.iter().any(|a| a.distinct || !a.order_by.is_empty());
            for set in ss {
                let tys: Vec<DataType> = set.iter().map(|&i| groups[i].ty()).collect();
                if tys.iter().any(|t| !sortable(t))
                    && (ordered_aggs || tys.iter().any(|t| !hashable(t)))
                {
                    return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "could not implement GROUP BY")
                        .detail("Some of the datatypes only support hashing, while others only support sorting."));
                }
            }
        }
        if has_aggs {
            let ng = groups.len();
            let naggs = st.aggs.len();
            let mask_col = if sets.is_some() {
                Some(ng + naggs)
            } else {
                None
            };
            let scope = self.scope().clone();
            let tx = |e: Expr| {
                post_agg(e, &groups, ng, mask_col, &scope, true).map_err(|err| ungrouped_pos(err, s))
            };
            // A select-list item a GROUP BY item named is its own key (and for a key without
            // columns, the first item written as it is, `group_item`): only those take a
            // variable-free key's place, which grouping sets null.
            let mut group_targets = std::mem::take(&mut st.group_targets);
            group_targets.resize(ng, None);
            let mut group_literal = std::mem::take(&mut st.group_literal);
            group_literal.resize(ng, false);
            // (With grouping sets: the constant keys' other occurrences in the select list
            // and HAVING, values passed through from the group's rows.)
            let mut group_text = std::mem::take(&mut st.group_text);
            group_text.resize(ng, None);
            let pass = PassThrough {
                literal: &group_literal,
                texts: &group_text,
                item: Default::default(),
                found: Default::default(),
            };
            // (The item's text as written: a key written as an expression is matched where the
            // item contains it.)
            let item_text = |n: Option<&Node>| n.map(|n| strip_locations(&format!("{n:?}")));
            let target_nodes: Vec<Option<&Node>> = s
                .target_list
                .iter()
                .map(|t| match inner(t) {
                    Some(NodeEnum::ResTarget(rt)) => rt.val.as_deref(),
                    _ => None,
                })
                .collect();
            let pass_on = sets.is_some() && !self.deparse;
            let tx_pass = |e: Expr, outer_keys: bool| {
                post_agg_with(e, &groups, ng, mask_col, &scope, outer_keys, pass_on.then_some(&pass))
                    .map_err(|err| ungrouped_pos(err, s))
            };
            tgt_exprs = tgt_exprs
                .into_iter()
                .enumerate()
                .map(|(i, e)| {
                    *pass.item.borrow_mut() = item_text(target_nodes.get(i).copied().flatten());
                    match group_targets.iter().position(|t| *t == Some(i)) {
                        Some(k) if k < ng && groups[k].same_as(&e) => Ok(Expr::Col(k, e.ty())),
                        _ => tx_pass(e, true),
                    }
                })
                .collect::<Result<_>>()?;
            if let Some(h) = having.take() {
                // (A key of outer references only is a Param to HAVING, which moves to WHERE:
                // not nulled there, unlike in the select list.)
                *pass.item.borrow_mut() = item_text(s.having_clause.as_deref());
                having = Some(tx_pass(h, false)?);
            }
            // A sort key that is a constant names the select-list item equal to it (by
            // position or findTargetlistEntrySQL99), a grouped key then.
            let tx_key = |e: Expr| match &e {
                Expr::Const(..) => match groups.iter().position(|g| g.same_as(&e)) {
                    Some(k) if group_targets.get(k).is_some_and(|t| t.is_some()) => {
                        Ok(Expr::Col(k, e.ty()))
                    }
                    _ => tx(e),
                },
                _ => tx(e),
            };
            // (With pass-through values, a constant sort key is the item it names, which
            // may be one: `ORDER BY 2` over `1 AS c0, 1 AS c1` sorts by c1's value.)
            let order_entries: Vec<Option<usize>> = if pass_on {
                s.sort_clause
                    .iter()
                    .map(|sb| match inner(sb) {
                        Some(NodeEnum::SortBy(sb)) => sb.node.as_deref(),
                        _ => None,
                    })
                    .zip(&order_exprs)
                    .map(|(n, e)| {
                        if let Some(NodeEnum::AConst(c)) = n.and_then(inner)
                            && let Some(pb::a_const::Val::Ival(i)) = &c.val
                        {
                            return usize::try_from(i.ival - 1).ok();
                        }
                        targets.iter().position(|t| t.expr.same_as(e))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            order_exprs = order_exprs
                .into_iter()
                .enumerate()
                .map(|(i, e)| match (&e, order_entries.get(i).copied().flatten()) {
                    (Expr::Const(..), Some(t)) if t < tgt_exprs.len() => Ok(tgt_exprs[t].clone()),
                    _ => tx_key(e),
                })
                .collect::<Result<_>>()?;
            on_exprs = on_exprs.into_iter().map(tx_key).collect::<Result<_>>()?;
            for w in &mut windows {
                w.args = std::mem::take(&mut w.args)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
                w.partition_by = std::mem::take(&mut w.partition_by)
                    .into_iter()
                    .map(tx_key)
                    .collect::<Result<_>>()?;
                for k in &mut w.order_by {
                    k.expr = tx_key(k.expr.clone())?;
                }
                if let crate::sql::plan::WinFunc::Agg(a) = &mut w.func {
                    // Aggregate window functions over grouped rows read the grouped columns.
                    a.args = w.args.clone();
                    if let Some(f) = a.filter.take() {
                        a.filter = Some(tx(f)?);
                    }
                }
                w.frame = tx_frame(std::mem::take(&mut w.frame), &tx)?;
            }
            for s in &mut srfs {
                s.args = std::mem::take(&mut s.args)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
            }
            // Ordered-set direct arguments are evaluated once per group, over its grouped columns.
            for a in &mut st.aggs {
                a.direct = std::mem::take(&mut a.direct)
                    .into_iter()
                    .map(|e| post_agg(e, &groups, ng, None, &scope, true).map_err(|err| ungrouped_pos(err.detail("Direct arguments of an ordered-set aggregate must use only grouped columns."), s)))
                    .collect::<Result<_>>()?;
            }
            // Sub-selects' outer references as written (folding may have dropped the
            // sub-select since): grouped, or the query fails as Postgres's parser fails it.
            // (Not those of a sub-select that is a grouping key as a whole.)
            let mut keyed: Vec<Vec<Expr>> = Vec::new();
            for g in &groups {
                g.walk(&mut |x| {
                    if let Expr::Subquery(sq) = x {
                        keyed.push(sq.params.clone());
                    }
                });
            }
            for p in &sub_params {
                if keyed.iter().any(|k| k.iter().any(|x| x.same_as(p))) {
                    continue;
                }
                post_agg(p.clone(), &groups, ng, mask_col, &scope, true).map_err(|mut e| {
                    if e.code == code::GROUPING_ERROR
                        && let Some(rest) = e.message.strip_prefix("column \"")
                        && let Some((name, _)) = rest.split_once("\" must appear")
                    {
                        e.message = format!("subquery uses ungrouped column \"{name}\" from outer query");
                    }
                    ungrouped_pos(e, s)
                })?;
            }
            // The pass-through values: columns after the keys (in no grouping set), the
            // aggregates' and the GROUPING() mask's after them.
            let found = pass.found.into_inner();
            if !found.is_empty() {
                let np = found.len();
                let renumber = |e: Expr| -> Expr {
                    e.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(c, t) if c >= PASS_BASE => Expr::Col(ng + (c - PASS_BASE), t),
                            Expr::Col(c, t) if c >= ng => Expr::Col(c + np, t),
                            x => x,
                        })
                    })
                    .expect("renumbering grouped columns")
                };
                tgt_exprs = tgt_exprs.into_iter().map(renumber).collect();
                having = having.map(renumber);
                order_exprs = order_exprs.into_iter().map(renumber).collect();
                on_exprs = on_exprs.into_iter().map(renumber).collect();
                for w in &mut windows {
                    w.args = std::mem::take(&mut w.args).into_iter().map(renumber).collect();
                    w.partition_by = std::mem::take(&mut w.partition_by).into_iter().map(renumber).collect();
                    for k in &mut w.order_by {
                        k.expr = renumber(k.expr.clone());
                    }
                    if let crate::sql::plan::WinFunc::Agg(a) = &mut w.func {
                        a.args = w.args.clone();
                        a.filter = a.filter.take().map(renumber);
                    }
                    w.frame = tx_frame(std::mem::take(&mut w.frame), &|e| Ok(renumber(e)))?;
                }
                for sr in &mut srfs {
                    sr.args = std::mem::take(&mut sr.args).into_iter().map(renumber).collect();
                }
                for a in &mut st.aggs {
                    a.direct = std::mem::take(&mut a.direct).into_iter().map(renumber).collect();
                }
                groups.extend(found);
            }
            // HAVING without GROUP BY or aggregates is degenerate grouping: one group whatever
            // the input, which isn't read at all (is_degenerate_grouping).
            // (So is GROUP BY () alone, empty grouping sets without keys: one row per set.)
            let empty_sets = groups.is_empty() && sets.as_ref().is_some_and(|v| v.iter().all(|x| x.is_empty()));
            let degenerate = st.aggs.is_empty() && ((!grouped && sets.is_none()) || empty_sets) && !self.deparse;
            if degenerate {
                // (Its FROM was simplified first: a constant that failed to fold there still
                // fails the statement.)
                let kept = if self.folds_failed() { super::fold::candidates(&plan) } else { Vec::new() };
                plan = if kept.is_empty() {
                    Plan::One
                } else {
                    Plan::Project { input: Arc::new(Plan::One), exprs: kept }
                };
            }
            // HAVING clauses that need no aggregation go to WHERE (subquery_planner): moved
            // when there is a GROUP BY (with grouping sets: reading only keys in every set),
            // copied (and kept) when there is none or an empty grouping set.
            if let Some(h) = having.take() {
                let (keep, push) = if self.deparse || matches!(plan, Plan::One) {
                    (Some(h), None)
                } else {
                    // (The keys' nondeterministic collations.)
                    let colls: Vec<Option<String>> = groups
                        .iter()
                        .map(|g| match self.hashed_key(g) {
                            Expr::Collate(_, c, _) => Some(c),
                            _ => None,
                        })
                        .collect();
                    having_to_where(h, &groups, sets.as_deref(), &colls)
                };
                having = keep;
                if let Some(w) = push {
                    plan = optimize::add_filter(&self.cat, plan, w);
                }
            }
            if degenerate {
                // (create_degenerate_grouping_paths: a Result per grouping set, each gated by
                // the HAVING clauses; no aggregation.)
                let one = match having.take() {
                    Some(h) => optimize::add_filter(&self.cat, Plan::One, h),
                    None => Plan::One,
                };
                let n = sets.as_ref().map_or(1, |v| v.len());
                plan = if n > 1 {
                    Plan::Append {
                        inputs: (0..n).map(|_| Arc::new(one.clone())).collect(),
                        inh: None,
                        alias: None,
                        pw: None,
                    }
                } else {
                    one
                };
            } else {
                plan = Plan::Agg {
                    input: Arc::new(plan),
                    groups: groups.iter().map(|g| self.hashed_key(g)).collect(),
                    sets: sets.clone(),
                    aggs: std::mem::take(&mut st.aggs),
                };
            }
            if let Some(h) = having.take() {
                plan = Plan::Filter {
                    input: Arc::new(plan),
                    pred: h,
                };
            }
        }

        // Set-returning functions in a window's PARTITION BY / ORDER BY run below it.
        let srf_in = |e: &Expr| {
            let mut found = false;
            e.walk(&mut |x| found |= matches!(x, Expr::Srf(..)));
            found
        };
        if !srfs.is_empty()
            && windows.iter().any(|w| {
                w.partition_by.iter().any(srf_in) || w.order_by.iter().any(|k| srf_in(&k.expr))
            })
        {
            let (p, pos, _) = srf_stages(plan, std::mem::take(&mut srfs), true)?;
            plan = p;
            let tx = |e: Expr| {
                replace_placeholders(e, &|p| {
                    if let Expr::Srf(j, t) = p {
                        Some(Expr::Col(pos[*j], t.clone()))
                    } else {
                        None
                    }
                })
            };
            tgt_exprs = tgt_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            order_exprs = order_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            on_exprs = on_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            for w in &mut windows {
                w.args = std::mem::take(&mut w.args)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
                w.partition_by = std::mem::take(&mut w.partition_by)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
                for k in &mut w.order_by {
                    k.expr = tx(k.expr.clone())?;
                }
            }
        }

        // Window functions.
        if !windows.is_empty() {
            let nin = width(&plan);
            let tx = |e: Expr| {
                replace_placeholders(e, &|p| {
                    if let Expr::Window(j, t) = p {
                        Some(Expr::Col(nin + j, t.clone()))
                    } else {
                        None
                    }
                })
            };
            tgt_exprs = tgt_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            order_exprs = order_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            on_exprs = on_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            for s in &mut srfs {
                s.args = std::mem::take(&mut s.args)
                    .into_iter()
                    .map(tx)
                    .collect::<Result<_>>()?;
            }
            plan = Plan::Window {
                input: Arc::new(plan),
                calls: windows,
            };
        }

        // Set-returning functions in the select list. (make_sort_input_target: when no
        // ORDER BY or DISTINCT ON key needs a set-returning call's value, the calls run after
        // the sort, over its sorted rows, the select list with them.)
        let has_srf = |e: &Expr| {
            let mut f = false;
            e.walk(&mut |x| f |= matches!(x, Expr::Srf(..)));
            f
        };
        // (An ORDER BY of constants sorts nothing.)
        let late_srfs = (!srfs.is_empty()
            && order_exprs.iter().any(|e| !e.columns().is_empty() || e.is_volatile() || has_srf(e))
            && !distinct_all
            && s.limit_option != pb::LimitOption::WithTies as i32
            && !order_exprs.iter().chain(&on_exprs).any(has_srf))
        .then(|| std::mem::take(&mut srfs));
        let mut srf_base = None;
        if !srfs.is_empty() {
            let (p, pos, last) = srf_stages(plan, srfs, false)?;
            plan = p;
            let tx = |e: Expr| {
                replace_placeholders(e, &|p| {
                    if let Expr::Srf(j, t) = p {
                        Some(Expr::Col(pos[*j], t.clone()))
                    } else {
                        None
                    }
                })
            };
            tgt_exprs = tgt_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            order_exprs = order_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            on_exprs = on_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            srf_base = Some(last);
        }

        // Projection: the select list, then hidden sort / DISTINCT ON columns. (With the
        // set-returning calls after the sort: the input's columns, which the select list
        // reads, then the keys.)
        let mut proj = match &late_srfs {
            Some(_) => (0..optimize::width(&plan))
                .map(|c| Expr::Col(c, DataType::Unknown))
                .collect(),
            None => tgt_exprs.clone(),
        };
        let ntargets = proj.len();
        let key_pos = |e: &Expr, proj: &mut Vec<Expr>| -> usize {
            match proj.iter().position(|p| p.same_as(e)) {
                Some(i) => i,
                None => {
                    proj.push(e.clone());
                    proj.len() - 1
                }
            }
        };
        let order_cols: Vec<usize> = order_exprs.iter().map(|e| key_pos(e, &mut proj)).collect();
        let on_cols: Vec<usize> = on_exprs.iter().map(|e| key_pos(e, &mut proj)).collect();
        let sort_keys: Vec<SortKey> = order
            .iter()
            .zip(&order_cols)
            .map(|(k, c)| SortKey {
                expr: Expr::Col(*c, proj[*c].ty()),
                desc: k.desc,
                nulls_first: k.nulls_first,
                using: k.using.clone(),
            })
            .collect();

        // Keys that are constant for the query (or that the WHERE clause equates to a
        // constant) don't order anything; Postgres drops them.
        let fixed = optimize::fixed_columns(&plan);
        // (A pulled-up subquery's output that is constant for the query, an InitPlan's
        // value, counts as well.)
        fn output_constant(p: &Plan, i: usize) -> bool {
            match p {
                Plan::Project { exprs, input } => match exprs.get(i) {
                    Some(Expr::Col(j, _)) => output_constant(input, *j),
                    Some(e) => optimize::pseudo_constant(e) && !crate::sql::explain::expr_volatile_deep(e),
                    None => false,
                },
                Plan::Filter { input, .. } => output_constant(input, i),
                _ => false,
            }
        }
        let const_out: Vec<usize> = (0..width(&plan)).filter(|&i| output_constant(&plan, i)).collect();
        // (A constant that failed to fold stays a key: its select-list entry is simplified,
        // and fails the statement.)
        let folds_failed = self.folds_failed();
        let ctx = self.ctx;
        let pending = self.fold_err.clone().unwrap_or_default();
        let constant_key = |k: &SortKey| {
            matches!(&k.expr, Expr::Col(c, _)
                if optimize::pseudo_constant(&proj[*c])
                    && !(folds_failed && super::fold::fails(&proj[*c], ctx, &pending))
                    || matches!(&proj[*c], Expr::Col(i, _) if fixed.contains(i) || const_out.contains(i)))
        };
        let needed: Vec<SortKey> = sort_keys
            .iter()
            .filter(|k| !constant_key(k))
            .cloned()
            .collect();
        // Volatile (or, under a LIMIT, expensive) select-list entries the sort doesn't use
        // are computed after it, as make_sort_input_target arranges.
        let postponed = if srf_base.is_none()
            && late_srfs.is_none()
            && !distinct_all
            && on_cols.is_empty()
            && !needed.is_empty()
        {
            postpone_targets(&proj, ntargets, &order_cols, s.limit_count.is_some())
        } else {
            None
        };
        plan = match (srf_base, &postponed) {
            // (split_pathtarget_at_srfs: an expression over a call's value is computed
            // above the ProjectSet, which returns the values and the columns read.)
            (Some(srfs), _)
                if proj.iter().any(|e| {
                    !matches!(e, Expr::Col(..)) && e.columns().iter().any(|&c| c >= width(&plan))
                }) =>
            {
                let mut used: Vec<(usize, DataType)> = Vec::new();
                for e in &proj {
                    e.walk(&mut |x| {
                        if let Expr::Col(c, t) = x
                            && !used.iter().any(|u| u.0 == *c)
                        {
                            used.push((*c, t.clone()));
                        }
                    });
                }
                // (The calls' values first, then the columns the expressions read, each in
                // the order the select list has them: the levels' lists of
                // split_pathtarget_at_srfs.)
                let iw = width(&plan);
                used.sort_by_key(|u| u.0 < iw);
                let below: Vec<Expr> = used.iter().map(|(c, t)| Expr::Col(*c, t.clone())).collect();
                let pos = |c: usize| used.iter().position(|u| u.0 == c).unwrap_or(c);
                Plan::Project {
                    input: Arc::new(Plan::ProjectSet {
                        input: Arc::new(plan),
                        exprs: below,
                        srfs,
                    }),
                    exprs: proj.iter().map(|e| e.clone().remap_cols(&pos)).collect(),
                }
            }
            (Some(srfs), _) => Plan::ProjectSet {
                input: Arc::new(plan),
                exprs: proj.clone(),
                srfs,
            },
            (None, Some((pre, _, _))) => optimize::project(plan, pre.clone()),
            (None, None) => optimize::project(plan, proj.clone()),
        };
        // DISTINCT over columns under a nondeterministic collation groups them as GROUP BY
        // does.
        let nd_keys: Vec<Expr> = if distinct_all {
            proj.iter()
                .enumerate()
                .map(|(i, e)| match self.hashed_key(e) {
                    Expr::Collate(_, c, l) if !matches!(e, Expr::Collate(..)) => {
                        Expr::Collate(Box::new(Expr::Col(i, e.ty())), c, l)
                    }
                    Expr::Call(d, ..) if d.name == super::INDETERMINATE_HASH => {
                        super::expr::unhashable_key(Expr::Col(i, e.ty()))
                    }
                    _ => Expr::Col(i, e.ty()),
                })
                .collect()
        } else {
            Vec::new()
        };
        if distinct_all
            && nd_keys.iter().any(|k| {
                matches!(k, Expr::Collate(..))
                    || matches!(k, Expr::Call(d, ..) if d.name == super::INDETERMINATE_HASH)
            })
        {
            plan = Plan::Agg {
                input: Arc::new(plan),
                groups: nd_keys,
                sets: None,
                aggs: Vec::new(),
            };
            if !needed.is_empty() {
                plan = Plan::Sort {
                    input: Arc::new(plan),
                    keys: needed,
                };
            }
        } else if distinct_all {
            plan = Plan::Distinct {
                input: Arc::new(plan),
                on: None,
                order: 0,
                hashed: false,
            };
            if !needed.is_empty() {
                plan = Plan::Sort {
                    input: Arc::new(plan),
                    keys: needed,
                };
            }
        } else if !on_cols.is_empty() {
            // Sort by the DISTINCT ON keys (then the rest of ORDER BY), keep the first per key.
            let mut keys: Vec<SortKey> = Vec::new();
            for (i, c) in on_cols.iter().enumerate() {
                match sort_keys.get(i) {
                    Some(k)
                        if matches!(&k.expr, Expr::Col(x, _) if x == c)
                            || on_cols.contains(&match &k.expr {
                                Expr::Col(x, _) => *x,
                                _ => usize::MAX,
                            }) =>
                    {
                        keys.push(k.clone())
                    }
                    _ => keys.push(SortKey {
                        expr: Expr::Col(*c, proj[*c].ty()),
                        desc: false,
                        nulls_first: false,
                        using: None,
                    }),
                }
            }
            for k in sort_keys.iter().skip(on_cols.len()) {
                if !constant_key(k) {
                    keys.push(k.clone());
                }
            }
            plan = Plan::Sort {
                input: Arc::new(plan),
                keys,
            };
            // Keys under a nondeterministic collation carry it (see nondeterministic_key).
            let on = on_cols
                .iter()
                .map(|c| {
                    let col = Expr::Col(*c, proj[*c].ty());
                    match self.nondeterministic_key(&proj[*c]) {
                        Expr::Collate(_, n, l) if !matches!(&proj[*c], Expr::Collate(..)) => {
                            Expr::Collate(Box::new(col), n, l)
                        }
                        _ => col,
                    }
                })
                .collect();
            plan = Plan::Distinct {
                input: Arc::new(plan),
                on: Some(on),
                order: sort_keys.iter().filter(|k| !constant_key(k)).count(),
                hashed: false,
            };
        } else if let Some((_, post, map)) = postponed {
            let keys = needed
                .into_iter()
                .map(|mut k| {
                    k.expr = k.expr.remap_cols(&|c| map[c]);
                    k
                })
                .collect();
            plan = optimize::sort(&self.cat, plan, keys);
            plan = Plan::Project {
                input: Arc::new(plan),
                exprs: post,
            };
        } else if !needed.is_empty() {
            plan = optimize::sort(&self.cat, plan, needed);
        }
        if let Some(srfs) = late_srfs {
            let (p, pos, last) = srf_stages(plan, srfs, false)?;
            let tx = |e: Expr| {
                replace_placeholders(e, &|p| {
                    if let Expr::Srf(j, t) = p {
                        Some(Expr::Col(pos[*j], t.clone()))
                    } else {
                        None
                    }
                })
            };
            tgt_exprs = tgt_exprs.into_iter().map(tx).collect::<Result<_>>()?;
            // (The sort columns the select list doesn't return ride along as resjunk
            // entries.)
            let mut exprs = tgt_exprs.clone();
            if !self.deparse {
                for &c in &order_cols {
                    if c < proj.len()
                        && !exprs.iter().any(|e| matches!(e, Expr::Col(j, _) if *j == c))
                        && matches!(proj[c], Expr::Col(..))
                    {
                        exprs.push(Expr::Col(c, proj[c].ty()));
                    }
                }
            }
            let junk = exprs.len() > tgt_exprs.len();
            plan = Plan::ProjectSet {
                input: Arc::new(p),
                exprs,
                srfs: last,
            };
            plan = self.apply_limit(plan, s, None)?;
            if junk {
                plan = Plan::Project {
                    input: Arc::new(plan),
                    exprs: tgt_exprs.iter().enumerate().map(|(i, e)| Expr::Col(i, e.ty())).collect(),
                };
            }
        } else {
            plan = self.apply_limit(plan, s, Some(sort_keys))?;
            if proj.len() > ntargets {
                let exprs = (0..ntargets).map(|i| Expr::Col(i, proj[i].ty())).collect();
                plan = Plan::Project {
                    input: Arc::new(plan),
                    exprs,
                };
            }
        }
        let cols = targets
            .iter()
            .zip(&tgt_exprs)
            .zip(tgt_colls)
            .map(|((t, e), collation)| {
                let ty = match e.ty() {
                    DataType::Unknown if !keep_unknown => DataType::Text,
                    t => t,
                };
                let domain = self.expr_domain(e, t.table, t.attnum).or(t.domain);
                OutCol {
                    name: t.name.clone(),
                    ty,
                    table: t.table,
                    attnum: t.attnum,
                    domain_array: self.domain_array(domain),
                    domain,
                    collation,
                }
            })
            .collect::<Vec<_>>();
        // Unknown-typed literals in the select list come out as text. (A ProjectSet's
        // own entries are cast in place: its target list is the select list.)
        let mut plan = plan;
        if !keep_unknown
            && let Plan::ProjectSet { exprs, .. } = &mut plan
            && exprs.len() >= tgt_exprs.len()
            && tgt_exprs
                .iter()
                .zip(exprs.iter())
                .all(|(t, e)| !matches!(t.ty(), DataType::Unknown) || matches!(e.ty(), DataType::Unknown))
        {
            for (i, t) in tgt_exprs.iter().enumerate() {
                if matches!(t.ty(), DataType::Unknown) {
                    exprs[i] = match exprs[i].clone() {
                        Expr::Const(v @ (Value::Text(_) | Value::Null), _) => Expr::Const(v, DataType::Text),
                        e => Expr::Cast(Box::new(e), DataType::Unknown, DataType::Text, false),
                    };
                }
            }
            for t in tgt_exprs.iter_mut() {
                if matches!(t.ty(), DataType::Unknown) {
                    *t = Expr::Cast(Box::new(t.clone()), DataType::Unknown, DataType::Text, false);
                }
            }
        }
        let plan = if !keep_unknown
            && tgt_exprs
                .iter()
                .any(|e| matches!(e.ty(), DataType::Unknown))
        {
            let exprs = tgt_exprs
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let c = Expr::Col(i, e.ty());
                    if matches!(e.ty(), DataType::Unknown) {
                        Expr::Cast(Box::new(c), DataType::Unknown, DataType::Text, false)
                    } else {
                        c
                    }
                })
                .collect();
            Plan::Project {
                input: Arc::new(plan),
                exprs,
            }
        } else {
            plan
        };
        // Access paths by cost, now that the level's quals, order and limit are known.
        // (A FROM subquery planned apart: by the query above, `Planner::walk`.)
        let mut plan = plan;
        if self.defer_level == Some(self.levels.len()) && !self.deparse {
            self.defer_level = None;
            self.deferred = true;
            return Ok((plan, cols));
        }
        if !self.deparse {
            let lim = self
                .arm_limit
                .filter(|a| a.0 == self.levels.len())
                .map(|a| (a.1, a.2));
            let pushed = s.locking_clause.is_empty()
                && self.pushed_lock.as_ref().is_some_and(|(l, _)| *l == self.levels.len());
            let _pushed = crate::scoped::set(&crate::sql::paths::PUSHED_LEVEL, pushed, "rowmarks::PUSHED_LEVEL");
            let own = !s.locking_clause.is_empty();
            if (own || pushed)
                && !self.unplanned
                && let Some(m) = crate::sql::paths::nullable_side_lock(self.ctx, &plan)
            {
                let what = match m {
                    RowMark::KeyShare => "FOR KEY SHARE",
                    RowMark::Share => "FOR SHARE",
                    RowMark::NoKeyUpdate => "FOR NO KEY UPDATE",
                    RowMark::Update => "FOR UPDATE",
                };
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{what} cannot be applied to the nullable side of an outer join"),
                ));
            }
            let _own = crate::scoped::set(&crate::sql::paths::OWN_LOCK, own, "rowmarks::OWN_LOCK");
            self.hidden_rtes += crate::sql::paths::plan_query_limited(self.ctx, &mut plan, lim);
            drop(_own);
            drop(_pushed);
            if !self.unplanned
                && let Err(e) = super::check_merge_keys(self.ctx, &mut plan)
            {
                self.plan_error(e)?;
            }
        }
        Ok((plan, cols))
    }

    /// ORDER BY / DISTINCT ON item: an output column name or position, or an expression.
    fn resolve_output_ref(&mut self, n: &Node, targets: &[Target], what: &str) -> Result<Expr> {
        non_integer_constant(n, what)?;
        if let Some(NodeEnum::AConst(c)) = inner(n)
            && let Some(pb::a_const::Val::Ival(i)) = &c.val
        {
            let p = i.ival;
            if p < 1 || p as usize > targets.len() {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!("{what} position {p} is not in select list"),
                    ),
                    c.location,
                ));
            }
            return Ok(targets[p as usize - 1].expr.clone());
        }
        if let Some(NodeEnum::ColumnRef(c)) = inner(n)
            && c.fields.len() == 1
            && let Some(name) = ast::sval(&c.fields[0])
        {
            let matches: Vec<&Target> = targets.iter().filter(|t| t.name == name).collect();
            if !matches.is_empty() {
                if matches.len() > 1 && !matches.iter().all(|m| m.expr.same_as(&matches[0].expr)) {
                    return Err(with_pos(
                        Error::new(
                            code::AMBIGUOUS_COLUMN,
                            format!("{what} \"{name}\" is ambiguous"),
                        ),
                        c.location,
                    ));
                }
                return Ok(matches[0].expr.clone());
            }
        }
        self.bind_expr(n)
    }

    // ── select list ──

    pub(crate) fn bind_targets(&mut self, list: &[Node]) -> Result<Vec<Target>> {
        let mut out = Vec::new();
        for t in list {
            let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                continue;
            };
            let val = rt.val.as_deref().unwrap();
            // `*` and `t.*`
            if let Some(NodeEnum::ColumnRef(c)) = inner(val)
                && matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
            {
                let quals = names(&c.fields);
                out.extend(self.expand_star(&quals, c.location)?);
                continue;
            }
            // `(expr).*`
            if let Some(NodeEnum::AIndirection(ind)) = inner(val)
                && matches!(
                    ind.indirection.last().and_then(inner),
                    Some(NodeEnum::AStar(_))
                )
            {
                let e = self.bind_expr(val)?;
                match e.ty() {
                    DataType::Row(fields) => {
                        for (i, (n, ty)) in fields.iter().enumerate() {
                            let fe = match &e {
                                Expr::Row(items, _) => items[i].clone(),
                                _ => Expr::Field(Box::new(e.clone()), i, ty.clone()),
                            };
                            out.push(Target {
                                name: n.clone(),
                                expr: fe,
                                table: 0,
                                attnum: 0,
                                domain: None,
                            });
                        }
                    }
                    t => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("type {} is not composite", t.display()),
                        ));
                    }
                }
                continue;
            }
            let expr = self.bind_expr(val)?;
            let name = if rt.name.is_empty() {
                figure_colname(val)
            } else {
                rt.name.clone()
            };
            let (table, attnum) = self.column_origin(&expr);
            let domain = self.declared_domain(&expr);
            out.push(Target {
                name,
                expr,
                table,
                attnum,
                domain,
            });
        }
        Ok(out)
    }

    /// One item of a VALUES list (transformExpressionList): `x.*` and `(expr).*` stand for
    /// their columns, a relation's or a composite value's (a PL/pgSQL row variable's).
    pub(crate) fn bind_values_item(&mut self, item: &Node) -> Result<Vec<Expr>> {
        let row_fields = |e: Expr| -> Result<Vec<Expr>> {
            match e.ty() {
                DataType::Row(fields) => Ok(fields
                    .iter()
                    .enumerate()
                    .map(|(i, (_, ty))| match &e {
                        Expr::Row(items, _) => items[i].clone(),
                        _ => Expr::Field(Box::new(e.clone()), i, ty.clone()),
                    })
                    .collect()),
                t => Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("type {} is not composite", t.display()),
                )),
            }
        };
        match inner(item) {
            Some(NodeEnum::ColumnRef(c))
                if c.fields.len() > 1
                    && matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_))) =>
            {
                let quals = names(&c.fields);
                match self.expand_star(&quals, c.location) {
                    Ok(ts) => Ok(ts.into_iter().map(|t| t.expr).collect()),
                    Err(e) => {
                        // Not a relation: a variable's row.
                        let row = ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
                            fields: c.fields[..c.fields.len() - 1].to_vec(),
                            location: c.location,
                        }));
                        match self.bind_expr(&row) {
                            Ok(r) if matches!(r.ty(), DataType::Row(_)) => row_fields(r),
                            _ => Err(e),
                        }
                    }
                }
            }
            Some(NodeEnum::AIndirection(ind))
                if matches!(ind.indirection.last().and_then(inner), Some(NodeEnum::AStar(_))) =>
            {
                let e = self.bind_expr(item)?;
                row_fields(e)
            }
            _ => Ok(vec![self.bind_expr(item)?]),
        }
    }

    fn column_origin(&self, e: &Expr) -> (u32, i16) {
        if let Expr::Col(..) = e {
            for r in &self.scope().rels {
                for c in &r.cols {
                    if c.expr.same_as(e) && c.table != 0 {
                        return (c.table, c.attnum);
                    }
                }
            }
        }
        (0, 0)
    }

    fn expand_star(&mut self, quals: &[String], loc: i32) -> Result<Vec<Target>> {
        let mut out = Vec::new();
        if quals.is_empty() {
            if self.scope().rels.is_empty() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "SELECT * with no tables specified is not valid",
                    ),
                    loc,
                ));
            }
            let mut rowtype_cols = Vec::new();
            for r in (self.scope().rels.iter())
                .filter(|r| r.exists_col.is_none() && !r.qualified_only)
            {
                for c in r.cols.iter().filter(|c| !c.hidden && !c.merged) {
                    rowtype_cols.push(c.rowtype_col);
                    out.push(Target {
                        name: c.name.clone(),
                        expr: c.expr.clone(),
                        table: c.table,
                        attnum: c.attnum,
                        domain: None,
                    });
                }
            }
            if self.refs.is_some() && loc >= 0 {
                let items = star_items(&self.scope().rels, None);
                if let Some(log) = self.ref_log() {
                    log.stars.push((loc, items));
                }
            }
            for t in &mut out {
                self.note_col(t.table, t.attnum);
                t.domain = self.declared_domain(&t.expr);
            }
            for c in rowtype_cols {
                self.note_rowtype_col(c);
            }
            return Ok(out);
        }
        let q: Vec<&str> = quals.iter().map(String::as_str).collect();
        let q = if q.len() == 3 { &q[1..] } else { &q[..] };
        // Search this level, then enclosing ones.
        for lvl in (0..self.levels.len()).rev() {
            let Some(r) = self.levels[lvl].scope.rels.iter().find(|r| {
                !r.anonymous
                    && match q {
                        [rel] => r.alias == *rel,
                        [s, rel] => r.alias == *rel && r.schema.as_deref() == Some(*s),
                        _ => false,
                    }
            }) else {
                continue;
            };
            let cols: Vec<ScopeCol> = r.cols.iter().filter(|c| !c.hidden).cloned().collect();
            let items = (self.refs.is_some() && loc >= 0)
                .then(|| star_items(std::slice::from_ref(r), Some(quals)));
            let row_ref = super::refs::RowRef {
                loc,
                rel: r.table.as_ref().map_or(0, |t| t.oid),
                src: r.src,
                quals: q.len(),
                refname: r.alias.clone(),
            };
            if loc >= 0
                && let Some(log) = self.ref_log()
            {
                log.rows.push(row_ref);
                if let Some(items) = items {
                    log.stars.push((loc, items));
                }
            }
            for c in cols {
                let e = self.lift(lvl, c.expr.clone());
                self.note_col(c.table, c.attnum);
                self.note_rowtype_col(c.rowtype_col);
                let domain = self.declared_domain(&e);
                out.push(Target {
                    name: c.name.clone(),
                    expr: e,
                    table: c.table,
                    attnum: c.attnum,
                    domain,
                });
            }
            return Ok(out);
        }
        // A composite PL/pgSQL variable or function argument: `new.*`.
        if let [var] = q {
            for lvl in (0..self.levels.len()).rev() {
                let Some(args) = &self.levels[lvl].fn_args else {
                    continue;
                };
                let Some(i) = self.levels[lvl].fn_arg_index(var) else {
                    continue;
                };
                let ty = args[i].1.clone();
                let DataType::Row(fields) = &ty else { break };
                for (f, (name, fty)) in fields.iter().enumerate() {
                    let e = self.lift(
                        lvl,
                        Expr::Field(Box::new(Expr::Outer(1, i, ty.clone())), f, fty.clone()),
                    );
                    out.push(Target {
                        name: name.clone(),
                        expr: e,
                        table: 0,
                        attnum: 0,
                        domain: None,
                    });
                }
                return Ok(out);
            }
        }
        // errorMissingRTE: what the name might have meant.
        Err(self.missing_rel(q.last().unwrap(), loc))
    }

    /// Postgres transforms ORDER BY before GROUP BY: an ORDER BY item that isn't an output
    /// column and names an ambiguous column fails first.
    fn order_by_ambiguity(&mut self, s: &pb::SelectStmt) -> Option<Error> {
        let names: Vec<String> = s
            .target_list
            .iter()
            .filter_map(|t| match inner(t) {
                Some(NodeEnum::ResTarget(r)) if !r.name.is_empty() => Some(r.name.clone()),
                Some(NodeEnum::ResTarget(r)) => match r.val.as_deref().and_then(inner) {
                    Some(NodeEnum::ColumnRef(c)) => c.fields.last().and_then(ast::sval).map(str::to_string),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for sb in &s.sort_clause {
            let Some(NodeEnum::SortBy(sb)) = inner(sb) else {
                continue;
            };
            let node = sb.node.as_deref()?;
            match inner(node) {
                Some(NodeEnum::AConst(_)) => continue,
                Some(NodeEnum::ColumnRef(c))
                    if c.fields.len() == 1
                        && ast::sval(&c.fields[0]).is_some_and(|n| names.iter().any(|x| x == n)) =>
                {
                    continue;
                }
                _ => {}
            }
            if let Err(e) = self.in_clause(Clause::OrderBy, |b| b.bind_expr(node))
                && e.code == code::AMBIGUOUS_COLUMN
            {
                return Some(e);
            }
        }
        None
    }

    // ── GROUP BY ──

    /// Group expressions (unique) and, for grouping sets, the sets as indexes.
    fn bind_group_by(
        &mut self,
        s: &pb::SelectStmt,
    ) -> Result<(Vec<Expr>, Option<Vec<Vec<usize>>>)> {
        let mut groups: Vec<Expr> = Vec::new();
        // Each GROUP BY item becomes a list of alternative sets; the result is their product.
        let mut items: Vec<Vec<Vec<usize>>> = Vec::new();
        let mut any_sets = false;
        for g in &s.group_clause {
            match inner(g) {
                Some(NodeEnum::GroupingSet(gs)) => {
                    any_sets = true;
                    items.push(self.grouping_set(gs, &mut groups, &s.target_list)?);
                }
                _ => {
                    let i = self.group_item(g, &mut groups, &s.target_list)?;
                    items.push(vec![i]);
                }
            }
        }
        if !any_sets {
            return Ok((groups, None));
        }
        let mut sets: Vec<Vec<usize>> = vec![vec![]];
        for alts in items {
            let mut next = Vec::new();
            for s in &sets {
                for a in &alts {
                    let mut x = s.clone();
                    for i in a {
                        if !x.contains(i) {
                            x.push(*i);
                        }
                    }
                    next.push(x);
                }
            }
            sets = next;
        }
        if s.group_distinct {
            let mut seen: Vec<Vec<usize>> = Vec::new();
            sets.retain(|x| {
                let mut k = x.clone();
                k.sort();
                if seen.contains(&k) {
                    false
                } else {
                    seen.push(k);
                    true
                }
            });
        }
        Ok((groups, Some(sets)))
    }

    /// One GROUP BY expression (possibly a row of several): indexes into `groups`.
    fn group_item(
        &mut self,
        g: &Node,
        groups: &mut Vec<Expr>,
        targets: &[Node],
    ) -> Result<Vec<usize>> {
        if let Some(NodeEnum::RowExpr(r)) = inner(g) {
            let mut out = Vec::new();
            for a in &r.args {
                out.extend(self.group_item(a, groups, targets)?);
            }
            return Ok(out);
        }
        let e = self.group_expr(g, targets)?;
        require_equality(&e.ty(), ast::location(g))?;
        let mut tgt = self.group_target(g, targets);
        // findTargetlistEntrySQL99: a key without columns is the first select-list item
        // written as it is (compared as written: folded, other expressions would look equal).
        let star = targets.iter().any(|t| match inner(t) {
            Some(NodeEnum::ResTarget(rt)) => matches!(
                rt.val.as_deref().and_then(inner),
                Some(NodeEnum::ColumnRef(_) | NodeEnum::AIndirection(_))
            ) && format!("{:?}", rt.val).contains("AStar"),
            _ => true,
        });
        if tgt.is_none() && !star && e.columns().is_empty() && !e.is_volatile() {
            let text = |n: &Node| strip_locations(&format!("{n:?}"));
            let want = text(g);
            tgt = targets.iter().position(|t| match inner(t) {
                Some(NodeEnum::ResTarget(rt)) => rt.val.as_deref().is_some_and(|v| text(v) == want),
                _ => false,
            });
        }
        // (Written as a constant: the select-list item it names, else the item itself.)
        let written = match tgt.and_then(|t| targets.get(t)).and_then(inner) {
            Some(NodeEnum::ResTarget(rt)) => rt.val.as_deref(),
            _ => Some(g),
        };
        let literal = written.is_some_and(literal_node);
        // (A literal's text too: its occurrences inside other items are kept from folding
        // as an expression's are, `SelectState::pass_marks`.)
        let text = written.map(|w| strip_locations(&format!("{w:?}")));
        let st = &mut self.level().st;
        st.group_targets.resize(groups.len(), None);
        st.group_literal.resize(groups.len(), false);
        st.group_text.resize(groups.len(), None);
        // An item named by alias or position is its own key; an expression is that of an
        // equal key.
        let found = match tgt {
            Some(t) => st.group_targets.iter().position(|x| *x == Some(t)).or_else(|| {
                groups
                    .iter()
                    .zip(&st.group_targets)
                    .position(|(x, gt)| gt.is_none() && x.same_as(&e))
            }),
            None => groups.iter().position(|x| x.same_as(&e)),
        };
        let i = match found {
            Some(i) => {
                if tgt.is_some() && st.group_targets[i].is_none() {
                    st.group_targets[i] = tgt;
                }
                i
            }
            None => {
                groups.push(e);
                st.group_targets.push(tgt);
                st.group_literal.push(literal);
                st.group_text.push(text);
                groups.len() - 1
            }
        };
        Ok(vec![i])
    }

    /// The select-list item a GROUP BY item names by position or output alias (not an input
    /// column), when the list has no `*`.
    fn group_target(&mut self, g: &Node, targets: &[Node]) -> Option<usize> {
        let star = targets.iter().any(|t| match inner(t) {
            Some(NodeEnum::ResTarget(rt)) => match rt.val.as_deref().and_then(inner) {
                Some(NodeEnum::ColumnRef(c)) => {
                    matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
                }
                Some(NodeEnum::AIndirection(_)) => true,
                _ => false,
            },
            _ => true,
        });
        if star {
            return None;
        }
        match inner(g) {
            Some(NodeEnum::AConst(c)) => match &c.val {
                Some(pb::a_const::Val::Ival(i)) if i.ival >= 1 => {
                    let p = i.ival as usize - 1;
                    (p < targets.len()).then_some(p)
                }
                _ => None,
            },
            Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => {
                let name = ast::sval(&c.fields[0])?;
                // (An input column of the name wins.)
                if self
                    .in_clause(Clause::GroupBy, |b| b.find_column(None, name, c.location))
                    .ok()
                    .flatten()
                    .is_some()
                {
                    return None;
                }
                targets.iter().position(|t| {
                    matches!(inner(t), Some(NodeEnum::ResTarget(rt)) if rt.name == name)
                })
            }
            _ => None,
        }
    }

    fn group_expr(&mut self, g: &Node, targets: &[Node]) -> Result<Expr> {
        non_integer_constant(g, "GROUP BY")?;
        // GROUP BY 2: the second select-list item.
        if let Some(NodeEnum::AConst(c)) = inner(g)
            && let Some(pb::a_const::Val::Ival(i)) = &c.val
        {
            let p = i.ival;
            let missing = || {
                with_pos(
                    Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!("GROUP BY position {p} is not in select list"),
                    ),
                    c.location,
                )
            };
            // Positions count the columns `*` and `(expr).*` expand to.
            let star = |t: &Node| match inner(t) {
                Some(NodeEnum::ResTarget(rt)) => match rt.val.as_deref().and_then(inner) {
                    Some(NodeEnum::ColumnRef(c)) => {
                        matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
                    }
                    Some(NodeEnum::AIndirection(ind)) => matches!(
                        ind.indirection.last().and_then(inner),
                        Some(NodeEnum::AStar(_))
                    ),
                    _ => false,
                },
                _ => false,
            };
            if p >= 1 && targets.iter().any(star) {
                let mut k = 0;
                for t in targets {
                    let cols = self
                        .in_clause(Clause::GroupBy, |b| b.bind_targets(std::slice::from_ref(t)))?;
                    if (p as usize) <= k + cols.len() {
                        return Ok(cols[p as usize - 1 - k].expr.clone());
                    }
                    k += cols.len();
                }
                return Err(missing());
            }
            let Some(t) = targets.get((p - 1).max(0) as usize).filter(|_| p >= 1) else {
                return Err(missing());
            };
            let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                return Err(crate::error::internal("target"));
            };
            return self.in_clause(Clause::GroupBy, |b| b.bind_expr(rt.val.as_deref().unwrap()));
        }
        // A bare name: an input column wins over an output alias.
        if let Some(NodeEnum::ColumnRef(c)) = inner(g)
            && c.fields.len() == 1
            && let Some(name) = ast::sval(&c.fields[0])
        {
            let input =
                self.in_clause(Clause::GroupBy, |b| b.find_column(None, name, c.location))?;
            if let Some(e) = input {
                return Ok(e);
            }
            for t in targets {
                if let Some(NodeEnum::ResTarget(rt)) = inner(t)
                    && rt.name == name
                {
                    return self
                        .in_clause(Clause::GroupBy, |b| b.bind_expr(rt.val.as_deref().unwrap()));
                }
            }
        }
        self.in_clause(Clause::GroupBy, |b| b.bind_expr(g))
    }

    fn grouping_set(
        &mut self,
        gs: &pb::GroupingSet,
        groups: &mut Vec<Expr>,
        targets: &[Node],
    ) -> Result<Vec<Vec<usize>>> {
        use pb::GroupingSetKind as K;
        let kind = K::try_from(gs.kind).unwrap_or(K::GroupingSetSimple);
        let elems = |b: &mut Self, groups: &mut Vec<Expr>| -> Result<Vec<Vec<usize>>> {
            gs.content
                .iter()
                .map(|c| b.group_item(c, groups, targets))
                .collect()
        };
        Ok(match kind {
            K::GroupingSetEmpty => vec![vec![]],
            K::GroupingSetSimple => vec![elems(self, groups)?.concat()],
            K::GroupingSetRollup => {
                let e = elems(self, groups)?;
                let mut out = Vec::new();
                for n in (0..=e.len()).rev() {
                    out.push(e[..n].concat());
                }
                out
            }
            K::GroupingSetCube => {
                let e = elems(self, groups)?;
                if e.len() > 12 {
                    return Err(Error::new(
                        code::PROGRAM_LIMIT_EXCEEDED,
                        "CUBE is limited to 12 elements",
                    ));
                }
                let mut out = Vec::new();
                let n = e.len();
                // expand_groupingset_node: set i has element j when bit j of i is set.
                for m in 0..(1u32 << n) {
                    let mut set = Vec::new();
                    for (i, x) in e.iter().enumerate() {
                        if m & (1 << i) != 0 {
                            set.extend(x.iter().copied());
                        }
                    }
                    out.push(set);
                }
                out
            }
            K::GroupingSetSets => {
                let mut out = Vec::new();
                for c in &gs.content {
                    match inner(c) {
                        Some(NodeEnum::GroupingSet(inner_gs)) => {
                            out.extend(self.grouping_set(inner_gs, groups, targets)?)
                        }
                        _ => out.push(self.group_item(c, groups, targets)?),
                    }
                }
                out
            }
            _ => vec![vec![]],
        })
    }

    // ── FROM ──

    fn locking_spec(&mut self, clauses: &[Node]) -> Result<Option<Locking>> {
        let mut out = Locking { clauses: Vec::new() };
        for c in clauses {
            let Some(NodeEnum::LockingClause(lc)) = inner(c) else {
                continue;
            };
            let mark = match pb::LockClauseStrength::try_from(lc.strength)
                .unwrap_or(pb::LockClauseStrength::LcsForupdate)
            {
                pb::LockClauseStrength::LcsForkeyshare => RowMark::KeyShare,
                pb::LockClauseStrength::LcsForshare => RowMark::Share,
                pb::LockClauseStrength::LcsFornokeyupdate => RowMark::NoKeyUpdate,
                _ => RowMark::Update,
            };
            let wait = match pb::LockWaitPolicy::try_from(lc.wait_policy)
                .unwrap_or(pb::LockWaitPolicy::LockWaitBlock)
            {
                pb::LockWaitPolicy::LockWaitSkip => Wait::SkipLocked,
                pb::LockWaitPolicy::LockWaitError => Wait::NoWait,
                _ => Wait::Block,
            };
            let (rels, locs): (Vec<String>, Vec<i32>) = lc
                .locked_rels
                .iter()
                .filter_map(|r| match inner(r) {
                    Some(NodeEnum::RangeVar(rv)) => Some((rv.relname.clone(), rv.location)),
                    _ => None,
                })
                .unzip();
            out.clauses.push(LockClause {
                rels,
                locs,
                mark,
                wait,
            });
        }
        // (A lock pushed down from the query above: every relation of this one, after the
        // level's own clauses (transformLockingClause of the level above runs after this
        // level's); the stronger strength where it has its own clause too.)
        if let Some((level, p)) = &self.pushed_lock
            && *level == self.levels.len()
        {
            out.clauses.extend(p.clauses.iter().cloned());
        }
        Ok((!out.clauses.is_empty()).then_some(out))
    }

    /// The FOR UPDATE / SHARE of the query above pushed into a FROM subquery or a view's
    /// query named `name` (transformLockingClause): with no relation list, or naming it.
    pub(crate) fn push_lock(&self, locking: Option<&Locking>, name: &str) -> Option<(usize, Locking)> {
        let (mark, wait, _) = locking?.mark_for(name)?;
        Some((
            self.levels.len() + 1,
            Locking {
                clauses: vec![LockClause {
                    rels: Vec::new(),
                    locs: Vec::new(),
                    mark,
                    wait,
                }],
            },
        ))
    }

    /// Bind the FROM list: sets the level's scope and returns the plan.
    /// deconstruct_recurse's from_collapse_limit test for a subquery pulled up into the FROM
    /// list being bound: its FROM items join the list's (one join problem) when the list's
    /// other items and its own number at most from_collapse_limit, else they are a sub-list
    /// planned as a join relation of its own first, as the engine plans a LATERAL subquery
    /// that isn't pulled up.
    fn lateral_collapses(&self, sub: &Node) -> bool {
        let n = match inner(sub) {
            Some(NodeEnum::SelectStmt(s)) => s.from_clause.len(),
            _ => 1,
        };
        let parent = self.from_lens.last().copied().unwrap_or(1);
        let limit = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("from_collapse_limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8);
        n <= 1 || parent.saturating_sub(1) + n <= limit
    }

    pub(crate) fn bind_from(&mut self, items: &[Node], locking: Option<&Locking>) -> Result<Plan> {
        self.from_lens.push(items.len());
        let r = self.bind_from_list(items, locking);
        self.from_lens.pop();
        r
    }

    fn bind_from_list(&mut self, items: &[Node], locking: Option<&Locking>) -> Result<Plan> {
        self.level().scope = Scope::default();
        if items.is_empty() {
            return Ok(Plan::One);
        }
        let mut plan: Option<Plan> = None;
        for item in items {
            let (p, scope, params) = self.bind_from_item(item, locking)?;
            plan = Some(match plan {
                // The first item's outer references read this level's outer row as it is,
                // when they map to it one for one; else (a PL/pgSQL variable, say) they are
                // computed for it, as a later item's are.
                None if !params.is_empty()
                    && !params
                        .iter()
                        .enumerate()
                        .all(|(j, e)| matches!(e, Expr::Outer(1, k, _) if *k == j)) =>
                {
                    self.level().scope = scope;
                    let nright = width(&p);
                    let held = !params.is_empty() && crate::sql::plan::deferred_lateral(&p);
                    Plan::Join {
                        kind: JoinKind::Inner,
                        left: Arc::new(Plan::One),
                        right: Arc::new(p),
                        cond: None,
                        hash: None,
                        params,
                        nleft: 0,
                        nright,
                        meta: Box::new(crate::sql::plan::JoinMeta {
                            lateral_held: held,
                            ..Default::default()
                        }),
                    }
                }
                None => {
                    self.level().scope = scope;
                    p
                }
                Some(left) => {
                    let nleft = width(&left);
                    let nright = width(&p);
                    if !self.deparse && !params.is_empty() {
                        let src = row_sources(&left);
                        self.note_lateral(&params, &src);
                    }
                    self.scope().check_disjoint(&scope)?;
                    self.level().scope.extend(scope.shifted(nleft));
                    // (A join item reading the items to its left: reassociated. Not a
                    // subquery's join: its own level reads its parameters directly.)
                    let joined = if matches!(p, Plan::Join { .. })
                        && matches!(inner(item), Some(NodeEnum::JoinExpr(_)))
                        && !params.is_empty()
                        && !self.deparse
                    {
                        reassociate_lateral(left, p, params)
                    } else {
                        Err(Box::new((left, p, params)))
                    };
                    match joined {
                        Ok(j) => j,
                        Err(b) => {
                            let (left, p, params) = *b;
                            let held = !params.is_empty() && crate::sql::plan::deferred_lateral(&p);
                            Plan::Join {
                                kind: JoinKind::Inner,
                                left: Arc::new(left),
                                right: Arc::new(p),
                                cond: None,
                                hash: None,
                                params,
                                nleft,
                                nright,
                                meta: Box::new(crate::sql::plan::JoinMeta {
                                    lateral_held: held,
                                    ..Default::default()
                                }),
                            }
                        }
                    }
                }
            });
        }
        let plan = plan.unwrap();
        for l in locking.iter().flat_map(|l| &l.clauses) {
            for (i, r) in l.rels.iter().enumerate() {
                // (A join's alias names no table to lock.)
                if self
                    .scope()
                    .rels
                    .iter()
                    .any(|x| &x.alias == r && x.join_rte && !x.qualified_only)
                {
                    let what = match l.mark {
                        RowMark::KeyShare => "FOR KEY SHARE",
                        RowMark::Share => "FOR SHARE",
                        RowMark::NoKeyUpdate => "FOR NO KEY UPDATE",
                        _ => "FOR UPDATE",
                    };
                    return Err(with_pos(
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("{what} cannot be applied to a join"),
                        ),
                        l.locs.get(i).copied().unwrap_or(-1),
                    ));
                }
                if !self
                    .scope()
                    .rels
                    .iter()
                    .any(|x| &x.alias == r && !x.qualified_only)
                {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_TABLE,
                            format!(
                                "relation \"{r}\" in FOR UPDATE clause not found in FROM clause"
                            ),
                        ),
                        l.locs.get(i).copied().unwrap_or(-1),
                    ));
                }
            }
        }
        Ok(plan)
    }

    /// One FROM item: its plan, its scope (columns from position 0), and the params it reads
    /// from items to its left (LATERAL).
    pub(crate) fn bind_from_item(
        &mut self,
        item: &Node,
        locking: Option<&Locking>,
    ) -> Result<(Plan, Scope, Vec<Expr>)> {
        match inner(item) {
            Some(NodeEnum::RangeVar(rv))
                if rv.relname == super::rules::RULE_RTE
                    && rv.schemaname.is_empty()
                    && self.rule_src.is_some() =>
            {
                let (p, scope) = self.rule_from_item().unwrap();
                Ok((p, scope, vec![]))
            }
            Some(NodeEnum::RangeVar(rv)) => {
                let (p, rel) = self.bind_range_var(rv, locking)?;
                Ok((p, Scope { rels: vec![rel] }, vec![]))
            }
            Some(NodeEnum::RangeSubselect(rs)) => {
                self.hidden_rtes += 1;
                // Not LATERAL: the FROM items to its left are out of its sight (in the
                // range table still, for errors).
                let hidden = (!rs.lateral).then(|| {
                    let scope = std::mem::take(&mut self.level().scope);
                    let n = self.unreachable.len();
                    self.unreachable.extend(scope.rels.iter().cloned().map(|mut r| {
                        if r.lateral != super::LAT_TARGET {
                            r.lateral = super::LAT_SIBLING;
                        }
                        r
                    }));
                    (scope, n)
                });
                let clause = std::mem::replace(&mut self.level().clause, Clause::FromSubselect);
                // (A plain SELECT's level: planned by the query above when it becomes a
                // Subquery Scan's.)
                let simple_select = matches!(inner(rs.subquery.as_deref().unwrap()), Some(NodeEnum::SelectStmt(s))
                    if matches!(
                        pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone),
                        pb::SetOperation::SetopNone | pb::SetOperation::Undefined
                    ) && s.values_lists.is_empty());
                let saved_defer = (self.defer_level.take(), std::mem::take(&mut self.deferred));
                // (A LATERAL one without an alias too: it may be pulled up, below.)
                if simple_select && (rs.alias.is_some() || rs.lateral) && !self.deparse && !self.unplanned && self.deps.is_none() && self.refs.is_none() {
                    self.defer_level = Some(self.levels.len() + 1);
                }
                let sub_level = self.levels.len() + 1;
                // FOR UPDATE / SHARE of this level reaching the subquery locks its relations
                // too (a VALUES list's none; a set operation's can't be).
                let alias_name = rs.alias.as_ref().map(|a| a.aliasname.as_str()).unwrap_or("");
                let mut push = if self.deparse { None } else { self.push_lock(locking, alias_name) };
                if let Some(NodeEnum::SelectStmt(sub)) = inner(rs.subquery.as_deref().unwrap())
                    && push.is_some()
                {
                    if !sub.values_lists.is_empty() {
                        push = None;
                    } else if !simple_select {
                        let what = match push.as_ref().map(|p| p.1.first_mark()) {
                            Some(RowMark::KeyShare) => "FOR KEY SHARE",
                            Some(RowMark::Share) => "FOR SHARE",
                            Some(RowMark::NoKeyUpdate) => "FOR NO KEY UPDATE",
                            _ => "FOR UPDATE",
                        };
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("{what} is not allowed with UNION/INTERSECT/EXCEPT"),
                        ));
                    }
                }
                let pushed = push.is_some();
                let saved_push = std::mem::replace(&mut self.pushed_lock, push);
                let r = self.bind_subquery(rs.subquery.as_deref().unwrap());
                self.pushed_lock = saved_push;
                let deferred = std::mem::take(&mut self.deferred);
                (self.defer_level, self.deferred) = saved_defer;
                self.level().clause = clause;
                if let Some((scope, n)) = hidden {
                    self.level().scope = scope;
                    self.unreachable.truncate(n);
                }
                let (plan, cols, params) = r?;
                let alias = rs.alias.as_ref();
                let name = alias.map(|a| a.aliasname.clone()).unwrap_or_default();
                let mut cols = cols;
                if let Some(a) = alias {
                    let colnames = names(&a.colnames);
                    if colnames.len() > cols.len() {
                        return Err(Error::new(
                            code::INVALID_COLUMN_REFERENCE,
                            format!(
                                "table \"{}\" has {} columns available but {} columns specified",
                                name,
                                cols.len(),
                                colnames.len()
                            ),
                        ));
                    }
                    for (c, n) in cols.iter_mut().zip(colnames) {
                        c.name = n;
                    }
                }
                let mut rel = derived_rel(&name, &cols, 0);
                rel.anonymous = alias.is_none();
                let mut plan = Arc::try_unwrap(plan).unwrap_or_else(|a| (*a).clone());
                if alias.is_some() {
                    let info = Arc::new((name.clone(), cols.iter().map(|c| c.name.clone()).collect()));
                    mark_subquery_windows(&mut plan, &info);
                }
                // (Grouping sets: no qual goes into the subquery; its Subquery Scan checks
                // them. Set-returning or volatile outputs: is_simple_subquery keeps it a
                // subquery, the clauses that don't read them going in.)
                let plan = if alias.is_some()
                    && (grouping_sets_subquery(&plan)
                        || optimize::unsafe_outputs(&plan).iter().any(|u| *u)
                        || own_query_level(rs.subquery.as_deref().unwrap(), &plan))
                    && self.deps.is_none()
                    && self.refs.is_none()
                    && !self.unplanned
                    && !self.deparse
                {
                    // (A subquery Postgres doesn't pull up: its whole row stays a Var to
                    // it, its IS [NOT] NULL test not expanded.)
                    rel.whole_var = true;
                    Plan::Barrier {
                        input: Arc::new(plan),
                        alias: name.clone(),
                        cols: cols.iter().map(|c| c.name.clone()).collect(),
                        secure: false,
                        path: None,
                        sorted: match inner(rs.subquery.as_deref().unwrap()) {
                            Some(NodeEnum::SelectStmt(s)) if !s.sort_clause.is_empty() => Some(s.sort_clause.len()),
                            _ => None,
                        },
                        rti: self.next_rti(),
                        copies: Vec::new(),
                    }
                } else if deferred
                    && rs.lateral
                    && !params.is_empty()
                    && !pushed
                    && crate::sql::paths::lateral_pullable(&plan, self.lateral_collapses(rs.subquery.as_deref().unwrap()))
                {
                    // A LATERAL subquery over a join Postgres pulls up
                    // (pull_up_simple_subquery: is_simple_subquery with its LATERAL
                    // restrictions): left unplanned, its relations the join group's above,
                    // no clause pushed into it (`JoinMeta::lateral_sub`).
                    let mut plan = plan;
                    union_alias(&mut plan, &name);
                    crate::sql::plan::mark_deferred_lateral(&mut plan);
                    plan
                } else if deferred {
                    // (Not one: its level planned now, as any.)
                    let mut plan = plan;
                    union_alias(&mut plan, &name);
                    let lim = self.arm_limit.filter(|a| a.0 == sub_level).map(|a| (a.1, a.2));
                    let _pushed = crate::scoped::set(&crate::sql::paths::PUSHED_LEVEL, pushed, "rowmarks::PUSHED_LEVEL");
                    let _own = crate::scoped::set(&crate::sql::paths::OWN_LOCK, false, "rowmarks::OWN_LOCK");
                    self.hidden_rtes += crate::sql::paths::plan_query_limited(self.ctx, &mut plan, lim);
                    drop(_pushed);
                    if let Err(e) = super::check_merge_keys(self.ctx, &mut plan) {
                        self.plan_error(e)?;
                    }
                    plan
                } else {
                    let mut plan = plan;
                    union_alias(&mut plan, &name);
                    plan
                };
                Ok((plan, Scope { rels: vec![rel] }, params))
            }
            Some(NodeEnum::RangeFunction(rf)) => self.bind_range_function(rf),
            Some(NodeEnum::JoinExpr(j)) => self.bind_join(j, locking),
            Some(NodeEnum::RangeTableSample(ts)) => self.bind_table_sample(ts, locking),
            Some(NodeEnum::RangeTableFunc(rtf)) => self.bind_xml_table(rtf),
            Some(NodeEnum::JsonTable(jt)) => self.bind_json_table(jt),
            _ => Err(crate::error::unsupported("this FROM item")),
        }
    }

    /// `rel TABLESAMPLE method (args) [REPEATABLE (seed)]`: a table or materialized view's scan
    /// that returns a sample of its rows.
    fn bind_table_sample(
        &mut self,
        ts: &pb::RangeTableSample,
        locking: Option<&Locking>,
    ) -> Result<(Plan, Scope, Vec<Expr>)> {
        let Some(NodeEnum::RangeVar(rv)) = ts.relation.as_deref().and_then(inner) else {
            return Err(crate::error::unsupported("TABLESAMPLE on this FROM item"));
        };
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        let cte = schema.is_none() && self.ctes.iter().flatten().any(|c| c.name == rv.relname);
        let sampled = !cte
            && match self.cat.find(schema, &rv.relname, &self.ctx.search_path) {
                Some(Relation::Table(_)) => true,
                Some(Relation::View(v)) => v.storage.is_some(),
                _ => false,
            };
        let (mut plan, rel) = self.bind_range_var(rv, locking)?;
        if !sampled {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "TABLESAMPLE clause can only be applied to tables and materialized views",
                ),
                rv.location,
            ));
        }
        let method = names(&ts.method).last().cloned().unwrap_or_default();
        let method: &'static str = match method.as_str() {
            "bernoulli" => "bernoulli",
            "system" => "system",
            _ => {
                return Err(with_pos(
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("tablesample method {method} does not exist"),
                    ),
                    ts.location,
                ));
            }
        };
        if ts.args.len() != 1 {
            return Err(with_pos(
                Error::new(
                    code::INVALID_TABLESAMPLE_ARGUMENT,
                    format!(
                        "tablesample method {method} requires 1 argument, not {}",
                        ts.args.len()
                    ),
                ),
                ts.location,
            ));
        }
        let arg = |b: &mut Self, n: &Node, ty: DataType| -> Result<Expr> {
            let e = b.in_clause(Clause::Other, |b| b.bind_expr(n))?;
            let from = e.ty();
            b.coerce_implicit(e, &ty).map_err(|e| {
                if matches!(from, DataType::Unknown) {
                    return e;
                }
                with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "argument of TABLESAMPLE must be type {}, not type {}",
                            ty.display(),
                            from.display()
                        ),
                    ),
                    ast::location(n),
                )
            })
        };
        let percent = arg(self, &ts.args[0], DataType::Float4)?;
        let repeatable = match ts.repeatable.as_deref() {
            Some(n) => Some(arg(self, n, DataType::Float8)?),
            None => None,
        };
        let sample = Some(Box::new(crate::sql::plan::TableSample {
            method,
            percent,
            repeatable,
        }));
        // Row-level security may have put a filter over the scan; a parent table has a scan
        // per member.
        super::optimize::scans_mut(&mut plan, &mut |s| s.sample = sample.clone());
        Ok((plan, Scope { rels: vec![rel] }, vec![]))
    }

    /// `restrict_nonsystem_relation_kind` with `view`: a query that is planned can't expand
    /// a view users made (ApplyRetrieveRule, rewriteTargetView). (Deparsing isn't planning:
    /// pg_dump sets it and prints views over views.)
    pub(crate) fn check_view_restricted(&self, v: &crate::catalog::ViewDef) -> Result<()> {
        if self.deps.is_some() || self.unplanned || self.deparse || v.oid < crate::catalog::FIRST_USER_OID {
            return Ok(());
        }
        if crate::session::settings::restricted_kinds(&self.ctx.sess.settings.borrow()).0 {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("access to non-system view \"{}\" is restricted", v.name),
            ));
        }
        Ok(())
    }

    /// `restrict_nonsystem_relation_kind` with `foreign-table`: no foreign table is planned.
    pub(crate) fn check_foreign_restricted(&self) -> Result<()> {
        if self.deps.is_some() || self.unplanned || self.deparse {
            return Ok(());
        }
        if crate::session::settings::restricted_kinds(&self.ctx.sess.settings.borrow()).1 {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "access to non-system foreign table is restricted",
            ));
        }
        Ok(())
    }

    /// A view's query, as a subquery that can't see the outer query.
    /// A view read as a subquery of a statement writing through its INSTEAD OF triggers:
    /// a security barrier, or a query Postgres doesn't pull up, stays a query level of its own
    /// (its Subquery Scan), as in a FROM clause.
    pub(crate) fn view_source_level(
        &self,
        v: &crate::catalog::ViewDef,
        plan: Plan,
        cols: &[OutCol],
        alias: &str,
    ) -> Plan {
        if v.storage.is_some() || self.deps.is_some() || self.refs.is_some() || self.unplanned || self.deparse {
            return plan;
        }
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        if v.option("security_barrier") {
            return Plan::Barrier {
                input: Arc::new(plan),
                alias: alias.to_string(),
                cols: names,
                secure: true,
                path: None,
                sorted: None,
                rti: Default::default(),
                copies: Vec::new(),
            };
        }
        let Ok(node) = ast::parse_stmt(&v.query) else {
            return plan;
        };
        if own_query_level(&node, &plan) || optimize::unsafe_outputs(&plan).iter().any(|u| *u) {
            let sorted = match inner(&node) {
                Some(NodeEnum::SelectStmt(s)) if !s.sort_clause.is_empty() => Some(s.sort_clause.len()),
                _ => None,
            };
            return Plan::Barrier {
                input: Arc::new(plan),
                alias: alias.to_string(),
                cols: names,
                secure: false,
                path: None,
                sorted,
                rti: Default::default(),
                copies: Vec::new(),
            };
        }
        plan
    }

    pub(crate) fn bind_view_query(
        &mut self,
        v: &crate::catalog::ViewDef,
    ) -> Result<(Plan, Vec<OutCol>)> {
        self.check_view_restricted(v)?;
        let node = ast::parse_stmt(&v.query)?;
        // A view reading ctid / xmin / …: its tables' scans carry them.
        self.want_sys |= super::mentions_sys_column(&node);
        let mut lvl = super::Level::new();
        lvl.barrier = true;
        self.levels.push(lvl);
        let saved_ctes = std::mem::take(&mut self.ctes);
        let saved_fns = std::mem::take(&mut self.fn_levels);
        // The view's query reads its tables with the view owner's privileges; with
        // security_invoker, the current user's (even inside another view).
        if v.option("security_barrier") {
            super::optimize::mark_statement_rls();
        }
        let saved_as = self.check_as;
        self.check_as = if v.option("security_invoker") {
            self.definer
        } else {
            Some(v.owner)
        };
        // Collecting a view's dependencies: not those of the views it reads.
        let nested = (self.deps.is_some() && self.levels.len() > 2)
            .then(|| (self.perms.clone(), self.deps.clone(), self.rowtype_deps.clone()));
        let saved_rowtypes = std::mem::replace(
            &mut self.view_rowtypes,
            (!v.rowtype_cols.is_empty()).then(|| Arc::new(v.rowtype_cols.clone())),
        );
        let r = self.bind_select_node(&node);
        self.view_rowtypes = saved_rowtypes;
        if let Some((perms, deps, rowtype_deps)) = nested {
            (self.perms, self.deps, self.rowtype_deps) = (perms, deps, rowtype_deps);
        }
        self.check_as = saved_as;
        self.fn_levels = saved_fns;
        self.ctes = saved_ctes;
        self.levels.pop();
        r
    }

    fn bind_range_var(
        &mut self,
        rv: &pb::RangeVar,
        locking: Option<&Locking>,
    ) -> Result<(Plan, ScopeRel)> {
        // An alias the engine added to a renamed relation (`ast::alias_name`) is the name
        // references use, but the relation shows (and scans) as itself.
        let named = rv.alias.as_ref().map(ast::alias_name);
        let alias = named.as_ref().map(|a| a.name.to_string());
        let implicit = named.as_ref().is_some_and(|a| !a.written);
        let implicit_schema = named.as_ref().and_then(|a| a.schema).map(str::to_string);
        let implicit_cols = named.as_ref().is_some_and(|a| a.implicit_cols);
        let written = if implicit { None } else { alias.clone() };
        let colnames = rv
            .alias
            .as_ref()
            .map(|a| names(&a.colnames))
            .unwrap_or_default();
        let schema = if rv.schemaname.is_empty() {
            None
        } else {
            Some(rv.schemaname.as_str())
        };
        // CTE references.
        if schema.is_none() {
            for level in self.ctes.iter().rev() {
                if let Some(c) = level.iter().find(|c| c.name == rv.relname) {
                    if c.no_returning {
                        return Err(with_pos(
                            Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "WITH query \"{}\" does not have a RETURNING clause",
                                    c.name
                                ),
                            ),
                            rv.location,
                        ));
                    }
                    // (Its query's plan-time error, now that it is read.)
                    if let Some(e) = c.deferred.clone() {
                        self.plan_error(e)?;
                    }
                    // An inlined one: its query as a subquery here.
                    if let Some(q) = &c.inline {
                        c.used.set(true);
                        // (The CTE's range table entry stays: EXPLAIN qualifies columns.)
                        self.hidden_rtes += 1;
                        let mut cols = c.cols.clone();
                        for (col, n) in cols.iter_mut().zip(&colnames) {
                            col.name = n.clone();
                        }
                        let rel = derived_rel(alias.as_deref().unwrap_or(&rv.relname), &cols, 0);
                        let mut q = (**q).clone();
                        self.restamp_rti(&mut q);
                        return Ok((q, rel));
                    }
                    c.used.set(true);
                    let mut cols = c.cols.clone();
                    for (col, n) in cols.iter_mut().zip(&colnames) {
                        col.name = n.clone();
                    }
                    let (name, refname) = (
                        c.name.clone(),
                        alias.clone().unwrap_or_else(|| c.name.clone()),
                    );
                    let names = cols.iter().map(|c| c.name.clone()).collect();
                    let working = c.working;
                    let work_filter = c.work_filter.clone();
                    let plan = if working {
                        let scan = Plan::WorkScan {
                            id: c.id,
                            ncols: c.cols.len(),
                            name,
                            alias: refname,
                            cols: names,
                        };
                        match work_filter {
                            Some(pred) => Plan::Filter { input: Arc::new(scan), pred },
                            None => scan,
                        }
                    } else {
                        Plan::CteScan {
                            id: c.id,
                            ncols: c.cols.len(),
                            name,
                            alias: refname,
                            cols: names,
                            rti: Default::default(),
                        }
                    };
                    let mut rel = derived_rel(alias.as_deref().unwrap_or(&rv.relname), &cols, 0);
                    rel.whole_var = c.aux == 0;
                    let n = rel.cols.len();
                    for col in &mut rel.cols[n - c.aux..] {
                        col.merged = true;
                        col.attnum = super::AUX_ATTNUM;
                    }
                    if working {
                        self.level().st.self_ref = true;
                    }
                    let mut plan = plan;
                    if let Plan::CteScan { rti, .. } = &mut plan {
                        *rti = self.next_rti();
                    }
                    return Ok((plan, rel));
                }
            }
        }
        // Transition tables of the trigger being run.
        if schema.is_none() {
            let enr = self
                .ctx
                .enrs
                .borrow()
                .iter()
                .rev()
                .find(|e| e.name == rv.relname)
                .cloned();
            if let Some(e) = enr {
                if self.persistent {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "transition table \"{}\" cannot be referenced in a persistent object",
                            e.name
                        ),
                    ));
                }
                let mut cols: Vec<OutCol> = e
                    .cols
                    .iter()
                    .map(|(n, t)| OutCol {
                        name: n.clone(),
                        ty: t.clone(),
                        table: 0,
                        attnum: 0,
                        domain_array: None,
                        domain: None,
                        collation: None,
                    })
                    .collect();
                for (col, n) in cols.iter_mut().zip(&colnames) {
                    col.name = n.clone();
                }
                let rel = derived_rel(alias.as_deref().unwrap_or(&rv.relname), &cols, 0);
                return Ok((
                    Plan::EnrScan {
                        name: e.name.clone(),
                        ncols: cols.len(),
                        alias: alias.clone(),
                        cols: cols.iter().map(|c| c.name.clone()).collect(),
                    },
                    rel,
                ));
            }
        }
        if let Some(s) = schema {
            self.check_schema_usage(s, rv.location)?;
        }
        let rel = self.cat.find(schema, &rv.relname, &self.ctx.search_path);
        let rel = match rel {
            Some(r) => r,
            None => match SysTable::lookup(schema, &rv.relname, &self.ctx.search_path) {
                Some(st) => {
                    let cols: Vec<OutCol> = st
                        .columns()
                        .iter()
                        .map(|(n, t)| OutCol {
                            name: n.clone(),
                            ty: t.clone(),
                            table: st.oid(),
                            attnum: 0,
                            domain_array: None,
                            domain: None,
                            collation: None,
                        })
                        .collect();
                    let mut cols = cols;
                    for (i, c) in cols.iter_mut().enumerate() {
                        c.attnum = i as i16 + 1;
                    }
                    for (col, n) in cols.iter_mut().zip(&colnames) {
                        col.name = n.clone();
                    }
                    // Catalogs like pg_statistic and pg_authid are superuser-only.
                    // (FOR UPDATE / SHARE needs UPDATE on it too.)
                    let name = alias.as_deref().unwrap_or(&rv.relname).to_string();
                    let mark = locking.and_then(|l| l.mark_for(&name)).map(|(m, w, _)| (m, w));
                    self.require_rel(
                        st.oid(),
                        crate::catalog::acl::SELECT
                            | if mark.is_some() { crate::catalog::acl::UPDATE } else { 0 },
                    );
                    let mut rel = derived_rel(alias.as_deref().unwrap_or(&rv.relname), &cols, 0);
                    rel.sys_table = Some(st.oid());
                    if alias.is_none() {
                        rel.schema = Some(if st.schema() == crate::catalog::PG_CATALOG_OID {
                            "pg_catalog".into()
                        } else {
                            "information_schema".into()
                        });
                    }
                    return Ok((
                        Plan::SysScan {
                            table: st,
                            filter: None,
                            path: None,
                            alias: alias.clone().filter(|a| a != &rv.relname),
                            mark,
                        },
                        rel,
                    ));
                }
                None if let Some(r) = self.toast_table_scan(schema, &rv.relname, alias.as_deref(), &colnames)? => {
                    return Ok(r);
                }
                None => {
                    let mut e = crate::catalog::no_such_relation(schema, &rv.relname);
                    // (isFutureCTE: a WITH item not in scope here.)
                    if schema.is_none() && self.future_ctes.contains(&rv.relname) {
                        e = e
                            .detail(format!("There is a WITH item named \"{}\", but it cannot be referenced from this part of the query.", rv.relname))
                            .hint("Use WITH RECURSIVE, or re-order the WITH items to remove forward references.");
                    }
                    return Err(with_pos(e, rv.location));
                }
            },
        };
        match rel {
            Relation::Table(t) => {
                self.check_foreign_scan(&t)?;
                let name = alias.clone().unwrap_or_else(|| t.name.clone());
                let mut rel = table_rel(
                    &t,
                    &name,
                    if alias.is_none() {
                        Some(self.cat.schema_name(t.schema).to_string())
                    } else {
                        implicit_schema.clone()
                    },
                );
                self.virtual_columns(&t, &mut rel)?;
                rel.src = Some(rv.location);
                if implicit {
                    rel.shown = Some(t.name.clone());
                }
                for c in &mut rel.cols {
                    c.src = Some(rv.location);
                }
                self.note_rel_ref(rv.location, t.oid, super::refs::RelUse::From, rv.alias.is_some());
                // Column aliases rename live columns in order.
                let mut it = colnames.iter();
                for c in rel.cols.iter_mut().filter(|c| !c.hidden) {
                    match it.next() {
                        Some(n) => {
                            c.name = n.clone();
                            if !implicit_cols {
                                c.src = None;
                            }
                        }
                        None => break,
                    }
                }
                if it.next().is_some() {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!(
                            "table \"{}\" has {} columns available but {} columns specified",
                            name,
                            t.live_columns().count(),
                            colnames.len()
                        ),
                    ));
                }
                let mark = locking.and_then(|l| l.mark_for(&name));
                let (mark, lock_order) = (mark.map(|(m, w, _)| (m, w)), mark.map_or(0, |(_, _, o)| o));
                // FOR UPDATE / SHARE needs UPDATE too (of a view's tables, when the view is
                // locked).
                self.require_rel(
                    t.oid,
                    crate::catalog::acl::SELECT
                        | if mark.is_some() || self.view_lock {
                            crate::catalog::acl::UPDATE
                        } else {
                            0
                        },
                );
                if !self.want_sys && self.policies_read_sys(&t) && self.rls_enabled(&t)? {
                    self.want_sys = true;
                }
                if self.want_sys {
                    rel.sys_col = Some(t.columns.len());
                }
                let scan = ScanPlan {
                    table: t.clone(),
                    index: None,
                    filter: None,
                    with_key: false,
                    reverse: false,
                    mark,
                    lock_order,
                    limit: None,
                    sys: self.want_sys,
                    alias: written.clone(),
                    current_of: None,
                    sample: None,
                    excl: None,
                    part_excl: None,
                    part: None,
                    access: Default::default(),
                    col_aliases: None,
                    written: Vec::new(),
                    phv_quals: Vec::new(),
                    phv_proj: false,
                    check_as: self.check_as,
                    rti: self.next_rti(),
                    useful: Default::default(),
                    params: Default::default(),
                    reltarget: Default::default(),
                    fdw: None,
                };
                let mut scan = scan;
                if !colnames.is_empty() {
                    scan.col_aliases = Some(Arc::new(
                        t.live_columns()
                            .zip(colnames.iter())
                            .map(|((_, c), a)| (c.name.clone(), a.clone()))
                            .collect(),
                    ));
                }
                let shown = if implicit { t.name.clone() } else { name.clone() };
                let mut plan =
                    if rv.inh && (!self.cat.children(t.oid).is_empty() || t.partkey.is_some()) {
                        self.expand_inheritance(scan, &shown)?
                    } else if t.partkey.is_some() {
                        // ONLY a partitioned table: it has no rows of its own.
                        let names = optimize::dummy_scan_names(&scan);
                        let rels = vec![Some(shown.clone()); names.len()];
                        let table = Some(t.oid);
                        Plan::Empty {
                            ncols: super::optimize::width(&Plan::Scan(Box::new(scan))),
                            cols: Some(Arc::new(names)),
                            info: Some(Arc::new(crate::sql::plan::DummyInfo { rels, ec: Vec::new(), table, ..Default::default() })),
                        }
                    } else {
                        if self.exclusion_mode() == "on" {
                            scan.excl = self.exclusion_constraints(&t)?;
                            scan.part_excl = self.partition_levels(&t, None)?;
                        }
                        Plan::Scan(Box::new(scan))
                    };
                // Row-level security: SELECT policies (and UPDATE ones for FOR UPDATE / SHARE).
                self.rls_sys = rel.sys_col;
                if let Some(q) = self.rls_using(&t, super::rls::SELECT)? {
                    let mut quals = vec![q];
                    if mark.is_some() {
                        // (list_append_unique: the same quals once.)
                    if let Some(q) = self.rls_using(&t, super::rls::UPDATE)?
                        && !quals.iter().any(|x| x.same_as(&q))
                    {
                        quals.push(q);
                    }
                    }
                    plan = super::optimize::add_rls_filter(&self.cat, plan, Expr::And(quals));
                }
                Ok((plan, rel))
            }
            Relation::View(v) => {
                // (A security barrier: the statement's quals stay above its own.)
                if v.option("security_barrier") {
                    super::optimize::mark_statement_rls();
                }
                let name = alias.clone().unwrap_or_else(|| v.name.clone());
                // A view locked FOR UPDATE / SHARE needs UPDATE, and so do the relations its
                // query reads (`markQueryForLocking`).
                let locked = self.view_lock
                    || locking.is_some_and(|l| l.mark_for(&name).is_some());
                self.require_rel(
                    v.oid,
                    crate::catalog::acl::SELECT
                        | if locked && v.storage.is_none() {
                            crate::catalog::acl::UPDATE
                        } else {
                            0
                        },
                );
                let mut cols: Vec<OutCol> = v
                    .columns
                    .iter()
                    .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                    .collect();
                let plan = match &v.storage {
                    Some(t) => {
                        for (c, tc) in cols.iter_mut().zip(&t.columns) {
                            if let Some(n) = &tc.collation {
                                c.collation = Some(super::Coll::Implicit(n.clone()));
                            }
                        }
                        let mark = locking.and_then(|l| l.mark_for(&name));
                        let (mark, lock_order) = (mark.map(|(m, w, _)| (m, w)), mark.map_or(0, |(_, _, o)| o));
                        // (CheckValidRowMarkRel, when the query runs.)
                        if mark.is_some() && !self.deparse {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!("cannot lock rows in materialized view \"{}\"", v.name),
                            ));
                        }
                        Plan::Scan(Box::new(ScanPlan {
                            table: t.clone(),
                            index: None,
                            filter: None,
                            with_key: false,
                            reverse: false,
                            mark,
                            lock_order,
                            limit: None,
                            sys: false,
                            alias: written.clone(),
                            current_of: None,
                            sample: None,
                            excl: None,
                            part_excl: None,
                            part: None,
                            access: Default::default(),
                            col_aliases: None,
                            written: Vec::new(),
                            phv_quals: Vec::new(),
                            phv_proj: false,
                            check_as: self.check_as,
                            rti: self.next_rti(),
                            useful: Default::default(),
                            params: Default::default(),
                            reltarget: Default::default(),
                            fdw: None,
                        }))
                    }
                    None if self.view_stack.contains(&v.oid) => {
                        // A view that (through others) reads itself: Postgres's rewriter
                        // fails on it; a query that is only analyzed (CREATE VIEW over it)
                        // never expands it, so it just sees the view's columns.
                        if self.deps.is_none() && !self.unplanned {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!(
                                    "infinite recursion detected in rules for relation \"{}\"",
                                    v.name
                                ),
                            ));
                        }
                        Plan::Empty {
                            ncols: cols.len(),
                            cols: None,
                            info: None,
                        }
                    }
                    None => {
                        self.hidden_rtes += 1;
                        crate::stack::check()?;
                        self.view_stack.push(v.oid);
                        let saved_lock = std::mem::replace(&mut self.view_lock, locked);
                        let push = if self.deparse { None } else { self.push_lock(locking, &name) };
                        let saved_push = std::mem::replace(&mut self.pushed_lock, push);
                        let r = self.bind_view_query(&v);
                        self.pushed_lock = saved_push;
                        self.view_lock = saved_lock;
                        self.view_stack.pop();
                        let (plan, vcols) = r?;
                        // A column the view's query reads that fails (`ViewDef::rowtype_cols`)
                        // fails only where this query uses it: the view's own row has a NULL
                        // there.
                        let (plan, raised) = match plan {
                            Plan::Project { input, mut exprs } => {
                                let mut raised = Vec::new();
                                for (i, e) in exprs.iter_mut().enumerate() {
                                    if crate::sql::expr::is_raising(e) {
                                        let null = Expr::null(e.ty());
                                        raised.push((i, std::mem::replace(e, null)));
                                    }
                                }
                                (Plan::Project { input, exprs }, raised)
                            }
                            p => (p, Vec::new()),
                        };
                        self.view_raised = raised;
                        for (i, (c, vc)) in cols.iter_mut().zip(&vcols).enumerate() {
                            // A query over a view depends on the view's columns.
                            (c.table, c.attnum) = if self.deps.is_some() || self.refs.is_some() {
                                (v.oid, i as i16 + 1)
                            } else {
                                (vc.table, vc.attnum)
                            };
                            c.collation = vc.collation.clone().map(super::Coll::implicit);
                        }
                        plan
                    }
                };
                let mut rel = derived_rel(&name, &cols, 0);
                for (i, e) in std::mem::take(&mut self.view_raised) {
                    if let Some(c) = rel.cols.get_mut(i) {
                        c.expr = e;
                    }
                }
                rel.src = Some(rv.location);
                rel.rel = Some(v.oid);
                for (i, c) in rel.cols.iter_mut().enumerate() {
                    c.src = (i >= colnames.len() || implicit_cols).then_some(rv.location);
                }
                for (c, n) in rel.cols.iter_mut().zip(&colnames) {
                    c.name = n.clone();
                }
                if alias.is_none() {
                    rel.schema = Some(self.cat.schema_name(v.schema).to_string());
                } else if implicit {
                    rel.schema = implicit_schema.clone();
                    rel.shown = Some(v.name.clone());
                }
                self.note_rel_ref(rv.location, v.oid, super::refs::RelUse::From, rv.alias.is_some());
                // A security barrier: the statement's leaky quals stay above its query.
                let plan = if v.option("security_barrier")
                    && v.storage.is_none()
                    && self.deps.is_none()
                    && self.refs.is_none()
                    && !self.unplanned
                    && !self.deparse
                {
                    Plan::Barrier {
                        input: Arc::new(plan),
                        alias: name.clone(),
                        cols: rel.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect(),
                        secure: true,
                        path: None,
                        sorted: None,
                        rti: self.next_rti(),
                        copies: Vec::new(),
                    }
                } else if v.storage.is_none()
                    && self.deps.is_none()
                    && self.refs.is_none()
                    && !self.unplanned
                    && !self.deparse
                    && !self.view_stack.contains(&v.oid)
                    && let Ok(node) = ast::parse_stmt(&v.query)
                    && (own_query_level(&node, &plan) || optimize::unsafe_outputs(&plan).iter().any(|u| *u))
                {
                    // (A view Postgres keeps a query level of its own: its Subquery Scan.)
                    let sorted = match inner(&node) {
                        Some(NodeEnum::SelectStmt(s)) if !s.sort_clause.is_empty() => Some(s.sort_clause.len()),
                        _ => None,
                    };
                    Plan::Barrier {
                        input: Arc::new(plan),
                        alias: name.clone(),
                        cols: rel.cols.iter().filter(|c| !c.hidden).map(|c| c.name.clone()).collect(),
                        secure: false,
                        path: None,
                        sorted,
                        rti: self.next_rti(),
                        copies: Vec::new(),
                    }
                } else {
                    plan
                };
                Ok((plan, rel))
            }
            Relation::Sequence(s) => {
                self.require_rel(s.oid, crate::catalog::acl::SELECT);
                // SELECT * FROM seq: last_value, log_cnt, is_called.
                let (last, called) = self.ctx.sess.db.seq_state(&s)?;
                let row = vec![
                    Expr::Const(Value::Int8(last), DataType::Int8),
                    Expr::Const(Value::Int8(self.ctx.sess.db.seq_log_cnt(&s)), DataType::Int8),
                    Expr::Const(Value::Bool(called), DataType::Bool),
                ];
                let cols = vec![
                    OutCol::new("last_value", DataType::Int8),
                    OutCol::new("log_cnt", DataType::Int8),
                    OutCol::new("is_called", DataType::Bool),
                ];
                let rel = derived_rel(alias.as_deref().unwrap_or(&s.name), &cols, 0);
                Ok((
                    Plan::Values {
                        rows: vec![row],
                        ncols: 3,
                        rti: self.next_rti(),
                    },
                    rel,
                ))
            }
            Relation::Index(i) => Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is an index", i.name),
                ),
                rv.location,
            )),
        }
    }

    /// A TOAST table read directly (`pg_toast.pg_toast_<table>`): its chunks, made from the
    /// table's out-of-line values. Its owner (and superusers) may read it.
    fn toast_table_scan(
        &mut self,
        schema: Option<&str>,
        name: &str,
        alias: Option<&str>,
        colnames: &[String],
    ) -> Result<Option<(Plan, ScopeRel)>> {
        let Some(t) = self
            .cat
            .find_toast(schema, name, &self.ctx.search_path)
            .filter(|(_, index)| !index)
            .map(|(t, _)| t)
        else {
            return Ok(None);
        };
        let in_schema = self.cat.schema_name(self.cat.toast_schema(&t)).to_string();
        if schema.is_some_and(|s| s != in_schema)
            || schema.is_none() && !self.ctx.search_path.contains(&in_schema)
        {
            return Ok(None);
        }
        let me = self.check_as.unwrap_or_else(|| self.ctx.current_user());
        if !self.cat.is_superuser(me) && !self.cat.has_privs_of_role(me, t.owner, self.ctx.db_owner()) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {name}"),
            ));
        }
        use crate::types::DataType as T;
        let out = [("chunk_id", T::Oid), ("chunk_seq", T::Int4), ("chunk_data", T::Bytea)];
        let def = FuncDef {
            name: "pg_toast",
            srf: Some(crate::sql::func::admin::toast_table_rows),
            out_cols: out.to_vec(),
            volatility: 'v',
            ..(*crate::sql::func::candidate_def(0, vec![T::Oid], T::Record)).clone()
        };
        let ty = T::row(out.iter().map(|(n, t)| (n.to_string(), t.clone())).collect());
        let call = SrfCall {
            func: Arc::new(def),
            args: vec![Expr::Const(Value::Oid(t.toast), T::Oid)],
            ty,
            ncols: 3,
            body: None,
        };
        let mut cols: Vec<OutCol> = out
            .iter()
            .map(|(n, ty)| OutCol {
                name: n.to_string(),
                ty: ty.clone(),
                table: 0,
                attnum: 0,
                domain_array: None,
                domain: None,
                collation: None,
            })
            .collect();
        for (c, n) in cols.iter_mut().zip(colnames) {
            c.name = n.clone();
        }
        let refname = alias.unwrap_or(name).to_string();
        let rel = derived_rel(&refname, &cols, 0);
        Ok(Some((
            Plan::FunctionScan {
                calls: vec![call],
                ordinality: false,
                alias: refname,
                cols: cols.iter().map(|c| c.name.clone()).collect(),
                rti: self.next_rti(),
            },
            rel,
        )))
    }

    fn bind_range_function(&mut self, rf: &pb::RangeFunction) -> Result<(Plan, Scope, Vec<Expr>)> {
        if let Some(r) = self.bind_user_range_function(rf)? {
            return Ok(r);
        }
        let alias = rf.alias.as_ref();
        let colnames = alias.map(|a| names(&a.colnames)).unwrap_or_default();
        // Each function: list [FuncCall, coldeflist].
        let mut calls: Vec<SrfCall> = Vec::new();
        let mut cols: Vec<OutCol> = Vec::new();
        let ((), params) = self.in_level(|b| {
            for f in &rf.functions {
                let Some(NodeEnum::List(l)) = inner(f) else {
                    continue;
                };
                let fnode = &l.items[0];
                let coldefs: Vec<&Node> = match l.items.get(1).and_then(inner) {
                    Some(NodeEnum::List(cd)) => cd.items.iter().collect(),
                    _ => Vec::new(),
                };
                let coldefs: Vec<&Node> = if coldefs.is_empty() && rf.functions.len() == 1 {
                    rf.coldeflist.iter().collect()
                } else {
                    coldefs
                };
                // unnest(a, b, …) in FROM is shorthand for ROWS FROM(unnest(a), unnest(b), …).
                if let Some(NodeEnum::FuncCall(fc)) = inner(fnode)
                    && fc.args.len() > 1
                    && names(&fc.funcname).last().is_some_and(|n| n == "unnest")
                    && coldefs.is_empty()
                {
                    for a in &fc.args {
                        let one = Node {
                            node: Some(NodeEnum::FuncCall(Box::new(pb::FuncCall {
                                args: vec![a.clone()],
                                ..(**fc).clone()
                            }))),
                        };
                        let (call, fcols) = b.in_clause(Clause::FromFunction, |b| {
                            b.bind_from_function(&one, &[], false)
                        })?;
                        calls.push(call);
                        cols.extend(fcols);
                    }
                    continue;
                }
                let (call, mut fcols) = b.in_clause(Clause::FromFunction, |b| {
                    b.bind_from_function(fnode, &coldefs, rf.functions.len() == 1 && !rf.ordinality)
                })?;
                // A scalar result's collation is what the arguments derive.
                if let [c] = fcols.as_mut_slice()
                    && c.ty.collatable()
                {
                    let mut merged = super::Coll::None;
                    for a in &call.args {
                        merged = merged.merge(b.collation_of(a)?)?;
                    }
                    if let super::Coll::Implicit(_) | super::Coll::Explicit(..) = merged {
                        c.collation = Some(merged);
                    }
                }
                calls.push(call);
                cols.extend(fcols);
            }
            Ok(())
        })?;
        if rf.ordinality {
            cols.push(OutCol::new("ordinality", DataType::Int8));
        }
        // Single scalar function without column aliases: the column is named after its OUT
        // parameter, else the alias, else the function (chooseScalarFunctionAlias).
        let name = alias.map(|a| a.aliasname.clone()).unwrap_or_else(|| {
            calls
                .first()
                .map(|c| c.func.name.to_string())
                .unwrap_or_default()
        });
        // A user function's lone OUT parameter (`RETURNS TABLE (x int)`) or composite field
        // names the column (a built-in's definition lists its OUT columns only when there are
        // several).
        let named_out = calls.first().is_some_and(|c| {
            !c.func.out_cols.is_empty()
                || self.cat.functions.get(&c.func.oid).is_some_and(|f| {
                    f.out_cols.len() == 1 || matches!(f.ret, DataType::Row(_) | DataType::Record)
                })
        });
        if calls.len() == 1
            && calls[0].ncols == 1
            && calls[0].func.out_cols.len() <= 1
            && !matches!(calls[0].ty, DataType::Row(_))
        {
            if let Some(a) = alias
                && a.colnames.is_empty()
                && !named_out
            {
                cols[0].name = a.aliasname.clone();
            } else if alias.is_none() && !named_out {
                cols[0].name = calls[0].func.name.to_string();
            }
        }
        if colnames.len() > cols.len() {
            return Err(Error::new(
                code::INVALID_COLUMN_REFERENCE,
                format!(
                    "table \"{name}\" has {} columns available but {} columns specified",
                    cols.len(),
                    colnames.len()
                ),
            ));
        }
        for (c, n) in cols.iter_mut().zip(&colnames) {
            c.name = n.clone();
        }
        let mut rel = derived_rel(&name, &cols, 0);
        rel.scalar_row = calls.len() == 1
            && calls[0].ncols == 1
            && cols.len() == 1
            && !matches!(calls[0].ty, DataType::Row(_) | DataType::Record);
        let results: Vec<(DataType, usize)> = calls.iter().map(|c| (c.ty.clone(), c.ncols)).collect();
        self.mark_rowtype_cols(&mut rel, &results);
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        // inline_set_returning_function: a SETOF record SQL function (its column
        // definitions given) alone in FROM is its query, a subquery under its name.
        if let [call] = calls.as_slice()
            && !rf.ordinality
            && let Some(body) = &call.body
            && let Some(f) = self.cat.functions.get(&call.func.oid).cloned()
            && f.language == "sql"
            && f.returns_set
            && matches!(f.ret, DataType::Record)
            && f.out_cols.is_empty()
            && self
                .sql_fn_at(&f, &call.args, super::expr::FnPlace::FromSet)
                .is_some_and(|c| c.inlined())
        {
            let mut body = (**body).clone();
            mark_subquery_windows(&mut body, &Arc::new((name.clone(), names.clone())));
            let args = if crate::exec::reads_enclosing(&body, 0) {
                call.args.clone()
            } else {
                Vec::new()
            };
            let plan = Plan::Join {
                kind: JoinKind::Inner,
                left: Arc::new(Plan::One),
                right: Arc::new(body),
                cond: None,
                hash: None,
                params: args,
                nleft: 0,
                nright: call.ncols,
                meta: Default::default(),
            };
            return Ok((plan, Scope { rels: vec![rel] }, params));
        }
        Ok((
            Plan::FunctionScan {
                calls,
                ordinality: rf.ordinality,
                alias: name.clone(),
                cols: names,
                rti: self.next_rti(),
            },
            Scope { rels: vec![rel] },
            params,
        ))
    }

    /// `process_function_rte_ref`: while collecting dependencies, the columns of a FROM
    /// function's named composite result (`results`: each function's result type and column
    /// count) are the columns of the composite type's relation, numbered with its dropped
    /// ones. (Also binding a stored view, which checks them: `ViewDef::rowtype_cols`.)
    fn mark_rowtype_cols(&self, rel: &mut ScopeRel, results: &[(DataType, usize)]) {
        if self.deps.is_none() && self.view_rowtypes.is_none() {
            return;
        }
        let mut at = 0;
        for (ty, n) in results {
            let attnums = ty.row_oid().and_then(|oid| {
                if let Some(t) = self.cat.tables.values().find(|t| t.rowtype == oid) {
                    let live = t.columns.iter().enumerate().filter(|(_, c)| !c.dropped);
                    return Some((t.oid, live.map(|(i, _)| i as i16 + 1).collect::<Vec<_>>()));
                }
                if let Some(v) = self.cat.views.values().find(|v| v.rowtype == oid) {
                    return Some((v.oid, (1..=v.columns.len() as i16).collect()));
                }
                let c = self.cat.composites.values().find(|c| c.oid == oid)?;
                Some((c.relid, (1..=c.fields.len() as i16).collect()))
            });
            if let Some((relid, attnums)) = attnums {
                for (c, a) in rel.cols.iter_mut().skip(at).take(*n).zip(attnums) {
                    c.rowtype_col = Some((relid, a));
                }
            }
            at += n;
        }
    }

    /// `FROM f(...)` for a CREATE FUNCTION function: its body runs once per outer row, with
    /// the arguments as parameters.
    fn bind_user_range_function(
        &mut self,
        rf: &pb::RangeFunction,
    ) -> Result<Option<(Plan, Scope, Vec<Expr>)>> {
        if rf.functions.len() != 1 || rf.ordinality || !rf.coldeflist.is_empty() {
            return Ok(None);
        }
        let Some(NodeEnum::List(l)) = inner(&rf.functions[0]) else {
            return Ok(None);
        };
        let Some(NodeEnum::FuncCall(f)) = l.items.first().and_then(inner) else {
            return Ok(None);
        };
        let fname = names(&f.funcname);
        let name = fname.last().cloned().unwrap_or_default();
        let schema = if fname.len() > 1 {
            Some(fname[fname.len() - 2].clone())
        } else {
            None
        };
        let has_user = self.cat.functions.values().any(|x| x.name == name);
        if !has_user {
            return Ok(None);
        }
        let (r, params) = self.in_level(|b| {
            let Some((fdef, args)) = b.in_clause(Clause::FromFunction, |b| {
                b.resolve_user_function(schema.as_deref(), &name, f)
            })?
            else {
                return Ok(None);
            };
            // A record needs its column definitions.
            if matches!(fdef.ret, DataType::Record) && fdef.out_cols.is_empty() {
                return Ok(None);
            }
            // pull_up_constant_function: a scalar function of constants that folds to a
            // constant (immutable, or an inlined SQL function) is a one-row FROM item of it.
            if !fdef.returns_set
                && (fdef.volatility == 'i' || (fdef.language == "sql" && fdef.volatility == 's'))
                && fdef.out_cols.is_empty()
                && !matches!(fdef.ret, DataType::Record | DataType::Row(_))
                && args.iter().all(|a| matches!(a, Expr::Const(..)))
                && !b.deparse
                && let Some(node) = l.items.first()
                && let Ok(e) = b.in_clause(Clause::FromFunction, |b| b.bind_expr(node))
                // (An immutable call of constants is evaluated while planning.)
                && let Some(e) = match e {
                    e @ Expr::Const(..) => Some(e),
                    Expr::Subquery(sub)
                        if sub.func == Some('i')
                            && sub.params.iter().all(|p| matches!(p, Expr::Const(..))) =>
                    {
                        let ty = sub.ty.clone();
                        crate::sql::expr::eval(&Expr::Subquery(sub), &[], b.ctx)
                            .ok()
                            .map(|v| Expr::Const(v, ty))
                    }
                    _ => None,
                }
            {
                let cols = vec![OutCol::new(name.clone(), e.ty())];
                let plan = Plan::Project {
                    input: Arc::new(Plan::One),
                    exprs: vec![e],
                };
                return Ok(Some((plan, cols, DataType::Unknown, false, true)));
            }
            // A C function has no body: its native code runs as a built-in function does.
            if fdef.language == "c" {
                return Ok(None);
            }
            let place = if fdef.returns_set {
                super::expr::FnPlace::FromSet
            } else {
                super::expr::FnPlace::Scalar
            };
            if fdef.language == "plpgsql" {
                b.call_collation(&args)?;
            }
            let at = b.sql_fn_at(&fdef, &args, place);
            // OUT parameters returned as one row value: expanded (and checked) outside the
            // function, as the scan reads its result.
            let mut expand: Option<Vec<OutCol>> = None;
            let (body, mut cols) = b.in_sql_fn(&fdef, at, |b| {
                let (body, bcols) = b.bind_user_body(&fdef, &args)?;
                if fdef.language == "sql" {
                    super::check_sql_retval(&b.cat, &b.ctx.search_path, &fdef, Some(&bcols))?;
                }
                // Result columns: TABLE/OUT columns, else one column named after the function.
                let mut cols: Vec<OutCol> =
                    if !fdef.out_cols.is_empty() && fdef.out_cols.len() == bcols.len() {
                        fdef.out_cols
                            .iter()
                            .enumerate()
                            .map(|(i, (n, t))| OutCol::new(super::expr::out_col_name(n, i), t.clone()))
                            .collect()
                    } else if let DataType::Row(r) = &fdef.ret
                        && r.oid != 0
                        && r.fields.len() == bcols.len()
                        && !(bcols.len() == 1 && bcols[0].ty.row_oid() == Some(r.oid))
                    {
                        // A named composite result: its columns.
                        r.fields
                            .iter()
                            .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                            .collect()
                    } else if matches!(fdef.ret, DataType::Record | DataType::Row(_)) {
                        bcols.clone()
                    } else {
                        let mut c = OutCol::new(name.clone(), fdef.ret.clone());
                        c.domain = fdef.ret_domain;
                        c.domain_array = b.domain_array(c.domain);
                        vec![c]
                    };
                // A named composite result returned as one row value (`select row(r, i)`):
                // its fields are the columns, checked against them when it runs.
                if fdef.out_cols.is_empty()
                    && let DataType::Row(r) = &fdef.ret
                    && r.oid != 0
                    && r.fields.len() > 1
                    && let [one] = &bcols[..]
                    && (matches!(one.ty, DataType::Record)
                        || matches!(&one.ty, DataType::Row(x) if x.oid == 0 || x.oid == r.oid))
                {
                    expand = Some(
                        r.fields
                            .iter()
                            .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                            .collect(),
                    );
                    return Ok((body, bcols));
                }
                // OUT parameters returned as one row value (`select (1, 2.1)`): its fields
                // are the columns, checked against them when it runs.
                if fdef.out_cols.len() > 1
                    && let [one] = &bcols[..]
                    && (matches!(one.ty, DataType::Record)
                        || matches!(&one.ty, DataType::Row(r) if r.oid == 0))
                {
                    let outs: Vec<OutCol> = fdef
                        .out_cols
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t))| OutCol::new(super::expr::out_col_name(n, i), t.clone()))
                        .collect();
                    expand = Some(outs);
                    return Ok((body, bcols));
                }
                // A void result: what the body returns is thrown away (no rows of a SETOF
                // one).
                if fdef.ret == DataType::Void {
                    let plan = if fdef.returns_set {
                        Plan::Empty { ncols: 1, cols: None, info: None }
                    } else {
                        // A body run when called (PL/pgSQL, statements) still runs.
                        let input = if matches!(body, Plan::PlCall { .. }) {
                            body
                        } else {
                            Plan::One
                        };
                        Plan::Project {
                            input: Arc::new(input),
                            exprs: vec![Expr::Const(Value::Null, DataType::Void)],
                        }
                    };
                    return Ok((plan, vec![OutCol::new(name.clone(), DataType::Void)]));
                }
                if cols.len() != bcols.len() {
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        format!(
                            "return type mismatch in function declared to return {}",
                            fdef.ret.display()
                        ),
                    )
                    .detail("Final statement returns too many columns."));
                }
                // A named composite returned as one value of it: its fields are the
                // columns.
                if let DataType::Row(rt) = &fdef.ret
                    && rt.oid != 0
                    && fdef.out_cols.is_empty()
                    && bcols.len() == 1
                    && bcols[0].ty.row_oid() == Some(rt.oid)
                {
                    let row = Expr::Col(0, bcols[0].ty.clone());
                    let cols: Vec<OutCol> = rt
                        .fields
                        .iter()
                        .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                        .collect();
                    let exprs = rt
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(i, (_, t))| Expr::Field(Box::new(row.clone()), i, t.clone()))
                        .collect();
                    return Ok((
                        Plan::Project {
                            input: Arc::new(body),
                            exprs,
                        },
                        cols,
                    ));
                }
                let mut exprs = Vec::new();
                let n = cols.len();
                for (i, (c, bc)) in cols.iter_mut().zip(&bcols).enumerate() {
                    let e = b.coerce_assign(Expr::Col(i, bc.ty.clone()), &c.ty, &c.name)?;
                    exprs.push(b.result_domain(&fdef, i, n, e)?);
                }
                // check_sql_fn_retval: a plain query's own target list takes the
                // coercions (constants folded with them).
                let mut body = body;
                if exprs.iter().enumerate().any(|(i, e)| !matches!(e, Expr::Col(j, _) if *j == i))
                    && let Plan::Project { input, exprs: bexprs } = &body
                    && bexprs.len() == n
                {
                    let mut composed = Vec::with_capacity(n);
                    for (i, (c, be)) in cols.iter().zip(bexprs).enumerate() {
                        let e = b.coerce_assign(be.clone(), &c.ty, &c.name)?;
                        composed.push(b.result_domain(&fdef, i, n, e)?);
                    }
                    exprs = composed.iter().enumerate().map(|(i, e)| Expr::Col(i, e.ty())).collect();
                    body = Plan::Project {
                        input: input.clone(),
                        exprs: composed,
                    };
                }
                // check_sql_fn_retval: a set operation's columns coerced to the result's
                // types are a projection over it as a subquery, "*SELECT*".
                fn setop(p: &Plan) -> bool {
                    match p {
                        Plan::Sort { input, .. } | Plan::Limit { input, .. } => setop(input),
                        p => matches!(p, Plan::SetOp { .. }),
                    }
                }
                if setop(&body)
                    && exprs.iter().enumerate().any(|(i, e)| !matches!(e, Expr::Col(j, _) if *j == i))
                {
                    let names = unique_colnames(bcols.iter().map(|c| c.name.clone()));
                    mark_subquery_windows(&mut body, &Arc::new(("*SELECT*".to_string(), names)));
                }
                Ok((
                    Plan::Project {
                        input: Arc::new(body),
                        exprs,
                    },
                    cols,
                ))
            })?;
            let mut body = b.sql_fn_plan(&fdef, at, body);
            if let Some(outs) = expand {
                let mut e = Expr::Col(0, cols[0].ty.clone());
                if let (Some(d), true) = (fdef.ret_domain, fdef.out_cols.is_empty()) {
                    e = b.domain_check(e, d)?;
                }
                let exprs = b.checked_record_fields(e, &outs);
                body = Plan::Project {
                    input: Arc::new(body),
                    exprs,
                };
                cols = outs;
            }
            if !fdef.returns_set {
                body = Plan::Limit {
                    input: Arc::new(body),
                    limit: Some(Expr::Const(Value::Int8(1), DataType::Int8)),
                    offset: None,
                    with_ties: None,
                };
            }
            let n = cols.len();
            // (A PL function's call, a SQL one not inlined: EXPLAIN's Function Scan; named
            // once the alias is. A SQL function not SETOF is never inlined into FROM, but
            // its call is evaluated or inlined as an expression.)
            let sql_kept = fdef.language == "sql" && (!fdef.returns_set || !at.is_some_and(|c| c.inlined()));
            let call = if fdef.language == "sql"
                && !fdef.returns_set
                && at.is_some_and(|c| c.inlined())
            {
                match b.user_call(fdef.clone(), args.clone(), -1) {
                    // (Inlined over constant arguments: its body's expression.)
                    Ok(Expr::Subquery(sq)) if sq.call.as_ref().is_some_and(|c| c.inline) => {
                        super::expr::inline_scalar(&sq.plan, &args)
                    }
                    Ok(Expr::Subquery(_) | Expr::Case { .. }) | Err(_) => None,
                    Ok(e) => Some(e),
                }
            } else {
                None
            };
            let from_call = (fdef.language != "sql" || sql_kept).then(|| {
                Arc::new(crate::sql::plan::FromCall {
                    def: crate::sql::func::display_def(&fdef),
                    call,
                    schema: b.cat.schema_name(fdef.schema).to_string(),
                    alias: String::new(),
                    cols: Vec::new(),
                    rows: if fdef.returns_set { fdef.prorows() as f64 } else { 1.0 },
                    cost: fdef.procost() as f64,
                    width: 0,
                })
            });
            // Inlined over arguments put in its body: nothing to pass it.
            let args = if fdef.language == "sql"
                && fdef.returns_set
                && at.is_some_and(|c| c.inlined())
                && !crate::exec::reads_enclosing(&body, 0)
            {
                Vec::new()
            } else {
                args
            };
            let plan = Plan::Join {
                // Not SETOF: its first row, or nulls.
                kind: if fdef.returns_set {
                    JoinKind::Inner
                } else {
                    JoinKind::Left
                },
                left: Arc::new(Plan::One),
                right: Arc::new(body),
                cond: None,
                hash: None,
                params: args,
                nleft: 0,
                nright: n,
                meta: Box::new(crate::sql::plan::JoinMeta {
                    from_call,
                    ..Default::default()
                }),
            };
            // chooseScalarFunctionAlias: a lone OUT parameter (`RETURNS TABLE (x int)`) or a
            // composite's field names the column; only a plain scalar takes the alias.
            let named = !fdef.out_cols.is_empty() || matches!(fdef.ret, DataType::Row(_) | DataType::Record);
            let scalar = fdef.out_cols.len() == 1 || !matches!(fdef.ret, DataType::Row(_) | DataType::Record);
            Ok(Some((plan, cols, fdef.ret.clone(), named, scalar)))
        })?;
        let Some((mut plan, mut cols, ret, named, scalar)) = r else {
            return Ok(None);
        };
        let mut params = params;
        // eval_const_expressions: a field of a whole-row argument from the items to its
        // left is that column (the call as the planner leaves it).
        if let Plan::Join { meta, .. } = &mut plan
            && let Some(fc) = &mut meta.from_call
            && let Some(call) = fc.call.clone()
        {
            let call = call
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Field(r, k, t) => {
                            let col = match (&*r, &r) {
                                (Expr::Outer(1, j, _), _) => match params.get(*j) {
                                    Some(w @ Expr::Row(items, _)) if super::is_whole_row(w) => {
                                        items.get(k).filter(|c| c.ty() == t).cloned()
                                    }
                                    _ => None,
                                },
                                _ => None,
                            };
                            match col {
                                Some(col) => {
                                    let at = params.iter().position(|p| p.same_as(&col)).unwrap_or_else(|| {
                                        params.push(col);
                                        params.len() - 1
                                    });
                                    Expr::Outer(1, at, t)
                                }
                                None => Expr::Field(r, k, t),
                            }
                        }
                        x => x,
                    })
                })
                .unwrap_or(call);
            Arc::make_mut(fc).call = Some(call);
        }
        let alias = rf.alias.as_ref();
        let colnames = alias.map(|a| names(&a.colnames)).unwrap_or_default();
        let rel_name = alias
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| name.clone());
        if cols.len() == 1
            && colnames.is_empty()
            && !named
            && let Some(a) = alias
        {
            cols[0].name = a.aliasname.clone();
        }
        if colnames.len() > cols.len() {
            return Err(Error::new(
                code::INVALID_COLUMN_REFERENCE,
                format!(
                    "table \"{rel_name}\" has {} columns available but {} columns specified",
                    cols.len(),
                    colnames.len()
                ),
            ));
        }
        for (c, n) in cols.iter_mut().zip(&colnames) {
            c.name = n.clone();
        }
        if let Plan::Join { meta, .. } = &mut plan
            && let Some(fc) = &mut meta.from_call
        {
            let fc = Arc::make_mut(fc);
            fc.alias = rel_name.clone();
            fc.cols = cols.iter().map(|c| c.name.clone()).collect();
            fc.width = cols.iter().map(|c| crate::sql::costs::type_width(&c.ty)).sum();
        }
        // An inlined set-returning SQL function: a FROM subquery under its name.
        if let Plan::Join {
            left, right, meta, ..
        } = &mut plan
            && meta.from_call.is_none()
            && matches!(&**left, Plan::One)
        {
            let info = Arc::new((rel_name.clone(), cols.iter().map(|c| c.name.clone()).collect()));
            mark_subquery_windows(Arc::make_mut(right), &info);
        }
        let mut rel = derived_rel(&rel_name, &cols, 0);
        rel.scalar_row = cols.len() == 1 && scalar;
        if let DataType::Row(rt) = &ret
            && rt.oid != 0
        {
            rel.row_ty = Some(ret.clone());
        }
        self.mark_rowtype_cols(&mut rel, &[(ret, cols.len())]);
        Ok(Some((plan, Scope { rels: vec![rel] }, params)))
    }

    /// A function in FROM: its call and output columns. `alone`: it is the only one, without
    /// ORDINALITY (so the planner may inline it).
    pub(crate) fn bind_from_function(
        &mut self,
        n: &Node,
        coldefs: &[&Node],
        alone: bool,
    ) -> Result<(SrfCall, Vec<OutCol>)> {
        let Some(NodeEnum::FuncCall(f)) = inner(n) else {
            // Other expressions in FROM (e.g. `FROM CAST(... AS int)`) are one-row values,
            // named as a select-list item would be (`FROM CURRENT_DATE` is "current_date").
            let e = self.bind_expr(n)?;
            let ty = e.ty();
            let name = figure_colname(n);
            if !coldefs.is_empty() {
                return self.record_expr_from(e, coldefs, n);
            }
            let mut call = const_srf(e);
            let mut def = (*call.func).clone();
            def.name = crate::sql::func::intern(&name);
            call.func = Arc::new(def);
            return Ok((call, vec![OutCol::new(name, ty)]));
        };
        let fname = names(&f.funcname);
        let name = fname.last().cloned().unwrap_or_default();
        let schema = (fname.len() > 1).then(|| fname[fname.len() - 2].as_str());
        if super::super::super::exec::agg::is_aggregate(&name)
            || !self.user_aggs(schema, &name).is_empty()
        {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    "aggregate functions are not allowed in functions in FROM",
                ),
                f.location,
            ));
        }
        let (def, args, ret) = match self.resolve_call(&name, &f.args, f.func_variadic, f.location)
        {
            Ok(r) => r,
            Err(e) => {
                if let Ok(Some((fdef, args))) = self.resolve_user_function(schema, &name, f) {
                    return self.user_from_function(&fdef, args, coldefs, f.location, alone);
                }
                // A user function or expression: evaluate as a one-row value.
                let ex = self.bind_expr(n).map_err(|_| e)?;
                let ty = ex.ty();
                return Ok((const_srf(ex), vec![OutCol::new(name, ty)]));
            }
        };
        self.note_builtin_call(def.oid);
        // A polymorphic result the arguments don't decide (array_in's anyarray).
        if let crate::sql::func::Ret::Fixed(declared) = &def.ret
            && declared.is_polymorphic()
            && !def.args.iter().any(DataType::is_polymorphic)
        {
            return Err(with_pos(
                Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "function \"{name}\" in FROM has unsupported return type {}",
                        declared.display()
                    ),
                ),
                f.location,
            ));
        }
        // addRangeTableEntryForFunction: a column definition list only for `record`.
        if let Some(cd) = coldefs.first()
            && (def.out_cols.len() > 1
                || !(matches!(ret, DataType::Record)
                    || matches!(&ret, DataType::Row(r) if r.oid == 0)))
        {
            let at = match inner(cd) {
                Some(NodeEnum::ColumnDef(c)) => c.location,
                _ => f.location,
            };
            let msg = if def.out_cols.len() > 1 {
                "a column definition list is redundant for a function with OUT parameters"
            } else if matches!(&ret, DataType::Row(r) if r.oid != 0) {
                "a column definition list is redundant for a function returning a named composite type"
            } else {
                "a column definition list is only allowed for functions returning \"record\""
            };
            return Err(with_pos(Error::new(code::SYNTAX_ERROR, msg), at));
        }
        let mut cols: Vec<OutCol> = Vec::new();
        if def.out_cols.len() > 1 {
            for (n, t) in &def.out_cols {
                let t = match t {
                    DataType::AnyElement
                    | DataType::AnyCompatible
                    | DataType::AnyCompatibleNonArray => ret.clone(),
                    t => t.clone(),
                };
                cols.push(OutCol::new(*n, t));
            }
        } else if matches!(ret, DataType::Record)
            || matches!(&ret, DataType::Row(r) if r.oid == 0 && !coldefs.is_empty())
        {
            if coldefs.is_empty() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "a column definition list is required for functions returning \"record\"",
                    ),
                    f.location,
                ));
            }
            for cd in coldefs {
                let Some(NodeEnum::ColumnDef(c)) = inner(cd) else {
                    continue;
                };
                let (t, _) = super::resolve_type_name(
                    &self.cat,
                    &self.ctx.search_path,
                    c.type_name.as_ref().unwrap(),
                )?;
                cols.push(OutCol::new(c.colname.clone(), t));
            }
        } else if let DataType::Row(fields) = &ret {
            for (n, t) in fields.iter() {
                cols.push(OutCol::new(n.clone(), t.clone()));
            }
        } else {
            let n = def
                .out_cols
                .first()
                .map(|(n, _)| n.to_string())
                .unwrap_or_else(|| def.name.to_string());
            let mut c = OutCol::new(n, ret.clone());
            // anyelement from an array of a domain (unnest): that domain.
            if matches!(def.ret, crate::sql::func::Ret::Poly) {
                c.domain = def
                    .args
                    .iter()
                    .zip(&args)
                    .filter(|(t, _)| matches!(t, DataType::AnyArray | DataType::AnyCompatibleArray))
                    .find_map(|(_, a)| self.declared_domain(a))
                    .and_then(|d| self.cat.domain_type(d).filter(|(_, array)| *array))
                    .map(|(x, _)| x.oid);
                c.domain_array = self.domain_array(c.domain);
            }
            cols.push(c);
        }
        let ncols = cols.len();
        // A record function learns its column types from the definition list.
        let ret = if (matches!(ret, DataType::Record)
            || matches!(&ret, DataType::Row(r) if r.oid == 0))
            && !coldefs.is_empty()
        {
            DataType::row(
                cols.iter()
                    .map(|c| (c.name.clone(), c.ty.clone()))
                    .collect(),
            )
        } else {
            ret
        };
        let def = self.input_collation_def(def, &args);
        Ok((
            SrfCall {
                func: def,
                args,
                ty: ret,
                ncols,
                body: None,
            },
            cols,
        ))
    }

    /// The fields of record `e` as columns `cols`, each checked (`record_checked`) to be
    /// what they say.
    fn checked_record_fields(&self, e: Expr, cols: &[OutCol]) -> Vec<Expr> {
        let msg = match self.record_mismatch(&e.ty(), cols) {
            Some(m) => Value::Text(m.into()),
            None => Value::Null,
        };
        let row_ty = DataType::row(
            cols.iter()
                .map(|c| (c.name.clone(), c.ty.clone()))
                .collect(),
        );
        let def = record_check_def(e.ty(), row_ty.clone());
        let checked = Expr::Call(def, vec![e, Expr::Const(msg, DataType::Text)], row_ty);
        cols.iter()
            .enumerate()
            .map(|(i, c)| Expr::Field(Box::new(checked.clone()), i, c.ty.clone()))
            .collect()
    }

    /// tupledesc_match's complaint about a record of type `ty` read as columns `cols`.
    fn record_mismatch(&self, ty: &DataType, cols: &[OutCol]) -> Option<String> {
        let DataType::Row(r) = ty else {
            return None;
        };
        let f = &r.fields;
        if f.len() != cols.len() {
            return Some(format!(
                "Returned row contains {} attribute{}, but query expects {}.",
                f.len(),
                if f.len() == 1 { "" } else { "s" },
                cols.len()
            ));
        }
        f.iter().zip(cols).enumerate().find_map(|(i, ((_, got), want))| {
            let (g, w) = (
                crate::catalog::system::type_oid(&self.cat, got),
                crate::catalog::system::type_oid(&self.cat, &want.ty),
            );
            let binary = g == w
                || matches!(
                    crate::catalog::system::builtin_cast_method(g, w),
                    Some(('b', _))
                );
            (!binary).then(|| {
                format!(
                    "Returned type {} at ordinal position {}, but query expects {}.",
                    got.display(),
                    i + 1,
                    want.ty.display()
                )
            })
        })
    }

    /// `FROM expr AS t(col type, ...)` for an expression of type record: its fields as the
    /// listed columns, checked to match them when it runs.
    fn record_expr_from(
        &mut self,
        e: Expr,
        coldefs: &[&Node],
        n: &Node,
    ) -> Result<(SrfCall, Vec<OutCol>)> {
        let ty = e.ty();
        if !(matches!(ty, DataType::Record) || matches!(&ty, DataType::Row(r) if r.oid == 0)) {
            let at = match coldefs.first().and_then(|c| inner(c)) {
                Some(NodeEnum::ColumnDef(c)) => c.location,
                _ => super::ast::location(n),
            };
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    "a column definition list is only allowed for functions returning \"record\"",
                ),
                at,
            ));
        }
        let mut cols = Vec::new();
        for cd in coldefs {
            let Some(NodeEnum::ColumnDef(c)) = inner(cd) else {
                continue;
            };
            let (t, _) = super::resolve_type_name(
                &self.cat,
                &self.ctx.search_path,
                c.type_name.as_ref().unwrap(),
            )?;
            cols.push(OutCol::new(c.colname.clone(), t));
        }
        let mismatch = self.record_mismatch(&ty, &cols);
        let row_ty = DataType::row(
            cols.iter()
                .map(|c| (c.name.clone(), c.ty.clone()))
                .collect(),
        );
        let ret = if cols.len() == 1 {
            cols[0].ty.clone()
        } else {
            row_ty.clone()
        };
        let def = record_check_def(ty.clone(), ret);
        let msg = match mismatch {
            Some(m) => Value::Text(m.into()),
            None => Value::Null,
        };
        Ok((
            SrfCall {
                func: def,
                args: vec![e, Expr::Const(msg, DataType::Text)],
                ty: row_ty,
                ncols: cols.len(),
                body: None,
            },
            cols,
        ))
    }

    /// A CREATE FUNCTION function in FROM: a call running its body with the arguments as the
    /// outer row (`addRangeTableEntryForFunction`, `check_sql_stmt_retval`).
    fn user_from_function(
        &mut self,
        fdef: &crate::catalog::Function,
        args: Vec<Expr>,
        coldefs: &[&Node],
        loc: i32,
        alone: bool,
    ) -> Result<(SrfCall, Vec<OutCol>)> {
        let record = matches!(fdef.ret, DataType::Record) && fdef.out_cols.is_empty();
        if let Some(cd) = coldefs.first().filter(|_| !record) {
            let at = match inner(cd) {
                Some(NodeEnum::ColumnDef(c)) => c.location,
                _ => loc,
            };
            let msg = if !fdef.out_cols.is_empty() {
                "a column definition list is redundant for a function with OUT parameters"
            } else if matches!(&fdef.ret, DataType::Row(r) if r.oid != 0) {
                "a column definition list is redundant for a function returning a named composite type"
            } else {
                "a column definition list is only allowed for functions returning \"record\""
            };
            return Err(with_pos(Error::new(code::SYNTAX_ERROR, msg), at));
        }
        let mut cols: Vec<OutCol> = Vec::new();
        if record {
            if coldefs.is_empty() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "a column definition list is required for functions returning \"record\"",
                    ),
                    loc,
                ));
            }
            for cd in coldefs {
                let Some(NodeEnum::ColumnDef(c)) = inner(cd) else {
                    continue;
                };
                let (t, _) = super::resolve_type_name(
                    &self.cat,
                    &self.ctx.search_path,
                    c.type_name.as_ref().unwrap(),
                )?;
                cols.push(OutCol::new(c.colname.clone(), t));
            }
        }
        // A C function: its native code, as a built-in function in FROM runs.
        if fdef.language == "c"
            && let Some(def) = Self::native_def(fdef)
        {
            self.load_library(fdef);
            if !record {
                if fdef.out_cols.len() > 1 {
                    cols = fdef
                        .out_cols
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t))| OutCol::new(super::expr::out_col_name(n, i), t.clone()))
                        .collect();
                } else if let DataType::Row(r) = &fdef.ret {
                    cols = r.fields.iter().map(|(n, t)| OutCol::new(n.clone(), t.clone())).collect();
                } else {
                    let n = fdef
                        .out_cols
                        .first()
                        .map(|(n, _)| n.clone())
                        .unwrap_or_else(|| fdef.name.clone());
                    cols = vec![OutCol::new(n, fdef.ret.clone())];
                }
            }
            let ty = if record || cols.len() > 1 {
                DataType::row(cols.iter().map(|c| (c.name.clone(), c.ty.clone())).collect())
            } else {
                fdef.ret.clone()
            };
            let def = self.input_collation_def(Arc::new(def), &args);
            return Ok((
                SrfCall {
                    func: def,
                    args,
                    ty,
                    ncols: cols.len(),
                    body: None,
                },
                cols,
            ));
        }
        let place = match (fdef.returns_set, alone) {
            (true, true) => super::expr::FnPlace::FromSet,
            (true, false) => super::expr::FnPlace::Run,
            (false, _) => super::expr::FnPlace::Scalar,
        };
        if fdef.language == "plpgsql" {
            self.call_collation(&args)?;
        }
        let at = self.sql_fn_at(fdef, &args, place);
        // OUT parameters returned as one row value: expanded (and checked) outside the
        // function, as the scan reads its result.
        let mut expand: Option<Vec<OutCol>> = None;
        let (body, mut cols) = self.in_sql_fn(fdef, at, |b| {
            let mut cols = cols;
            // PL/pgSQL returns rows of the column definitions: RETURN NEXT / QUERY check
            // theirs against them as it runs.
            if record && fdef.language == "plpgsql" {
                let mut f = fdef.clone();
                f.ret = DataType::row(
                    cols.iter()
                        .map(|c| (c.name.clone(), c.ty.clone()))
                        .collect(),
                );
                let (body, _) = b.bind_user_body(&f, &args)?;
                return Ok((body, cols));
            }
            let (body, bcols) = b.bind_user_body(fdef, &args)?;
            if fdef.language == "sql" {
                super::check_sql_retval(&b.cat, &b.ctx.search_path, fdef, Some(&bcols))?;
            }
            if !record {
                cols = if !fdef.out_cols.is_empty() && fdef.out_cols.len() == bcols.len() {
                    fdef.out_cols
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t))| OutCol::new(super::expr::out_col_name(n, i), t.clone()))
                        .collect()
                } else if let DataType::Row(r) = &fdef.ret
                    && r.oid != 0
                    && r.fields.len() == bcols.len()
                    && !(bcols.len() == 1 && bcols[0].ty.row_oid() == Some(r.oid))
                {
                    // A named composite result: its columns (names and types).
                    r.fields
                        .iter()
                        .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                        .collect()
                } else if matches!(fdef.ret, DataType::Record | DataType::Row(_)) {
                    bcols.clone()
                } else {
                    let mut c = OutCol::new(fdef.name.clone(), fdef.ret.clone());
                    c.domain = fdef.ret_domain;
                    c.domain_array = b.domain_array(c.domain);
                    vec![c]
                };
                // A named composite result returned as one row value (`select row(r, i)`):
                // its fields are the columns, checked against them when it runs.
                if fdef.out_cols.is_empty()
                    && let DataType::Row(r) = &fdef.ret
                    && r.oid != 0
                    && r.fields.len() > 1
                    && let [one] = &bcols[..]
                    && (matches!(one.ty, DataType::Record)
                        || matches!(&one.ty, DataType::Row(x) if x.oid == 0 || x.oid == r.oid))
                {
                    expand = Some(
                        r.fields
                            .iter()
                            .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                            .collect(),
                    );
                    return Ok((body, bcols));
                }
                // OUT parameters returned as one row value (`select (1, 2.1)`): its fields
                // are the columns, checked against them when it runs.
                if fdef.out_cols.len() > 1
                    && let [one] = &bcols[..]
                    && (matches!(one.ty, DataType::Record)
                        || matches!(&one.ty, DataType::Row(r) if r.oid == 0))
                {
                    let outs: Vec<OutCol> = fdef
                        .out_cols
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t))| OutCol::new(super::expr::out_col_name(n, i), t.clone()))
                        .collect();
                    expand = Some(outs);
                    return Ok((body, bcols));
                }
            }
            let mismatch = |d: String| {
                Error::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "return type mismatch in function declared to return {}",
                        fdef.ret.display()
                    ),
                )
                .detail(d)
            };
            if bcols.len() != cols.len() {
                return Err(mismatch(format!(
                    "Final statement returns too {} columns.",
                    if bcols.len() > cols.len() {
                        "many"
                    } else {
                        "few"
                    }
                )));
            }
            let mut exprs = Vec::new();
            for (i, (c, bc)) in cols.iter().zip(&bcols).enumerate() {
                let e = if record {
                    b.coerce_implicit(Expr::Col(i, bc.ty.clone()), &c.ty)
                        .map_err(|_| {
                            mismatch(format!(
                                "Final statement returns {} instead of {} at column {}.",
                                bc.ty.display(),
                                c.ty.display(),
                                i + 1
                            ))
                        })?
                } else {
                    let e = b.coerce_assign(Expr::Col(i, bc.ty.clone()), &c.ty, &c.name)?;
                    b.result_domain(fdef, i, cols.len(), e)?
                };
                exprs.push(e);
            }
            Ok((
                Plan::Project {
                    input: Arc::new(body),
                    exprs,
                },
                cols,
            ))
        })?;
        let mut body = self.sql_fn_plan(fdef, at, body);
        if let Some(outs) = expand {
            let mut e = Expr::Col(0, cols[0].ty.clone());
            if let (Some(d), true) = (fdef.ret_domain, fdef.out_cols.is_empty()) {
                e = self.domain_check(e, d)?;
            }
            let exprs = self.checked_record_fields(e, &outs);
            body = Plan::Project {
                input: Arc::new(body),
                exprs,
            };
            cols = outs;
        }
        let n = cols.len();
        // Not SETOF: its first row, or nulls.
        if !fdef.returns_set {
            let first = Plan::Limit {
                input: Arc::new(body),
                limit: Some(Expr::Const(Value::Int8(1), DataType::Int8)),
                offset: None,
                with_ties: None,
            };
            body = Plan::Join {
                kind: JoinKind::Left,
                left: Arc::new(Plan::One),
                right: Arc::new(first),
                cond: None,
                hash: None,
                params: Vec::new(),
                nleft: 0,
                nright: n,
                meta: Default::default(),
            };
        }
        let ty = if let DataType::Row(r) = &fdef.ret
            && !record
            && n > 1
            && r.oid != 0
            && r.fields.len() == n
        {
            // A named composite result stays that type (its relation's columns are what a
            // stored query depends on).
            fdef.ret.clone()
        } else if n > 1 || record {
            DataType::row(
                cols.iter()
                    .map(|c| (c.name.clone(), c.ty.clone()))
                    .collect(),
            )
        } else {
            cols[0].ty.clone()
        };
        Ok((
            SrfCall {
                func: crate::sql::func::display_def(fdef),
                args,
                ty,
                ncols: n,
                body: Some(Arc::new(body)),
            },
            cols,
        ))
    }

    fn bind_join(
        &mut self,
        j: &pb::JoinExpr,
        locking: Option<&Locking>,
    ) -> Result<(Plan, Scope, Vec<Expr>)> {
        // The join is a range table entry of its own (RTE_JOIN), which the plan cache's
        // planning charge counts.
        self.hidden_rtes += 1;
        let jt = pb::JoinType::try_from(j.jointype).unwrap_or(pb::JoinType::JoinInner);
        let kind = match jt {
            pb::JoinType::JoinLeft => JoinKind::Left,
            pb::JoinType::JoinRight => JoinKind::Right,
            pb::JoinType::JoinFull => JoinKind::Full,
            _ => JoinKind::Inner,
        };
        // Bind both sides with only the left visible to the right (LATERAL), and the FROM
        // items left of the join (the enclosing FROM list's, or those an enclosing join's
        // right side sees) visible to the LATERAL items of both: the left side reads them
        // as the row the join is joined to, the right side through the join's parameters
        // (which pass them on from the parameters of the join the item is the right side
        // of).
        let saved = std::mem::take(&mut self.level().scope);
        let r = (|| -> Result<(Plan, Scope, Vec<Expr>)> {
            self.level().scope = saved.clone();
            let (lp, lscope, lparams) = self.bind_from_item(j.larg.as_deref().unwrap(), locking)?;
            let mut visible = lscope.clone();
            // (A RIGHT / FULL join's right side can't reference its left one.)
            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                for r in &mut visible.rels {
                    if r.lateral == 0 {
                        r.lateral = super::LAT_JOIN;
                    }
                }
            }
            // The items further left, each of their values a parameter of the join the
            // item is in (marked until the ones read are known).
            let mut outer_vals: Vec<Expr> = Vec::new();
            for r in &saved.rels {
                let mut r = r.clone();
                for c in &mut r.cols {
                    let ty = c.expr.ty();
                    outer_vals.push(std::mem::replace(&mut c.expr, Expr::Outer(1, OUTER_ITEM_MARK + outer_vals.len(), ty)));
                }
                // (Their row positions are the enclosing row's.)
                r.key_col = None;
                r.sys_col = None;
                visible.rels.push(r);
            }
            self.level().scope = visible;
            let (rp, rscope, rparams) = self.bind_from_item(j.rarg.as_deref().unwrap(), locking)?;
            // The right side's references to them: the item's parameters after the left
            // side's, renumbered; with them, the enclosing query's values it reads come
            // through the item's parameters too (the join the item is the right side of
            // runs it under its own).
            let reads_items = rparams.iter().any(|e| {
                let mut hit = false;
                e.walk(&mut |x| hit |= matches!(x, Expr::Outer(1, k, _) if *k >= OUTER_ITEM_MARK));
                hit
            });
            let (rparams, lparams) = if reads_items {
                let mut item_params = lparams;
                let slot = |v: Expr, item_params: &mut Vec<Expr>| -> usize {
                    match item_params.iter().position(|x| x.same_as(&v)) {
                        Some(i) => i,
                        None => {
                            item_params.push(v);
                            item_params.len() - 1
                        }
                    }
                };
                let rparams: Vec<Expr> = rparams
                    .into_iter()
                    .map(|e| {
                        e.transform(&mut |x| {
                            Ok(match x {
                                Expr::Outer(1, k, t) if k >= OUTER_ITEM_MARK => {
                                    let v = outer_vals[k - OUTER_ITEM_MARK].clone();
                                    Expr::Outer(1, slot(v, &mut item_params), t)
                                }
                                Expr::Outer(1, k, t) => Expr::Outer(1, slot(Expr::Outer(1, k, t.clone()), &mut item_params), t),
                                x => x,
                            })
                        })
                        .unwrap()
                    })
                    .collect();
                (rparams, item_params)
            } else {
                (rparams, lparams)
            };
            // (Only a reference to the left side's columns: an enclosing query's values
            // are fixed for the join.)
            if rparams.iter().any(|e| !e.columns().is_empty())
                && matches!(kind, JoinKind::Right | JoinKind::Full)
            {
                return Err(Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    "invalid reference to FROM-clause entry for table",
                )
                .detail("The combining JOIN type must be INNER or LEFT for a LATERAL reference."));
            }
            lscope.check_disjoint(&rscope)?;
            // A stored definition being recorded: what the join is made of (`refs`).
            let mut rec = self.refs.is_some().then(|| super::refs::JoinRec {
                members: (lscope.rels.iter().chain(&rscope.rels))
                    .filter_map(|r| r.src.filter(|&s| s >= 0))
                    .collect(),
                dangerous: kind == JoinKind::Full,
                ..Default::default()
            });
            // (Its index among the recorded joins: what its merged columns are known by.)
            let index = rec
                .as_ref()
                .and_then(|_| self.ref_log().map(|l| l.joins.len()));
            // A nullable side's virtual generated columns that wouldn't go NULL with its
            // row are computed below the join (the PlaceHolderVars Postgres wraps them in).
            let (lp, lscope) = if matches!(kind, JoinKind::Right | JoinKind::Full) {
                materialize_virtuals(lp, lscope)
            } else {
                (lp, lscope)
            };
            let (rp, rscope) = if matches!(kind, JoinKind::Left | JoinKind::Full) {
                materialize_virtuals(rp, rscope)
            } else {
                (rp, rscope)
            };
            // (So are its JOIN USING merged columns over a FROM subquery's computed output:
            // their own values, PlaceHolderVars when they need one.)
            let (lp, lscope) = if matches!(kind, JoinKind::Right | JoinKind::Full) {
                materialize_using(lp, lscope)
            } else {
                (lp, lscope)
            };
            let (rp, rscope) = if matches!(kind, JoinKind::Left | JoinKind::Full) {
                materialize_using(rp, rscope)
            } else {
                (rp, rscope)
            };
            let nleft = width(&lp);
            let nright = width(&rp);
            let rscope = rscope.shifted(nleft);
            let mut scope = Scope::default();
            // USING / NATURAL: merged columns come first.
            let using: Vec<String> = if j.is_natural {
                let lnames: Vec<String> = visible_names(&lscope);
                let rnames: Vec<String> = visible_names(&rscope);
                lnames.into_iter().filter(|n| rnames.contains(n)).collect()
            } else {
                names(&j.using_clause)
            };
            if self.deparse && j.is_natural {
                self.natural_using
                    .insert(j as *const pb::JoinExpr as usize, using.clone());
            }
            let mut conds = Vec::new();
            let (mut lscope, mut rscope) = (lscope, rscope);
            if !using.is_empty() {
                let mut merged = ScopeRel {
                    alias: String::new(),
                    anonymous: true,
                    ..Default::default()
                };
                for n in &using {
                    let lcol = find_unique_col(&lscope, n).ok_or_else(|| {
                        Error::new(code::UNDEFINED_COLUMN, format!("column \"{n}\" specified in USING clause does not exist in left table"))
                    })?;
                    let rcol = find_unique_col(&rscope, n).ok_or_else(|| {
                        Error::new(code::UNDEFINED_COLUMN, format!("column \"{n}\" specified in USING clause does not exist in right table"))
                    })?;
                    // Both sides' columns are read (markVarForSelectPriv).
                    self.note_col(lcol.table, lcol.attnum);
                    self.note_col(rcol.table, rcol.attnum);
                    // A stored definition being recorded: the columns a USING joins on keep
                    // their names (`refs`).
                    if let Some(log) = self.ref_log() {
                        for c in [&lcol, &rcol] {
                            if let Some(src) = c.src.filter(|&s| s >= 0) {
                                log.usings.push((src, c.table, c.attnum));
                            }
                        }
                    }
                    if let Some(r) = rec.as_mut() {
                        r.usings.push((n.clone(), (lcol.src, lcol.attnum), (rcol.src, rcol.attnum)));
                    }
                    let (lc, rc) = (lcol.expr, rcol.expr);
                    let eq = self.binop("=", lc.clone(), rc.clone(), j.rtindex)?;
                    // The sides' implicit collations conflicting, `=` has none: it fails
                    // when it compares (or the join hashes).
                    let eq = match self
                        .scope_col_collation(&lscope, &lc)
                        .merge(self.scope_col_collation(&rscope, &rc))
                    {
                        Ok(super::Coll::Conflict(..)) => super::indeterminate_eq(eq),
                        _ => eq,
                    };
                    conds.push(eq);
                    let (lt, rt) = (lc.ty(), rc.ty());
                    let ty = super::unify_types(&[lt.clone(), rt.clone()], "JOIN/USING")?;
                    // An inner join takes the column of a side that needs no coercion, the
                    // left's first (buildMergedJoinVar).
                    let right = match kind {
                        JoinKind::Right => true,
                        JoinKind::Inner => lt != ty && rt == ty,
                        _ => false,
                    };
                    // The side's column, coerced to the merged type when it isn't of it.
                    let value = match kind {
                        JoinKind::Full => {
                            let l = self.coerce_implicit(lc.clone(), &ty)?;
                            let r = self.coerce_implicit(rc.clone(), &ty)?;
                            Expr::Coalesce(vec![l, r], ty.clone())
                        }
                        _ if right && rt != ty => self.coerce_implicit(rc.clone(), &ty)?,
                        _ if right => rc.clone(),
                        _ if lt != ty => self.coerce_implicit(lc.clone(), &ty)?,
                        _ => lc.clone(),
                    };
                    // Not FULL: the column of the side it comes from, with its collation.
                    let collation = match kind {
                        JoinKind::Full => None,
                        _ if right => Some(self.scope_col_collation(&rscope, &rc)),
                        _ => Some(self.scope_col_collation(&lscope, &lc)),
                    };
                    if let Some(r) = rec.as_mut()
                        && !matches!(value, Expr::Col(..))
                    {
                        r.dangerous = true;
                    }
                    let i = merged.cols.len();
                    merged.cols.push(ScopeCol {
                        name: n.clone(),
                        expr: value,
                        hidden: false,
                        merged: false,
                        table: 0,
                        attnum: 0,
                        collation,
                        src: index.map(|j| super::refs::merged_src(j, i)),
                        rowtype_col: None,
                        domain: None,
                    });
                    mark_merged(&mut lscope, n);
                    mark_merged(&mut rscope, n);
                }
                if let Some(a) = &j.join_using_alias {
                    // checkNameSpaceConflicts: the alias is a relation name of the join's.
                    if lscope.rels.iter().chain(&rscope.rels).any(|r| {
                        !r.anonymous && r.schema.is_none() && r.alias == a.aliasname
                    }) {
                        return Err(Error::new(
                            code::DUPLICATE_ALIAS,
                            format!("table name \"{}\" specified more than once", a.aliasname),
                        ));
                    }
                    let mut named = merged.clone();
                    named.alias = a.aliasname.clone();
                    named.anonymous = false;
                    named.join_rte = true;
                    named.cols.iter_mut().for_each(|c| c.merged = true);
                    scope.rels.push(named);
                }
                scope.rels.push(merged);
            }
            let lstart = scope.rels.len();
            let rstart = lstart + lscope.rels.len();
            scope.extend(lscope);
            scope.extend(rscope);
            // ON condition over the combined row.
            if let Some(q) = j.quals.as_deref() {
                let inner_saved = std::mem::replace(&mut self.level().scope, scope.clone());
                // (The FROM items outside the join are in the range table but not its ON
                // clause's namespace: an invalid reference, not a missing one.)
                let n0 = self.unreachable.len();
                for r in &inner_saved.rels {
                    if !r.anonymous && !scope.rels.iter().any(|x| x.alias == r.alias) {
                        let mut r = r.clone();
                        r.lateral = 0;
                        self.unreachable.push(r);
                    }
                }
                let r = self.in_clause(Clause::JoinOn, |b| b.bind_expr(q));
                self.unreachable.truncate(n0);
                self.level().scope = inner_saved;
                let e = r?;
                let e = self.coerce_bool_at(e, "JOIN/ON", q)?;
                // (ON NULL alone stays NULL: the join's qual, not a relation's to prove
                // empty.)
                let e = self.simplify_where(e);
                let e = match (&e, j.quals.as_deref().and_then(inner)) {
                    (Expr::Const(Value::Bool(false), _), Some(NodeEnum::AConst(c))) if c.isnull => {
                        Expr::Const(Value::Null, DataType::Bool)
                    }
                    _ => e,
                };
                conds.push(e);
            }
            // Above the join, its nullable sides' rows may be null-extended.
            let nulled = match kind {
                JoinKind::Left => rstart..scope.rels.len(),
                JoinKind::Right => lstart..rstart,
                JoinKind::Full => lstart..scope.rels.len(),
                _ => 0..0,
            };
            for r in &mut scope.rels[nulled] {
                r.nullable = true;
            }
            for r in &mut scope.rels[lstart..] {
                r.in_join = true;
            }
            let cond = match conds.len() {
                0 => None,
                1 => conds.pop(),
                _ => Some(Expr::And(conds)),
            };
            // A join alias hides the inner names.
            if let Some(a) = &j.alias {
                if self.deparse {
                    self.join_inner
                        .insert(j as *const pb::JoinExpr as usize, scope.clone());
                }
                let cols: Vec<ScopeCol> = scope
                    .rels
                    .iter()
                    .flat_map(|r| r.cols.iter().filter(|c| !c.hidden && !c.merged).cloned())
                    .collect();
                let colnames = names(&a.colnames);
                if colnames.len() > cols.len() {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!(
                            "join expression \"{}\" has {} columns available but {} columns specified",
                            a.aliasname,
                            cols.len(),
                            colnames.len()
                        ),
                    ));
                }
                // A join with column aliases: what they stand for, so they can follow its
                // relations' columns.
                if let Some(r) = rec.as_mut() {
                    r.aliases = colnames.len();
                    r.cols = cols.iter().map(|c| (c.table, c.attnum)).collect();
                }
                // The relations it hides stay in the range table: referencing one is an
                // invalid reference, not a missing one.
                self.unreachable
                    .extend(scope.rels.iter().filter(|r| {
                        // (Not a JOIN USING alias: not a range table entry.)
                        !r.anonymous && !(!r.cols.is_empty() && r.cols.iter().all(|c| c.merged))
                    }).cloned());
                let mut rel = ScopeRel {
                    alias: a.aliasname.clone(),
                    cols,
                    join_rte: true,
                    ..Default::default()
                };
                for (c, n) in rel.cols.iter_mut().zip(colnames) {
                    c.name = n;
                    c.src = None;
                }
                scope = Scope { rels: vec![rel] };
            }
            if let Some(r) = rec.take()
                && let Some(log) = self.ref_log()
            {
                log.joins.push(r);
            }
            if !self.deparse && !rparams.is_empty() {
                let src = row_sources(&lp);
                self.note_lateral(&rparams, &src);
            }
            // distribute_qual_to_rels: the ON clause's join clauses add their columns to
            // the relations' reltargets (an outer join's clause reading its nonnullable side
            // too: its relids are the join's ojscope; one of the nullable side alone is
            // degenerate, pushed down).
            if !self.deparse
                && let Some(c) = &cond
            {
                let mut src = row_sources(&lp);
                src.resize(nleft, None);
                src.extend(row_sources(&rp));
                let mut parts = Vec::new();
                optimize::conjuncts(c.clone(), &mut parts);
                for e in &parts {
                    let cols = e.columns();
                    let ojscope = match kind {
                        JoinKind::Left => cols.iter().any(|&c| c < nleft),
                        JoinKind::Right => cols.iter().any(|&c| c >= nleft),
                        JoinKind::Full => true,
                        _ => false,
                    };
                    self.note_join_clause(&cols, &src, ojscope);
                }
            }
            // pull_up_sublinks in a left join's ON clause: an EXISTS / IN reading only the
            // right side joins that side.
            let (rp, cond) = match cond {
                Some(c)
                    if kind == JoinKind::Left
                        && !self.deparse
                        && locking.is_none()
                        && rparams.is_empty() =>
                {
                    let mut parts = Vec::new();
                    optimize::conjuncts(c, &mut parts);
                    let (subs, stay): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| {
                        let sub = match e {
                            Expr::Subquery(_) => true,
                            Expr::Not(x) => matches!(&**x, Expr::Subquery(_)),
                            _ => false,
                        };
                        let cols = e.columns();
                        sub && !cols.is_empty() && cols.iter().all(|&c| c >= nleft)
                    });
                    let and_all = |mut v: Vec<Expr>| match v.len() {
                        0 => None,
                        1 => v.pop(),
                        _ => Some(Expr::And(v)),
                    };
                    if subs.is_empty() {
                        (rp, and_all(stay))
                    } else {
                        let pred = Expr::And(
                            subs.into_iter()
                                .map(|e| e.shift_cols(-(nleft as isize)))
                                .collect(),
                        );
                        let (rp, rest) = super::sublink::pull_up_only(&self.cat, rp, pred);
                        let mut stay = stay;
                        stay.extend(rest.map(|e| e.shift_cols(nleft as isize)));
                        (rp, and_all(stay))
                    }
                }
                c => (rp, c),
            };
            let on_written = match (&cond, kind) {
                (Some(c), JoinKind::Inner) if !self.deparse => {
                    let mut parts = Vec::new();
                    optimize::conjuncts(c.clone(), &mut parts);
                    optimize::written_equalities(&parts)
                }
                _ => Vec::new(),
            };
            // (An inner join's right side, a join reading its left side's values:
            // reassociated, the ON clause checked over the whole.)
            let plan = if kind == JoinKind::Inner
                && matches!(rp, Plan::Join { .. })
                && matches!(j.rarg.as_deref().and_then(inner), Some(NodeEnum::JoinExpr(_)))
                && !rparams.is_empty()
                && !self.deparse
            {
                match reassociate_lateral(lp, rp, rparams) {
                    Ok(j) => match cond {
                        Some(c) => optimize::add_filter(&self.cat, j, c),
                        None => j,
                    },
                    Err(b) => {
                        let (lp, rp, rparams) = *b;
                        optimize::join(&self.cat, kind, lp, rp, cond, rparams, nleft, nright)
                    }
                }
            } else if kind == JoinKind::Inner
                && !self.deparse
                && locking.is_none()
                && rparams.is_empty()
                && let Some(c) = cond.clone()
            {
                // pull_up_sublinks in an inner join's ON clause: its EXISTS / IN sublinks
                // join the join (when all of them can).
                let mut parts = Vec::new();
                optimize::conjuncts(c, &mut parts);
                let (subs, stay): (Vec<Expr>, Vec<Expr>) = parts.into_iter().partition(|e| match e {
                    Expr::Subquery(_) => true,
                    Expr::Not(x) => matches!(&**x, Expr::Subquery(_)),
                    _ => false,
                });
                let stay_cond = match stay.len() {
                    0 => None,
                    1 => stay.into_iter().next(),
                    _ => Some(Expr::And(stay)),
                };
                let pulled = (!subs.is_empty())
                    .then(|| {
                        let j = optimize::join(&self.cat, kind, lp.clone(), rp.clone(), stay_cond, Vec::new(), nleft, nright);
                        let (p, rest) = super::sublink::pull_up(&self.cat, self.ctx, j, Expr::And(subs), &self.unplanned_subs);
                        rest.is_none().then_some(p)
                    })
                    .flatten();
                match pulled {
                    Some(p) => p,
                    None => optimize::join(&self.cat, kind, lp, rp, cond, rparams, nleft, nright),
                }
            } else {
                optimize::join(&self.cat, kind, lp, rp, cond, rparams, nleft, nright)
            };
            // (JOIN syntax: its sides are one join problem only within join_collapse_limit.)
            let mut plan = plan;
            if kind == JoinKind::Inner
                && let Plan::Join { kind: JoinKind::Inner, meta, nleft: nl, .. } = &mut plan
                && *nl == nleft
            {
                meta.explicit = true;
                meta.ec_written = on_written;
            }
            if !self.deparse {
                self.note_semi_joins(&plan);
            }
            Ok((plan, scope, lparams))
        })();
        self.level().scope = saved;
        r
    }
}

impl Binder<'_, '_> {
    /// add_vars_to_targetlist for a join clause (distribute_qual_to_rels: a clause of
    /// several relations, or one an outer join's `ojscope` holds): the base relation columns
    /// it reads (`cols`, of a row whose columns are `src`, in the order pull_var_clause meets
    /// them) join their scans' reltargets.
    fn note_join_clause(&mut self, cols: &[usize], src: &[Option<(crate::sql::plan::Rti, usize)>], ojscope: bool) {
        let cols: Vec<(crate::sql::plan::Rti, usize)> =
            cols.iter().filter_map(|&c| src.get(c).copied().flatten()).collect();
        let mut rels: Vec<usize> = cols.iter().map(|(r, _)| r.0).collect();
        rels.sort_unstable();
        rels.dedup();
        if rels.is_empty() || !(ojscope || rels.len() > 1) {
            return;
        }
        for (r, c) in cols {
            match self.rel_vars.iter_mut().find(|(x, _)| *x == r) {
                Some((_, v)) => {
                    if !v.contains(&c) {
                        v.push(c);
                    }
                }
                None => self.rel_vars.push((r, vec![c])),
            }
        }
    }

    /// find_lateral_references: the columns a LATERAL item reads (`params`, over a row whose
    /// columns are `src`) come into their scans' reltargets before any clause's.
    fn note_lateral(&mut self, params: &[Expr], src: &[Option<(crate::sql::plan::Rti, usize)>]) {
        for e in params {
            for c in e.columns() {
                let Some((r, c)) = src.get(c).copied().flatten() else { continue };
                match self.lateral_vars.iter_mut().find(|(x, _)| *x == r) {
                    Some((_, v)) => {
                        if !v.contains(&c) {
                            v.push(c);
                        }
                    }
                    None => self.lateral_vars.push((r, vec![c])),
                }
            }
        }
    }

    /// The semi / anti joins pull_up_sublinks made under `p` (and any before): their
    /// clauses are join clauses of both sides (an anti join's, its ojscope), met after the
    /// FROM list they wrap; an equality a class with a constant replaced still counts.
    fn note_semi_joins(&mut self, p: &Plan) {
        match p {
            Plan::Join { kind, left, right, cond, hash, nleft, meta, .. } => {
                self.note_semi_joins(left);
                self.note_semi_joins(right);
                if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    return;
                }
                let mut src = row_sources(left);
                src.resize(*nleft, None);
                src.extend(row_sources(right));
                // (Each clause's columns: the hash keys', the other quals', those of the
                // equalities a class with a constant replaced.)
                let mut clauses: Vec<Vec<usize>> = Vec::new();
                if let Some((lk, rk)) = hash {
                    for (a, b) in lk.iter().zip(rk) {
                        let mut cols = a.columns();
                        cols.extend(b.columns().into_iter().map(|c| c + nleft));
                        clauses.push(cols);
                    }
                }
                if let Some(c) = cond {
                    let mut parts = Vec::new();
                    optimize::conjuncts(c.clone(), &mut parts);
                    clauses.extend(parts.iter().map(Expr::columns));
                }
                if !meta.kept.is_empty() {
                    clauses.push(meta.kept.clone());
                }
                // (An anti join's clause reading its left side has the join's ojscope; a semi
                // join's clauses are an inner join's.)
                for cols in &clauses {
                    let ojscope = *kind == JoinKind::Anti && cols.iter().any(|&c| c < *nleft);
                    self.note_join_clause(cols, &src, ojscope);
                }
            }
            Plan::Filter { input, .. } | Plan::Project { input, .. } => self.note_semi_joins(input),
            _ => {}
        }
    }

    /// Put each scan's LATERAL and join clause columns so far on it (`ScanPlan::reltarget`).
    fn apply_rel_vars(&mut self, p: &mut Plan) {
        fn go(p: &mut Plan, vars: &[(crate::sql::plan::Rti, Vec<usize>)]) {
            if let Plan::Scan(s) = p {
                if let Some((_, v)) = vars.iter().find(|(r, _)| *r == s.rti && r.0 != 0)
                    && s.reltarget.0.as_slice() != v.as_slice()
                {
                    s.reltarget = crate::sql::plan::RelVars(Arc::new(v.clone()));
                }
                return;
            }
            for c in crate::sql::explain::plan_inputs_mut(p) {
                go(Arc::make_mut(c), vars);
            }
        }
        if self.rel_vars.is_empty() && self.lateral_vars.is_empty() {
            return;
        }
        let mut all = self.lateral_vars.clone();
        for (r, v) in &self.rel_vars {
            match all.iter_mut().find(|(x, _)| x == r) {
                Some((_, w)) => {
                    for c in v {
                        if !w.contains(c) {
                            w.push(*c);
                        }
                    }
                }
                None => all.push((*r, v.clone())),
            }
        }
        go(p, &all);
        // (A FROM subquery's join tree kept as bound for this level's pull-up: its scans
        // are this level's relations too.)
        for b in self.ctx.bound_trees.borrow_mut().iter_mut() {
            go(Arc::make_mut(&mut b.tree), &all);
        }
    }
}

/// The base relation column (its scan's range table index and column) each column of a
/// FROM item's row plainly is.
fn row_sources(p: &Plan) -> Vec<Option<(crate::sql::plan::Rti, usize)>> {
    let w = width(p);
    let mut v = match p {
        Plan::Scan(s) if s.rti.0 != 0 => (0..w).map(|i| (i < s.table.columns.len()).then_some((s.rti, i))).collect(),
        Plan::Filter { input, .. } => row_sources(input),
        Plan::Project { input, exprs } => {
            let inner = row_sources(input);
            exprs
                .iter()
                .map(|e| match e {
                    Expr::Col(j, _) => inner.get(*j).copied().flatten(),
                    _ => None,
                })
                .collect()
        }
        Plan::Join { kind, left, right, nleft, .. } => {
            let mut v = row_sources(left);
            v.resize(*nleft, None);
            if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                v.extend(row_sources(right));
            }
            v
        }
        _ => Vec::new(),
    };
    v.resize(w, None);
    v
}

/// Whether the plan of a FROM item reads any value of its query level's outer row (the
/// parameters of the join it is the right side of).
fn reads_level_outer(p: &Plan) -> bool {
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
    crate::sql::explain::plan_inputs(p).into_iter().any(|c| reads_level_outer(c))
}

/// `left` joined to a join item `L ⋈ R` (inner or left) whose LATERAL references read
/// `left`'s values (`params`, over `left`'s row, which the item reads as `Outer(1, k)`):
/// reassociated, `(left ⨯ L) ⋈ R`, its right side reading `left`'s values directly, so no
/// join runs inside another's parameterized side (Postgres's join tree is flat there).
/// The item as it is (`Err`) when it isn't such a join.
fn reassociate_lateral(left: Plan, item: Plan, params: Vec<Expr>) -> std::result::Result<Plan, Box<(Plan, Plan, Vec<Expr>)>> {
    let Plan::Join {
        kind: kind @ (JoinKind::Inner | JoinKind::Left),
        left: l,
        right: r,
        cond,
        hash,
        params: rparams,
        nleft: nl,
        nright: nr,
        meta,
    } = item
    else {
        return Err(Box::new((left, item, params)));
    };
    if meta.from_call.is_some() || params.is_empty() {
        return Err(Box::new((
            left,
            Plan::Join { kind, left: l, right: r, cond, hash, params: rparams, nleft: nl, nright: nr, meta },
            params,
        )));
    }
    let na = width(&left);
    let lp = if reads_level_outer(&l) { params.clone() } else { Vec::new() };
    let inner = Plan::Join {
        kind: JoinKind::Inner,
        left: Arc::new(left),
        right: l,
        cond: None,
        hash: None,
        params: lp,
        nleft: na,
        nright: nl,
        meta: Default::default(),
    };
    let lift = |e: Expr| -> Expr {
        e.transform(&mut |x| {
            Ok(match x {
                Expr::Col(i, t) => Expr::Col(na + i, t),
                Expr::Outer(1, k, t) => params.get(k).cloned().unwrap_or(Expr::Outer(1, k, t)),
                x => x,
            })
        })
        .unwrap()
    };
    let rparams: Vec<Expr> = rparams.into_iter().map(lift).collect();
    let cond = cond.map(|c| c.shift_cols(na as isize));
    let hash = hash.map(|(lk, rk)| (lk.into_iter().map(|e| e.shift_cols(na as isize)).collect(), rk));
    let mut meta = meta;
    meta.kept = meta.kept.iter().map(|c| c + na).collect();
    meta.pushed = meta.pushed.iter().map(|c| c + na).collect();
    Ok(Plan::Join {
        kind,
        left: Arc::new(inner),
        right: r,
        cond,
        hash,
        params: rparams,
        nleft: na + nl,
        nright: nr,
        meta,
    })
}

/// A FROM item left of a join, as its right side's LATERAL references read it before the
/// join's parameters are known: `Outer(1, OUTER_ITEM_MARK + i)`, value `i` of those items.
const OUTER_ITEM_MARK: usize = 1 << 40;

fn visible_names(s: &Scope) -> Vec<String> {
    s.rels
        .iter()
        .flat_map(|r| {
            r.cols
                .iter()
                .filter(|c| !c.hidden && !c.merged)
                .map(|c| c.name.clone())
        })
        .collect()
}

/// A star's columns as references that name the same columns (`qualifier…, column`), for
/// writing it out (`refs`); None when some can't be named unambiguously. `quals`: the star's
/// qualifier, else each column is qualified by its relation's name.
fn star_items(rels: &[ScopeRel], quals: Option<&[String]>) -> Option<Vec<(Vec<String>, String)>> {
    let rels: Vec<&ScopeRel> = rels
        .iter()
        .filter(|r| quals.is_some() || r.exists_col.is_none() && !r.qualified_only)
        .collect();
    let mut out = Vec::new();
    for r in &rels {
        let visible = |c: &&ScopeCol| !c.hidden && (quals.is_some() || !c.merged);
        for c in r.cols.iter().filter(visible) {
            if r.cols.iter().filter(|x| !x.hidden && x.name == c.name).count() > 1 {
                return None;
            }
            let q = match quals {
                Some(q) => q.to_vec(),
                None if r.anonymous => {
                    // Merged USING columns go unqualified: the name must be theirs alone.
                    let same = (rels.iter())
                        .flat_map(|x| x.cols.iter())
                        .filter(|x| !x.hidden && !x.merged && x.name == c.name)
                        .count();
                    if same != 1 {
                        return None;
                    }
                    Vec::new()
                }
                None => {
                    let dup = (rels.iter())
                        .filter(|x| !x.anonymous && x.alias == r.alias)
                        .count()
                        > 1;
                    match (dup, &r.schema) {
                        (false, _) => vec![r.alias.clone()],
                        (true, Some(s)) => vec![s.clone(), r.alias.clone()],
                        (true, None) => return None,
                    }
                }
            };
            if q.iter().any(String::is_empty) {
                return None;
            }
            out.push((q, c.name.clone()));
        }
    }
    Some(out)
}

fn find_unique_col(s: &Scope, name: &str) -> Option<ScopeCol> {
    let found: Vec<&ScopeCol> = s
        .rels
        .iter()
        .flat_map(|r| {
            r.cols
                .iter()
                .filter(|c| !c.hidden && !c.merged && c.name == name)
        })
        .collect();
    if found.len() == 1 {
        Some(found[0].clone())
    } else {
        None
    }
}

fn mark_merged(s: &mut Scope, name: &str) {
    for r in &mut s.rels {
        for c in &mut r.cols {
            if c.name == name && !c.hidden {
                c.merged = true;
            }
        }
    }
}

/// A non-set expression used as a FROM item: one row.
/// Set-returning calls as ProjectSet stages: a call in another's arguments runs a stage
/// earlier. Each stage passes its input through, plus its calls' values. Returns the plan,
/// where each call's value ends up, and the last stage's calls (left for the caller's own
/// ProjectSet, unless `all`).
fn srf_stages(
    mut plan: Plan,
    srfs: Vec<SrfCall>,
    all: bool,
) -> Result<(Plan, Vec<usize>, Vec<SrfCall>)> {
    let mut depth = vec![1usize; srfs.len()];
    for i in 0..srfs.len() {
        let mut d = 1;
        for a in &srfs[i].args {
            a.walk(&mut |e| {
                if let Expr::Srf(j, _) = e {
                    d = d.max(depth[*j] + 1);
                }
            });
        }
        depth[i] = d;
    }
    let stages = depth.iter().copied().max().unwrap_or(1);
    let mut pos = vec![0usize; srfs.len()];
    let mut w = width(&plan);
    let mut last = Vec::new();
    for k in 1..=stages {
        let ids: Vec<usize> = (0..srfs.len()).filter(|i| depth[*i] == k).collect();
        let mut calls = Vec::new();
        for (m, &i) in ids.iter().enumerate() {
            let mut c = srfs[i].clone();
            let p = &pos;
            c.args = std::mem::take(&mut c.args)
                .into_iter()
                .map(|e| {
                    replace_placeholders(e, &|x| {
                        if let Expr::Srf(j, t) = x {
                            Some(Expr::Col(p[*j], t.clone()))
                        } else {
                            None
                        }
                    })
                })
                .collect::<Result<_>>()?;
            calls.push(c);
            pos[i] = w + m;
        }
        if k < stages || all {
            let mut exprs: Vec<Expr> = (0..w).map(|c| Expr::Col(c, DataType::Unknown)).collect();
            exprs.extend(
                calls
                    .iter()
                    .enumerate()
                    .map(|(m, c)| Expr::Col(w + m, c.ty.clone())),
            );
            w += calls.len();
            plan = Plan::ProjectSet {
                input: Arc::new(plan),
                exprs,
                srfs: calls,
            };
        } else {
            last = calls;
        }
    }
    Ok((plan, pos, last))
}

fn const_srf(e: Expr) -> SrfCall {
    let ty = e.ty();
    let def = Arc::new(FuncDef {
        oid: 0,
        name: "?column?",
        args: vec![ty.clone()],
        variadic: false,
        spread: false,
        ret: crate::sql::func::Ret::Fixed(ty.clone()),
        strict: false,
        volatility: 'v',
        imp: |a, _| Ok(a[0].clone()),
        min_args: 1,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: vec![],
        coll: None,
    });
    SrfCall {
        func: def,
        args: vec![e],
        ty,
        ncols: 1,
        body: None,
    }
}

/// The function `record_checked` is, from a record of type `from` to `to`.
fn record_check_def(from: DataType, to: DataType) -> Arc<FuncDef> {
    Arc::new(FuncDef {
        oid: 0,
        name: "?column?",
        args: vec![from, DataType::Text],
        variadic: false,
        spread: false,
        ret: crate::sql::func::Ret::Fixed(to),
        strict: false,
        volatility: 'v',
        imp: record_checked,
        min_args: 2,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: vec![],
        coll: None,
    })
}

/// The row a record-valued FROM expression returns, checked against its column definition
/// list (`tupledesc_match`): the second argument is the mismatch found when it was bound
/// (raised when the row is computed), the result its fields.
fn record_checked(a: &[Value], c: &crate::sql::func::FnCtx) -> Result<Value> {
    if let Value::Text(d) = &a[1] {
        return Err(Error::new(
            code::DATATYPE_MISMATCH,
            "function return row and query-specified return row do not match",
        )
        .detail(d.to_string()));
    }
    let n = match c.ret {
        DataType::Row(r) => r.fields.len(),
        _ => 1,
    };
    let fields = match &a[0] {
        Value::Record(f) => f.as_ref().clone(),
        _ => vec![Value::Null; n],
    };
    Ok(if n == 1 {
        fields.into_iter().next().unwrap_or(Value::Null)
    } else {
        Value::Record(Arc::new(fields))
    })
}

/// Whether a query mentions a relation name (to detect recursive CTEs).
fn references_name(n: &Node, name: &str) -> bool {
    // (Over the tree as JSON: a nested WITH defining the name hides it below it.)
    serde_json::to_value(n).is_ok_and(|v| json_references(&v, name))
}

/// Whether parse tree `v` (as JSON) reads relation `name` unqualified, where a CTE of that
/// name defined by an inner WITH isn't it: hidden in that WITH's query, in the CTEs after it
/// (and everywhere in a RECURSIVE one).
fn json_references(v: &serde_json::Value, name: &str) -> bool {
    use serde_json::Value as J;
    match v {
        J::Array(a) => a.iter().any(|x| json_references(x, name)),
        J::Object(o) => {
            if let Some(J::Object(rv)) = o.get("RangeVar") {
                return rv.get("relname").and_then(J::as_str) == Some(name)
                    && rv.get("schemaname").and_then(J::as_str).is_none_or(str::is_empty);
            }
            for stmt in ["SelectStmt", "InsertStmt", "UpdateStmt", "DeleteStmt", "MergeStmt"] {
                let Some(J::Object(st)) = o.get(stmt) else {
                    continue;
                };
                let Some(J::Object(w)) = st.get("with_clause") else {
                    break;
                };
                let ctes: Vec<&J> = w
                    .get("ctes")
                    .and_then(J::as_array)
                    .map(|a| a.iter().collect())
                    .unwrap_or_default();
                let cte_name = |c: &J| {
                    c.pointer("/node/CommonTableExpr/ctename")
                        .and_then(J::as_str)
                        .map(str::to_string)
                };
                let Some(k) = ctes.iter().position(|c| cte_name(c).as_deref() == Some(name))
                else {
                    break;
                };
                if w.get("recursive").and_then(J::as_bool) == Some(true) {
                    return false;
                }
                return ctes[..=k].iter().any(|c| json_references(c, name));
            }
            o.values().any(|x| json_references(x, name))
        }
        _ => false,
    }
}

/// A node's debug text without its `location`s: two nodes written alike compare equal.
pub(super) fn strip_locations(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("location: ") {
        out.push_str(&rest[..i + 10]);
        rest = rest[i + 10..].trim_start_matches(|c: char| c.is_ascii_digit() || c == '-');
    }
    out.push_str(rest);
    out
}

/// Point an ungrouped-column error at the first mention of that column, checking the select
/// list, then HAVING, then ORDER BY (the order Postgres checks them in).
fn ungrouped_pos(e: Error, s: &pb::SelectStmt) -> Error {
    if e.code != code::GROUPING_ERROR || e.position.is_some() {
        return e;
    }
    let Some(name) = e.message.split('"').nth(1).map(str::to_string) else {
        return e;
    };
    let first = |nodes: &[&Node]| -> Option<i32> {
        let mut best: Option<i32> = None;
        for n in nodes {
            let Some(ne) = n.node.as_ref() else { continue };
            for (node, _, _, _) in ne.nodes() {
                if let pg_query::NodeRef::ColumnRef(c) = node {
                    let f = names(&c.fields);
                    if (f.join(".") == name
                        || (f.len() == 1
                            && name.rsplit('.').next() == f.last().map(String::as_str)))
                        && c.location >= 0
                    {
                        best = Some(best.map_or(c.location, |b| b.min(c.location)));
                    }
                }
            }
        }
        best
    };
    let tl: Vec<&Node> = s.target_list.iter().collect();
    let hv: Vec<&Node> = s.having_clause.as_deref().into_iter().collect();
    let ob: Vec<&Node> = s.sort_clause.iter().collect();
    match first(&tl).or_else(|| first(&hv)).or_else(|| first(&ob)) {
        Some(l) => with_pos(e, l),
        None => e,
    }
}

impl Binder<'_, '_> {
    /// Ungrouped plain columns of `exprs` whose table's primary key columns are all in
    /// `groups` (so each group has one value of them).
    /// `groups`: the grouping columns a primary key must be among; `all`: every grouping
    /// expression (columns among them need nothing).
    fn functionally_dependent(&mut self, groups: &[Expr], all: &[Expr], exprs: &[&Expr]) -> Vec<Expr> {
        let scope = self.levels.last().map(|l| &l.scope);
        let Some(scope) = scope else {
            return Vec::new();
        };
        let mut cols: Vec<Expr> = Vec::new();
        for e in exprs {
            e.walk(&mut |x| {
                if let Expr::Col(..) = x
                    && !all.iter().any(|g| g.same_as(x))
                    && !cols.iter().any(|c| c.same_as(x))
                {
                    cols.push(x.clone());
                }
            });
        }
        let mut out = Vec::new();
        for c in cols {
            let Expr::Col(i, _) = &c else { continue };
            let found = scope.rels.iter().find_map(|r| {
                r.cols
                    .iter()
                    .find(|sc| matches!(&sc.expr, Expr::Col(x, _) if x == i) && sc.table != 0)
                    .map(|sc| (r, sc.table))
            });
            let Some((rel, oid)) = found else { continue };
            let Some(t) = self.cat.table(oid) else { continue };
            let Some(pk) = &t.pkey else { continue };
            // (get_primary_key_attnos with deferrableOk false)
            if t.constraints.iter().any(|c| {
                matches!(c.kind, crate::catalog::ConstraintKind::PrimaryKey { .. }) && c.deferrable
            }) {
                continue;
            }
            let grouped = pk.iter().all(|p| {
                rel.cols.iter().any(|sc| {
                    sc.table == oid
                        && sc.attnum == *p as i16 + 1
                        && groups.iter().any(|g| g.same_as(&sc.expr))
                })
            });
            if grouped {
                out.push(c);
                // A view relying on this depends on the primary key constraint.
                if let Some(con) = t.constraints.iter().find(|c| {
                    matches!(c.kind, crate::catalog::ConstraintKind::PrimaryKey { .. })
                }) && let Some(d) = self.deps.as_mut()
                    && !d.contains(&(crate::catalog::depend::CONSTRAINT, con.oid))
                {
                    d.push((crate::catalog::depend::CONSTRAINT, con.oid));
                }
            }
        }
        out
    }
}

/// Rewrite an expression from FROM-row terms to the aggregation output row.
/// A constant as written: a literal, perhaps cast (its bound form is what was written, not
/// a folded expression).
fn literal_node(n: &Node) -> bool {
    match inner(n) {
        Some(NodeEnum::AConst(_)) => true,
        Some(NodeEnum::TypeCast(c)) => c.arg.as_deref().is_some_and(literal_node),
        _ => false,
    }
}

/// The grouping-set queries' pass-through values (`post_agg`): an expression equal to a
/// grouping key written as a constant, other than the select-list item the key names, is no
/// grouping column but a value of the group's input rows (Postgres computes it below the
/// Agg and reads it from the group's representative row: NULL in the empty grouping set's
/// row over no input rows). Each occurrence is a value of its own, a column past the keys.
struct PassThrough<'a> {
    literal: &'a [bool],
    /// Keys written as expressions: their text, and the text of the item being mapped.
    texts: &'a [Option<String>],
    item: std::cell::RefCell<Option<String>>,
    found: std::cell::RefCell<Vec<Expr>>,
}

/// Where `post_agg` numbers a pass-through value before the columns are known.
const PASS_BASE: usize = 1 << 40;

fn post_agg(
    e: Expr,
    groups: &[Expr],
    ng: usize,
    mask_col: Option<usize>,
    scope: &Scope,
    outer_keys: bool,
) -> Result<Expr> {
    post_agg_with(e, groups, ng, mask_col, scope, outer_keys, None)
}

/// `post_agg`, with the pass-through values of a grouping-set query collected in `pass`.
fn post_agg_with(
    e: Expr,
    groups: &[Expr],
    ng: usize,
    mask_col: Option<usize>,
    scope: &Scope,
    outer_keys: bool,
    pass: Option<&PassThrough>,
) -> Result<Expr> {
    // (Constants are always acceptable: never a grouped column, so never nulled by a
    // grouping set either. Nor is anything equal to a key without the level's columns,
    // volatile functions or (`outer_keys`: in the select list) outer references: as the
    // reference plans it, only the select-list item the key names reads such a key.)
    let keyed = |g: &Expr| {
        !g.columns().is_empty() || g.is_volatile() || (outer_keys && g.has_outer_refs())
    };
    // (A key's occurrence kept from folding while bound (`SelectState::pass_marks`): its
    // expression, a pass-through value below when it is one.)
    let e = match e {
        Expr::SqlSyntax(a) if matches!(&*a, Expr::SqlSyntax(_)) => match *a {
            Expr::SqlSyntax(b) => *b,
            a => a,
        },
        e => e,
    };
    // (A constant key's other occurrences: values of the group's rows.)
    if let Some(pt) = pass
        && let Some(k) = groups
            .iter()
            .position(|g| !keyed(g) && !g.has_outer_refs() && g.same_as(&e))
        && (pt.literal.get(k) == Some(&true)
            || pt.texts.get(k).cloned().flatten().is_some_and(|t| {
                pt.item.borrow().as_ref().is_some_and(|it| it.contains(t.as_str()))
            }))
    {
        let mut f = pt.found.borrow_mut();
        f.push(e.clone());
        return Ok(Expr::Col(PASS_BASE + f.len() - 1, e.ty()));
    }
    if !matches!(e, Expr::Const(..))
        && let Some(k) = groups.iter().position(|g| keyed(g) && g.same_query_as(&e))
    {
        return Ok(Expr::Col(k, e.ty()));
    }
    match e {
        Expr::Agg(j, t) => Ok(Expr::Col(ng + j, t)),
        // (Without grouping sets every key is grouped in the one set: 0. A GroupingFunc
        // all the same, over the keys, as Postgres keeps it: not a constant.)
        Expr::Grouping(pos) => Ok(match mask_col {
            Some(m) => grouping_expr(m, ng, &pos),
            None => grouping_of_keys(groups, &pos),
        }),
        Expr::Col(i, t) => {
            let name = scope
                .rels
                .iter()
                .find_map(|r| {
                    r.cols
                        .iter()
                        .find(|c| matches!(&c.expr, Expr::Col(x, _) if *x == i))
                        .map(|c| {
                            if r.alias.is_empty() {
                                c.name.clone()
                            } else {
                                format!("{}.{}", r.alias, c.name)
                            }
                        })
                })
                .unwrap_or_else(|| format!("#{i}"));
            let _ = t;
            Err(Error::new(
                code::GROUPING_ERROR,
                format!(
                    "column \"{name}\" must appear in the GROUP BY clause or be used in an aggregate function"
                ),
            ))
        }
        // A sub-select's outer references to ungrouped columns (check_ungrouped_columns at
        // sublevels_up > 0): its own message. Its IN / ANY left operands are this level's.
        Expr::Subquery(mut sq) => {
            let tx = |c: Expr| post_agg_with(c, groups, ng, mask_col, scope, outer_keys, pass);
            sq.left = std::mem::take(&mut sq.left).into_iter().map(tx).collect::<Result<_>>()?;
            sq.params = std::mem::take(&mut sq.params)
                .into_iter()
                .map(|c| {
                    tx(c).map_err(|mut e| {
                        if e.code == code::GROUPING_ERROR
                            && let Some(rest) = e.message.strip_prefix("column \"")
                            && let Some((name, _)) = rest.split_once("\" must appear")
                        {
                            e.message = format!("subquery uses ungrouped column \"{name}\" from outer query");
                        }
                        e
                    })
                })
                .collect::<Result<_>>()?;
            Ok(Expr::Subquery(sq))
        }
        e => e.map_children(&mut |c| post_agg_with(c, groups, ng, mask_col, scope, outer_keys, pass)),
    }
}

fn tx_frame(
    f: crate::sql::plan::Frame,
    tx: &dyn Fn(Expr) -> Result<Expr>,
) -> Result<crate::sql::plan::Frame> {
    use crate::sql::plan::FrameBound as B;
    let b = |x: B| -> Result<B> {
        Ok(match x {
            B::Preceding(e) => B::Preceding(tx(e)?),
            B::Following(e) => B::Following(tx(e)?),
            x => x,
        })
    };
    Ok(crate::sql::plan::Frame {
        unit: f.unit,
        start: b(f.start)?,
        end: b(f.end)?,
        exclude: f.exclude,
        between: f.between,
    })
}

/// GROUPING(a, b): bits of the mask column for the given group positions.
fn grouping_expr(mask_col: usize, ng: usize, pos: &[usize]) -> Expr {
    let mut args = vec![
        Expr::Col(mask_col, DataType::Int8),
        Expr::Const(Value::Int4(ng as i32), DataType::Int4),
    ];
    args.extend(
        pos.iter()
            .map(|p| Expr::Const(Value::Int4(*p as i32), DataType::Int4)),
    );
    let def = Arc::new(FuncDef {
        oid: 0,
        name: "grouping",
        args: vec![],
        variadic: false,
        spread: false,
        ret: crate::sql::func::Ret::Fixed(DataType::Int4),
        strict: false,
        volatility: 'i',
        imp: |a, _| {
            let mask = a[0].as_i64().unwrap_or(0);
            let ng = a[1].as_i64().unwrap_or(0);
            let mut out = 0i64;
            for p in &a[2..] {
                let p = p.as_i64().unwrap_or(0);
                out = (out << 1) | ((mask >> (ng - 1 - p)) & 1);
            }
            Ok(Value::Int4(out as i32))
        },
        min_args: 0,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: vec![],
        coll: None,
    });
    Expr::Call(def, args, DataType::Int4)
}

/// GROUPING(a, b) without grouping sets: over the keys (their columns of the aggregation's
/// row), always 0.
fn grouping_of_keys(groups: &[Expr], pos: &[usize]) -> Expr {
    let args: Vec<Expr> = pos.iter().map(|&p| Expr::Col(p, groups.get(p).map_or(DataType::Unknown, |g| g.ty()))).collect();
    let def = Arc::new(FuncDef {
        oid: 0,
        name: "GROUPING",
        args: vec![],
        variadic: false,
        spread: false,
        ret: crate::sql::func::Ret::Fixed(DataType::Int4),
        strict: false,
        volatility: 'i',
        imp: |_, _| Ok(Value::Int4(0)),
        min_args: 0,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: vec![],
        coll: None,
    });
    Expr::Call(def, args, DataType::Int4)
}

/// Replace placeholders top-down.
pub(crate) fn replace_placeholders(e: Expr, f: &dyn Fn(&Expr) -> Option<Expr>) -> Result<Expr> {
    if let Some(r) = f(&e) {
        return Ok(r);
    }
    e.map_children(&mut |c| replace_placeholders(c, f))
}

/// Postgres's FigureColname: the default name of a select-list item.
pub(crate) fn figure_colname(n: &Node) -> String {
    let (name, _) = figure(n);
    name.unwrap_or_else(|| "?column?".into())
}

/// FigureIndexColname: an index expression column's preliminary name (None: "expr").
pub(crate) fn figure_index_colname(n: &Node) -> Option<String> {
    figure(n).0
}

fn figure(n: &Node) -> (Option<String>, u8) {
    match inner(n) {
        // (The last field name, ignoring `*`.)
        Some(NodeEnum::ColumnRef(c)) => match c.fields.iter().rev().find_map(|f| ast::sval(f)) {
            Some(s) => (Some(s.to_string()), 2),
            None => (None, 0),
        },
        Some(NodeEnum::AIndirection(i)) => {
            for x in i.indirection.iter().rev() {
                if let Some(s) = ast::sval(x) {
                    return (Some(s.to_string()), 2);
                }
            }
            figure(i.arg.as_deref().unwrap())
        }
        Some(NodeEnum::FuncCall(f)) => (names(&f.funcname).last().cloned(), 2),
        Some(NodeEnum::AExpr(a)) => {
            if a.kind == pb::AExprKind::AexprNullif as i32 {
                (Some("nullif".into()), 2)
            } else {
                (None, 0)
            }
        }
        Some(NodeEnum::TypeCast(tc)) => {
            let (n, s) = figure(tc.arg.as_deref().unwrap());
            if s > 1 {
                return (n, s);
            }
            match tc
                .type_name
                .as_ref()
                .and_then(|t| names(&t.names).last().cloned())
            {
                Some(t) => (Some(t), 1),
                None => (n, s),
            }
        }
        Some(NodeEnum::CollateClause(c)) => figure(c.arg.as_deref().unwrap()),
        Some(NodeEnum::GroupingFunc(_)) => (Some("grouping".into()), 2),
        Some(NodeEnum::MergeSupportFunc(_)) => (Some("merge_action".into()), 2),
        Some(NodeEnum::JsonObjectConstructor(_)) => (Some("json_object".into()), 2),
        Some(NodeEnum::JsonArrayConstructor(_) | NodeEnum::JsonArrayQueryConstructor(_)) => {
            (Some("json_array".into()), 2)
        }
        Some(NodeEnum::JsonArrayAgg(_)) => (Some("json_arrayagg".into()), 2),
        Some(NodeEnum::JsonObjectAgg(_)) => (Some("json_objectagg".into()), 2),
        Some(NodeEnum::JsonParseExpr(_)) => (Some("json".into()), 2),
        Some(NodeEnum::JsonScalarExpr(_)) => (Some("json_scalar".into()), 2),
        Some(NodeEnum::JsonSerializeExpr(_)) => (Some("json_serialize".into()), 2),
        Some(NodeEnum::JsonFuncExpr(f)) => (
            Some(
                match pb::JsonExprOp::try_from(f.op) {
                    Ok(pb::JsonExprOp::JsonExistsOp) => "json_exists",
                    Ok(pb::JsonExprOp::JsonQueryOp) => "json_query",
                    _ => "json_value",
                }
                .into(),
            ),
            2,
        ),
        Some(NodeEnum::SubLink(s)) => match pb::SubLinkType::try_from(s.sub_link_type)
            .unwrap_or(pb::SubLinkType::ExprSublink)
        {
            pb::SubLinkType::ExistsSublink => (Some("exists".into()), 2),
            pb::SubLinkType::ArraySublink => (Some("array".into()), 2),
            pb::SubLinkType::ExprSublink => {
                // (The subquery's first output column's name: a set operation's left side's,
                // VALUES' column1.)
                let mut sel = match s.subselect.as_deref().and_then(inner) {
                    Some(NodeEnum::SelectStmt(sel)) => Some(&**sel),
                    _ => None,
                };
                while let Some(l) = sel.and_then(|x| x.larg.as_deref()) {
                    sel = Some(l);
                }
                if sel.is_some_and(|x| !x.values_lists.is_empty()) {
                    return (Some("column1".into()), 2);
                }
                if let Some(sel) = sel
                    && let Some(t) = sel.target_list.first()
                    && let Some(NodeEnum::ResTarget(rt)) = inner(t)
                {
                    if !rt.name.is_empty() {
                        return (Some(rt.name.clone()), 2);
                    }
                    return figure(rt.val.as_deref().unwrap());
                }
                (None, 0)
            }
            _ => (None, 0),
        },
        Some(NodeEnum::CaseExpr(c)) => {
            let (n, s) = match c.defresult.as_deref() {
                Some(d) => figure(d),
                None => (None, 0),
            };
            if s <= 1 {
                (Some("case".into()), 1)
            } else {
                (n, s)
            }
        }
        Some(NodeEnum::AArrayExpr(_)) => (Some("array".into()), 2),
        Some(NodeEnum::RowExpr(_)) => (Some("row".into()), 2),
        Some(NodeEnum::CoalesceExpr(_)) => (Some("coalesce".into()), 2),
        Some(NodeEnum::MinMaxExpr(m)) => (
            Some(
                if m.op == pb::MinMaxOp::IsGreatest as i32 {
                    "greatest"
                } else {
                    "least"
                }
                .into(),
            ),
            2,
        ),
        Some(NodeEnum::SqlvalueFunction(f)) => {
            use pb::SqlValueFunctionOp as O;
            let name = match O::try_from(f.op).unwrap_or(O::SvfopCurrentDate) {
                O::SvfopCurrentDate => "current_date",
                O::SvfopCurrentTime | O::SvfopCurrentTimeN => "current_time",
                O::SvfopCurrentTimestamp | O::SvfopCurrentTimestampN => "current_timestamp",
                O::SvfopLocaltime | O::SvfopLocaltimeN => "localtime",
                O::SvfopLocaltimestamp | O::SvfopLocaltimestampN => "localtimestamp",
                O::SvfopCurrentRole => "current_role",
                O::SvfopCurrentUser => "current_user",
                O::SvfopUser => "user",
                O::SvfopSessionUser => "session_user",
                O::SvfopCurrentCatalog => "current_catalog",
                O::SvfopCurrentSchema => "current_schema",
                _ => "?column?",
            };
            (Some(name.into()), 2)
        }
        Some(NodeEnum::XmlExpr(x)) => {
            use pb::XmlExprOp as X;
            let name = match X::try_from(x.op).unwrap_or(X::Undefined) {
                X::IsXmlconcat => "xmlconcat",
                X::IsXmlelement => "xmlelement",
                X::IsXmlforest => "xmlforest",
                X::IsXmlparse => "xmlparse",
                X::IsXmlpi => "xmlpi",
                X::IsXmlroot => "xmlroot",
                X::IsXmlserialize => "xmlserialize",
                _ => return (None, 0),
            };
            (Some(name.into()), 2)
        }
        Some(NodeEnum::XmlSerialize(_)) => (Some("xmlserialize".into()), 2),
        _ => (None, 0),
    }
}

/// The alias of a recursive term's reference to its own CTE.
fn self_ref_alias(n: &Node, name: &str) -> Option<String> {
    match inner(n) {
        Some(NodeEnum::RangeVar(rv)) if rv.schemaname.is_empty() && rv.relname == name => Some(
            rv.alias
                .as_ref()
                .map(|a| a.aliasname.clone())
                .unwrap_or_else(|| rv.relname.clone()),
        ),
        Some(NodeEnum::JoinExpr(j)) => j
            .larg
            .as_deref()
            .and_then(|l| self_ref_alias(l, name))
            .or_else(|| j.rarg.as_deref().and_then(|r| self_ref_alias(r, name))),
        _ => None,
    }
}

struct Cycle {
    by: Vec<usize>,
    to: Expr,
    default: Expr,
    ne: Arc<FuncDef>,
}

/// A recursive CTE's SEARCH / CYCLE columns (in that order), and how each term computes them.
#[derive(Default)]
struct SearchCycle {
    cols: Vec<OutCol>,
    /// (BY columns, breadth first).
    search: Option<(Vec<usize>, bool)>,
    cycle: Option<Cycle>,
    append: Option<Arc<FuncDef>>,
    eq: Option<Arc<FuncDef>>,
    plus: Option<Arc<FuncDef>>,
    /// The query's column names (the breadth-first row's fields after its depth).
    names: Vec<String>,
}

impl SearchCycle {
    fn row(cols: &[Expr], by: &[usize]) -> Expr {
        Expr::Row(
            by.iter().map(|i| cols[*i].clone()).collect(),
            DataType::Record,
        )
    }

    fn path_start(cols: &[Expr], by: &[usize]) -> Expr {
        Expr::Array(
            vec![Self::row(cols, by)],
            DataType::array_of(DataType::Record),
        )
    }

    fn path_extend(&self, path: &Expr, cols: &[Expr], by: &[usize]) -> Expr {
        let ty = DataType::array_of(DataType::Record);
        Expr::Call(
            self.append.clone().unwrap(),
            vec![path.clone(), Self::path_start(cols, by)],
            ty,
        )
    }

    /// The breadth-first sequence row: its depth ("*DEPTH*"), then the BY columns.
    fn depth_row(&self, depth: Expr, cols: &[Expr], by: &[usize]) -> Expr {
        let mut f = vec![depth];
        f.extend(by.iter().map(|i| cols[*i].clone()));
        let fields: Vec<(String, DataType)> = by
            .iter()
            .map(|i| (self.names.get(*i).cloned().unwrap_or_default(), cols[*i].ty()))
            .collect();
        Expr::Row(f, breadth_row_type(&fields))
    }

    /// The extra columns for a row of the non-recursive term.
    fn start(&self, cols: &[Expr]) -> Vec<Expr> {
        let mut out = Vec::new();
        if let Some((by, breadth)) = &self.search {
            out.push(if *breadth {
                self.depth_row(Expr::Const(Value::Int8(0), DataType::Int8), cols, by)
            } else {
                Self::path_start(cols, by)
            });
        }
        if let Some(c) = &self.cycle {
            out.push(c.default.clone());
            out.push(Self::path_start(cols, &c.by));
        }
        out
    }

    /// The extra columns for a row of the recursive term; `passed` are the working row's.
    fn next(&self, cols: &[Expr], passed: &[Expr]) -> Vec<Expr> {
        let mut out = Vec::new();
        let mut k = 0;
        if let Some((by, breadth)) = &self.search {
            out.push(if *breadth {
                let depth = Expr::Field(Box::new(passed[0].clone()), 0, DataType::Int8);
                let inc = Expr::Call(self.plus.clone().unwrap(), vec![depth], DataType::Int8);
                self.depth_row(inc, cols, by)
            } else {
                self.path_extend(&passed[0], cols, by)
            });
            k = 1;
        }
        if let Some(c) = &self.cycle {
            let path = &passed[k + 1];
            let seen = Expr::ArrayOp {
                left: Box::new(Self::row(cols, &c.by)),
                right: Box::new(path.clone()),
                op: self.eq.clone().unwrap(),
                all: false,
            };
            out.push(Expr::Case {
                operand: None,
                whens: vec![(seen, c.to.clone())],
                else_: Some(Box::new(c.default.clone())),
                ty: c.to.ty(),
            });
            out.push(self.path_extend(path, cols, &c.by));
        }
        out
    }

    /// CYCLE: working rows already marked as a cycle are not expanded.
    fn not_cycle(&self, passed: &[Expr]) -> Option<Expr> {
        let c = self.cycle.as_ref()?;
        let mark = passed[usize::from(self.search.is_some())].clone();
        // (The boolean marks true / false: NOT mark, as rewriteSearchAndCycle writes it.)
        if matches!(c.to, Expr::Const(Value::Bool(true), _)) && matches!(c.default, Expr::Const(Value::Bool(false), _)) {
            return Some(Expr::Not(Box::new(mark)));
        }
        Some(Expr::Call(
            c.ne.clone(),
            vec![mark, c.to.clone()],
            DataType::Bool,
        ))
    }
}

/// A breadth-first SEARCH column's row type: "*DEPTH*", then the BY columns.
fn breadth_row_type(by: &[(String, DataType)]) -> DataType {
    let mut fields = vec![("*DEPTH*".to_string(), DataType::Int8)];
    fields.extend(by.iter().cloned());
    DataType::row(fields)
}

/// findTargetlistEntrySQL92: a constant item must be an output column position.
fn non_integer_constant(n: &Node, what: &str) -> Result<()> {
    match inner(n) {
        Some(NodeEnum::AConst(c)) if !matches!(c.val, Some(pb::a_const::Val::Ival(_))) => {
            Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("non-integer constant in {what}"),
                ),
                c.location,
            ))
        }
        _ => Ok(()),
    }
}

/// `get_sort_group_operators` asked for equality: an error at `loc` for a type without it.
pub(crate) fn require_equality(ty: &DataType, loc: i32) -> Result<()> {
    if ty.has_equality() {
        return Ok(());
    }
    Err(with_pos(
        Error::new(
            code::UNDEFINED_FUNCTION,
            format!(
                "could not identify an equality operator for type {}",
                ty.display()
            ),
        ),
        loc,
    ))
}

/// Where each of `n` output columns was written in a select list (a `*` stands for the
/// columns it expands to); -1 where that isn't known.
/// The select-list entry an ORDER BY expression repeats: the sort uses it (SQL99's
/// findTargetlistEntrySQL99), so a volatile expression isn't evaluated twice.
fn repeated_target(n: &Node, list: &[Node], targets: &[Target]) -> Option<Expr> {
    if list.len() != targets.len()
        || !matches!(inner(n), Some(NodeEnum::FuncCall(_) | NodeEnum::AExpr(_) | NodeEnum::TypeCast(_)))
    {
        return None;
    }
    let text = ast::deparse_expr(n).ok()?;
    list.iter().zip(targets).find_map(|(t, tg)| match inner(t) {
        Some(NodeEnum::ResTarget(r))
            if r.val.as_deref().and_then(|v| ast::deparse_expr(v).ok()).as_deref()
                == Some(text.as_str()) =>
        {
            Some(tg.expr.clone())
        }
        _ => None,
    })
}

/// Which select-list entries a sort's input leaves to be computed after it: the volatile
/// ones it doesn't sort by, with the expensive ones (subqueries, user functions) when there
/// are volatile ones or a LIMIT. Returns the sort's input columns, the columns after the
/// sort (over those), and where each kept column went.
fn postpone_targets(
    proj: &[Expr],
    ntargets: usize,
    keys: &[usize],
    limit: bool,
) -> Option<(Vec<Expr>, Vec<Expr>, Vec<usize>)> {
    let volatile = |e: &Expr| {
        let mut v = false;
        e.walk(&mut |x| match x {
            Expr::Call(d, ..) if d.volatility == 'v' => v = true,
            Expr::Subquery(s) if s.func == Some('v') => v = true,
            _ => {}
        });
        v
    };
    // (An InitPlan's value is a parameter, cheap: only a SubPlan run per row counts.)
    let expensive = |e: &Expr| {
        let mut v = false;
        e.walk(&mut |x| {
            v |= matches!(x, Expr::Subquery(s)
                if !(s.params.is_empty() && s.func.is_none()
                    && !matches!(s.kind, crate::sql::expr::SubqueryKind::Any | crate::sql::expr::SubqueryKind::All)))
        });
        v
    };
    let (mut vol, mut exp) = (Vec::new(), Vec::new());
    for (i, e) in proj.iter().enumerate().take(ntargets) {
        if keys.contains(&i) {
            continue;
        }
        if volatile(e) {
            vol.push(i);
        } else if expensive(e) {
            exp.push(i);
        }
    }
    let later: Vec<usize> = if !vol.is_empty() {
        vol.into_iter().chain(exp).collect()
    } else if limit && !exp.is_empty() {
        exp
    } else {
        return None;
    };
    let mut pre = Vec::new();
    let mut map = vec![usize::MAX; proj.len()];
    for (i, e) in proj.iter().enumerate() {
        if !later.contains(&i) {
            map[i] = pre.len();
            pre.push(e.clone());
        }
    }
    // The input columns the postponed entries read, carried through the sort.
    let mut input_at: std::collections::HashMap<usize, usize> = Default::default();
    for &i in &later {
        proj[i].walk(&mut |x| {
            if let Expr::Col(c, t) = x
                && !input_at.contains_key(c)
            {
                let at = pre
                    .iter()
                    .position(|p| matches!(p, Expr::Col(pc, _) if pc == c))
                    .unwrap_or_else(|| {
                        pre.push(Expr::Col(*c, t.clone()));
                        pre.len() - 1
                    });
                input_at.insert(*c, at);
            }
        });
    }
    let post = proj
        .iter()
        .enumerate()
        .map(|(i, e)| {
            if map[i] != usize::MAX {
                return Expr::Col(map[i], e.ty());
            }
            // An entry the sort's input computes already is taken from there.
            if let Some(j) = (0..proj.len()).find(|&j| map[j] != usize::MAX && equal_exprs(&proj[j], e)) {
                return Expr::Col(map[j], e.ty());
            }
            e.clone().remap_cols(&|c| input_at[&c])
        })
        .collect();
    Some((pre, post, map))
}

/// Expressions equal as Postgres's equal() sees them: two calls of the same user function
/// with the same arguments are (each call is planned as its own subquery here).
fn equal_exprs(a: &Expr, b: &Expr) -> bool {
    fn canon(e: &Expr) -> String {
        let mut subs = String::new();
        e.walk(&mut |x| {
            if let Expr::Subquery(s) = x {
                subs.push_str(&format!("[{:?}|{:?}|{:?}]", s.func, s.params, s.plan));
            }
        });
        let d = format!("{e:?}");
        let mut out = String::with_capacity(d.len());
        let mut rest = d.as_str();
        while let Some(i) = rest.find("SUBQUERY#") {
            out.push_str(&rest[..i + 9]);
            rest = rest[i + 9..].trim_start_matches(|c: char| c.is_ascii_digit());
        }
        out.push_str(rest);
        out + &subs
    }
    a.same_as(b) || canon(a) == canon(b)
}

fn target_locations(list: &[Node], n: usize) -> Vec<i32> {
    let star = |t: &Node| match inner(t) {
        Some(NodeEnum::ResTarget(r)) => matches!(
            r.val.as_deref().and_then(inner),
            Some(NodeEnum::ColumnRef(c))
                if matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
        ),
        _ => false,
    };
    let loc = |t: &Node| match inner(t) {
        Some(NodeEnum::ResTarget(r)) => r.val.as_deref().map_or(r.location, ast::location),
        _ => -1,
    };
    let stars = list.iter().filter(|t| star(t)).count();
    let mut out = Vec::with_capacity(n);
    if stars <= 1 {
        let star_width = n.saturating_sub(list.len() - stars);
        for t in list {
            let k = if star(t) { star_width } else { 1 };
            out.extend(std::iter::repeat_n(loc(t), k));
        }
    }
    out.resize(n, -1);
    out
}

/// Splits a HAVING clause (over the aggregation's output: keys, then aggregates) into what
/// stays and what goes to WHERE (in the input's terms).
fn having_to_where(
    h: Expr,
    groups: &[Expr],
    sets: Option<&[Vec<usize>]>,
    key_colls: &[Option<String>],
) -> (Option<Expr>, Option<Expr>) {
    let ng = groups.len();
    let mut parts = Vec::new();
    optimize::conjuncts(h, &mut parts);
    let (mut keep, mut push) = (Vec::new(), Vec::new());
    for p in parts {
        let cols = p.columns();
        let mut sub = false;
        // (Or a record image comparison: an equality other than the grouping's, which
        // can tell apart rows of one group.)
        p.walk(&mut |x| {
            sub |= match x {
                Expr::Subquery(s) => s.func.is_none(),
                // (A GROUPING() is the grouping's: contain_agg_clause.)
                Expr::Call(d, ..) if d.oid == 0 && d.name == "GROUPING" => true,
                Expr::Call(d, ..) | Expr::ArrayOp { op: d, .. } => d.name.starts_with('*') && d.name.len() > 1,
                _ => false,
            }
        });
        let nullable = |k: &usize| sets.is_some_and(|ss| ss.iter().any(|s| !s.contains(k)));
        // (A key grouped under a nondeterministic collation: a clause over it may tell
        // apart rows of its group.)
        let coll_conflict = optimize::grouping_conflict(&p, &|k| {
            (k < ng).then(|| key_colls.get(k).cloned().flatten())
        });
        if sub
            || coll_conflict
            || p.is_volatile()
            || cols.iter().any(|&k| k >= ng || nullable(&k))
        {
            keep.push(p);
            continue;
        }
        let w = p
            .clone()
            .transform(&mut |x| {
                Ok(match x {
                    Expr::Col(k, _) => groups[k].clone(),
                    x => x,
                })
            })
            .expect("column replacement");
        // (No GROUP BY, or an empty grouping set: kept as well.)
        let empty_set = sets.is_some_and(|ss| ss.iter().any(|s| s.is_empty()));
        if ng == 0 || empty_set {
            keep.push(p);
        }
        push.push(w);
    }
    (optimize::and_all(keep), optimize::and_all(push))
}

/// A subquery aggregating by grouping sets (below its projection, sort and limit).
/// is_simple_subquery's tests on a FROM subquery's own clauses (prepjointree.c): ORDER BY,
/// LIMIT / OFFSET, DISTINCT, grouping, aggregates, FOR UPDATE or a WITH make it a query
/// level of its own, which Postgres plans apart and scans (a Subquery Scan), not pulled up
/// into the query. (Set operations and VALUES lists have their own handling; so do
/// window functions.)
pub(super) fn own_query_level(n: &Node, plan: &Plan) -> bool {
    let Some(NodeEnum::SelectStmt(s)) = inner(n) else {
        return false;
    };
    own_query_level_stmt(s, plan)
}

pub(super) fn own_query_level_stmt(s: &pb::SelectStmt, plan: &Plan) -> bool {
    let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
    // A UNION ALL whose tree has other set operations inside isn't an append relation
    // (is_simple_union_all): a query level of its own too.
    if op == pb::SetOperation::SetopUnion && s.all {
        fn simple(s: &pb::SelectStmt) -> bool {
            let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
            match op {
                pb::SetOperation::SetopNone | pb::SetOperation::Undefined => true,
                pb::SetOperation::SetopUnion if s.all => {
                    s.larg.as_deref().is_none_or(simple) && s.rarg.as_deref().is_none_or(simple)
                }
                _ => false,
            }
        }
        return !simple(s) && matches!(plan, Plan::Append { inh: None, .. });
    }
    if !matches!(op, pb::SetOperation::SetopNone | pb::SetOperation::Undefined) || !s.values_lists.is_empty() {
        return false;
    }
    // The level's own nodes, down to its FROM's.
    fn top(p: &Plan, agg: &mut bool, win: &mut bool) {
        match p {
            Plan::Project { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Distinct { input, .. }
            | Plan::LockRows { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Context { input, .. } => top(input, agg, win),
            Plan::Filter { input, .. } if matches!(&**input, Plan::Agg { .. }) => top(input, agg, win),
            Plan::With { body, .. } => top(body, agg, win),
            Plan::Agg { .. } => *agg = true,
            Plan::Window { input, .. } => {
                *win = true;
                top(input, agg, win)
            }
            _ => {}
        }
    }
    let (mut agg, mut win) = (false, false);
    top(plan, &mut agg, &mut win);
    if win {
        return false;
    }
    agg || !s.sort_clause.is_empty()
        || s.limit_count.is_some()
        || s.limit_offset.is_some()
        || !s.distinct_clause.is_empty()
        || !s.group_clause.is_empty()
        || s.having_clause.is_some()
        || !s.locking_clause.is_empty()
        || s.with_clause.is_some()
}

/// is_safe_append_member: a UNION ALL leaf query pulled up into the appendrel has one
/// FROM item and no WHERE (there is no place for its quals in an appendrel); any other
/// stays a subquery, planned apart.
fn safe_append_member(s: &pb::SelectStmt) -> bool {
    let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
    if !matches!(op, pb::SetOperation::SetopNone | pb::SetOperation::Undefined) || !s.values_lists.is_empty() {
        return true;
    }
    s.where_clause.is_none()
        && s.from_clause.len() <= 1
        && !s
            .from_clause
            .iter()
            .any(|f| matches!(f.node.as_ref(), Some(pg_query::NodeEnum::JoinExpr(_))))
}

pub(super) fn grouping_sets_subquery(p: &Plan) -> bool {
    match p {
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Filter { input, .. } => grouping_sets_subquery(input),
        Plan::Agg { sets: Some(s), .. } => !(s.len() == 1 && s[0].is_empty()),
        _ => false,
    }
}

/// Column names made distinct as EXPLAIN shows a relation's (set_relation_column_names:
/// a repeated name gets `_1`, `_2`, …).
fn unique_colnames(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in names {
        let mut name = n.clone();
        let mut i = 0;
        while out.contains(&name) {
            i += 1;
            name = format!("{n}_{i}");
        }
        out.push(name);
    }
    out
}

/// pull_up_simple_union_all: a FROM subquery that is a simple UNION ALL is an appendrel
/// whose parent is the subquery's own range table entry (its alias), not `*SELECT* 1`.
fn union_alias(p: &mut Plan, name: &str) {
    match p {
        Plan::Project { input, .. } => union_alias(Arc::make_mut(input), name),
        Plan::Append { inh: None, alias: alias @ Some(_), .. } => {
            *alias = Some(if name.is_empty() { "unnamed_subquery" } else { name }.to_string());
        }
        _ => {}
    }
}

/// A FROM subquery's own windows (below its projection, sort and limit): the alias and
/// column names of the subquery they belong to.
fn mark_subquery_windows(p: &mut Plan, info: &Arc<(String, Vec<String>)>) {
    match p {
        Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Filter { input, .. }
        | Plan::Distinct { input, .. } => mark_subquery_windows(Arc::make_mut(input), info),
        // (Those of a subquery inside it keep theirs.)
        Plan::Window { calls, .. } => {
            for c in calls.iter_mut() {
                if c.subquery.is_none() {
                    c.subquery = Some(info.clone());
                }
            }
        }
        // (And a set operation's: a filter it can't take shows as its Subquery Scan.)
        Plan::SetOp { sub, .. } if sub.is_none() => {
            *sub = Some(info.clone());
        }
        _ => {}
    }
}

/// Whether `e` goes NULL when the row it reads is null-extended: it reads a column, and
/// only through strict constructs (pullup_replace_vars_callback's test for leaving out a
/// PlaceHolderVar).
fn nulls_with_row(e: &Expr) -> bool {
    if e.columns().is_empty() {
        return false;
    }
    let mut strict = true;
    e.walk(&mut |x| match x {
        Expr::Col(..) | Expr::Const(..) | Expr::Cast(..) | Expr::Relabel(..) | Expr::Collate(..) => {}
        Expr::Call(d, ..) if d.strict => {}
        _ => strict = false,
    });
    strict
}

/// A FROM item's virtual generated columns that wouldn't go NULL with a null-extended row
/// (constants, COALESCE, CASE, …): computed in a projection over its plan, in their own
/// slots, and read from there.
/// An outer join's nullable side's JOIN USING merged columns (flatten_join_alias_vars'
/// expansions of the join's alias Vars) that read a FROM subquery's computed output: their
/// values computed in a projection right over the side's join, which the outer join above
/// nulls. Once the subquery is pulled up the expansion reads its output as the join below
/// has it (no outer join above nulls it there), and the alias value is a PlaceHolderVar of
/// its own when it isn't a standard alias expression any more (add_nullingrels_if_needed):
/// `'123'::bigint` of `(select 123 as x ...) left join (...) using (x)`, evaluated at that
/// join, not the subquery's `123` at its scan with the cast above.
fn materialize_using(mut plan: Plan, mut scope: Scope) -> (Plan, Scope) {
    // (Whether column `k` of `p`'s row is a subquery's computed output.)
    fn computed(p: &Plan, k: usize) -> bool {
        match p {
            Plan::Join { left, right, nleft, kind, .. } if !matches!(kind, JoinKind::Semi | JoinKind::Anti) => {
                if k < *nleft {
                    computed(left, k)
                } else {
                    computed(right, k - nleft)
                }
            }
            Plan::Filter { input, .. } => computed(input, k),
            Plan::Project { input, exprs } => match exprs.get(k) {
                Some(Expr::Col(j, _)) => computed(input, *j),
                Some(_) => true,
                None => false,
            },
            _ => false,
        }
    }
    if !matches!(plan, Plan::Join { .. }) {
        return (plan, scope);
    }
    let mut found: Vec<Expr> = Vec::new();
    for rel in scope.rels.iter().filter(|r| r.anonymous || r.join_rte) {
        for c in &rel.cols {
            if !matches!(c.expr, Expr::Col(..))
                && c.expr.columns().iter().any(|&k| computed(&plan, k))
                && !found.iter().any(|f| f.same_as(&c.expr))
            {
                found.push(c.expr.clone());
            }
        }
    }
    if found.is_empty() {
        return (plan, scope);
    }
    let w = width(&plan);
    let types = crate::sql::paths::analyzejoins::col_types(&plan);
    let mut exprs: Vec<Expr> = (0..w)
        .map(|k| Expr::Col(k, types.get(k).cloned().unwrap_or(DataType::Unknown)))
        .collect();
    exprs.extend(found.iter().cloned());
    // (Every column with that value reads it there: the join alias's own too.)
    for rel in &mut scope.rels {
        for c in &mut rel.cols {
            if let Some(i) = found.iter().position(|f| f.same_as(&c.expr)) {
                let ty = c.expr.ty();
                c.expr = Expr::Col(w + i, ty);
            }
        }
    }
    if let Plan::Join { meta, .. } = &mut plan {
        meta.alias_cols = found.len();
    }
    (
        Plan::Project {
            input: Arc::new(plan),
            exprs,
        },
        scope,
    )
}

fn materialize_virtuals(plan: Plan, mut scope: Scope) -> (Plan, Scope) {
    let w = width(&plan);
    let mut exprs: Option<Vec<Expr>> = None;
    for rel in &mut scope.rels {
        let Some(t) = rel.table.clone() else { continue };
        if !t.columns.iter().any(|c| c.virtual_gen) {
            continue;
        }
        // The table row's position in the plan's row.
        let Some(off) = rel.cols.iter().find_map(|c| match c.expr {
            Expr::Col(p, _) if c.table == t.oid && c.attnum > 0 && p + 1 >= c.attnum as usize => {
                Some(p + 1 - c.attnum as usize)
            }
            _ => None,
        }) else {
            continue;
        };
        for c in &mut rel.cols {
            if c.table != t.oid || c.attnum <= 0 {
                continue;
            }
            let i = c.attnum as usize - 1;
            let virt = t.columns.get(i).is_some_and(|col| col.virtual_gen && !col.dropped);
            if !virt || matches!(c.expr, Expr::Col(..)) || nulls_with_row(&c.expr) {
                continue;
            }
            let slot = off + i;
            if slot >= w {
                continue;
            }
            let ex = exprs.get_or_insert_with(|| {
                let types = crate::sql::paths::analyzejoins::col_types(&plan);
                (0..w)
                    .map(|k| Expr::Col(k, types.get(k).cloned().unwrap_or(DataType::Unknown)))
                    .collect()
            });
            let ty = c.expr.ty();
            ex[slot] = c.expr.clone();
            c.expr = Expr::Col(slot, ty);
        }
    }
    match exprs {
        Some(exprs) => (
            Plan::Project {
                input: Arc::new(plan),
                exprs,
            },
            scope,
        ),
        None => (plan, scope),
    }
}
