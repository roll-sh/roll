//! INSERT / UPDATE / DELETE, and the per-table write rules (defaults, generated columns,
//! CHECK and domain constraints, index keys) the executor applies to every written row.

use super::ast::{self, inner, with_pos};
use super::{Binder, Clause, Scope, ScopeRel, table_rel};
use crate::catalog::{ConstraintKind, Identity, IndexKey, Relation, Table};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::expr::Expr;
use crate::sql::plan::{
    BoundIndex, BoundTrigger, ConflictAction, DeletePlan, IndexConstraint, InsertPlan, MergeWhen,
    OldNew, OnConflict, OutCol, Plan, ScanPlan, TableRules, UpdatePlan,
};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::sync::Arc;

/// Indexes of a table with bound key expressions; the primary key comes first.
pub fn bound_indexes(t: &Arc<Table>, ctx: &ExecCtx) -> Result<Vec<BoundIndex>> {
    let mut b = Binder::new(ctx, Vec::new());
    b.bound_indexes(t)
}

/// Everything the executor checks or computes when writing rows of `t`.
pub fn table_rules(t: &Arc<Table>, ctx: &ExecCtx) -> Result<TableRules> {
    let mut b = Binder::new(ctx, Vec::new());
    b.table_rules(t)
}

/// Routing of rows written to the partitioned table `t`.
pub fn partition_routing(
    t: &Arc<Table>,
    ctx: &ExecCtx,
) -> Result<Option<Arc<crate::sql::plan::PartRouting>>> {
    let mut b = Binder::new(ctx, Vec::new());
    b.partition_routing(t)
}

impl Binder<'_, '_> {
    /// (Constraints and defaults run with no privilege checks of their own.)
    pub(crate) fn bound_indexes(&mut self, t: &Arc<Table>) -> Result<Vec<BoundIndex>> {
        let saved = std::mem::take(&mut self.perms);
        let r = self.bound_indexes_inner(t, false);
        self.perms = saved;
        r
    }

    /// `bound_indexes`, INVALID ones too (to build them).
    pub(crate) fn bound_indexes_all(&mut self, t: &Arc<Table>) -> Result<Vec<BoundIndex>> {
        let saved = std::mem::take(&mut self.perms);
        let r = self.bound_indexes_inner(t, true);
        self.perms = saved;
        r
    }

    fn bound_indexes_inner(&mut self, t: &Arc<Table>, invalid: bool) -> Result<Vec<BoundIndex>> {
        let mut ixs = self.cat.table_indexes(t);
        // An index a failed concurrent build left INVALID isn't kept up (indisready).
        if !invalid && t.partkey.is_none() {
            ixs.retain(|i| i.valid);
        }
        if let Some(p) = t.pkey_index
            && !ixs.iter().any(|i| i.oid == p)
            && let Some(ix) = self.cat.index(p)
        {
            ixs.push(ix.clone());
        }
        ixs.sort_by_key(|i| (!i.primary, i.oid));
        let mut out = Vec::new();
        for ix in ixs {
            let mut keys = Vec::new();
            for c in &ix.columns {
                keys.push(match &c.key {
                    IndexKey::Column(i) => Expr::Col(*i, t.columns[*i].ty.clone()),
                    IndexKey::Expr(sql) => {
                        let n = ast::parse_expr(sql)?;
                        self.bind_over_table(t, &n, Clause::IndexExpr)?
                    }
                });
            }
            let predicate = match &ix.predicate {
                Some(sql) => {
                    let n = ast::parse_expr(sql)?;
                    let e = self.bind_over_table(t, &n, Clause::IndexPredicate)?;
                    Some(self.coerce_bool(e, "WHERE")?)
                }
                None => None,
            };
            let constraint = match t
                .constraints
                .iter()
                .find(|c| c.kind.index() == Some(ix.oid))
            {
                Some(con) => {
                    let exclusion = match &con.kind {
                        ConstraintKind::Exclusion { ops, .. } => {
                            let n = keys.len();
                            let mut out = Vec::with_capacity(n);
                            for (i, op) in ops.iter().enumerate() {
                                let ty = keys[i].ty();
                                out.push(self.binop(
                                    op,
                                    Expr::Col(i, ty.clone()),
                                    Expr::Col(n + i, ty),
                                    -1,
                                )?);
                            }
                            Some(out)
                        }
                        _ => None,
                    };
                    let leading_eq = matches!(&con.kind, ConstraintKind::Exclusion { ops, .. } if ops.first().is_some_and(|o| o == "="));
                    Some(Arc::new(IndexConstraint {
                        oid: con.oid,
                        name: con.name.clone(),
                        deferral: con.deferral(),
                        exclusion,
                        leading_eq,
                        without_overlaps: con.without_overlaps(),
                    }))
                }
                None => None,
            };
            // (A clustered primary key's entries are its rows' data keys: by bytes.)
            let colls = if ix.id == 0 { Vec::new() } else { key_colls(&self.cat, t, &ix, &keys) };
            out.push(BoundIndex {
                colls,
                index: ix,
                keys,
                predicate,
                constraint,
            });
        }
        Ok(out)
    }

    /// The table's triggers, by name, with WHEN conditions bound.
    /// (Constraints and defaults run with no privilege checks of their own.)
    pub(crate) fn bound_triggers(&mut self, t: &Arc<Table>) -> Result<Vec<BoundTrigger>> {
        let saved = std::mem::take(&mut self.perms);
        let r = self.bound_triggers_inner(t);
        self.perms = saved;
        r
    }

    fn bound_triggers_inner(&mut self, t: &Arc<Table>) -> Result<Vec<BoundTrigger>> {
        let mut trigs: Vec<Arc<crate::catalog::Trigger>> = self
            .cat
            .triggers
            .values()
            .filter(|x| x.table == t.oid)
            .cloned()
            .collect();
        trigs.sort_by(|a, b| a.name.cmp(&b.name));
        let mut out = Vec::new();
        for trig in trigs {
            let Some(func) = self
                .cat
                .functions
                .get(&trig.function)
                .cloned()
                .or_else(|| crate::catalog::Function::builtin_trigger(trig.function, ""))
            else {
                continue;
            };
            let when = match &trig.when {
                Some(w) => Some(self.bind_trigger_when(t, w)?),
                None => None,
            };
            out.push(BoundTrigger {
                trig,
                func,
                when,
                ri: None,
            });
        }
        // The internal triggers of foreign keys fire among them, in name order.
        let ri = crate::exec::trigger::ri_bound(&self.cat, t.oid, false);
        if !ri.is_empty() {
            out.extend(ri);
            out.sort_by(|a, b| a.trig.name.cmp(&b.trig.name));
        }
        Ok(out)
    }

    /// A trigger's WHEN condition: OLD columns at 0.., NEW columns after them.
    pub(crate) fn bind_trigger_when(&mut self, t: &Arc<Table>, sql: &str) -> Result<Expr> {
        let node = ast::parse_expr(sql)?;
        self.bind_trigger_when_node(t, &node)
    }

    /// `bind_trigger_when` of the condition as written (errors point into the statement).
    pub(crate) fn bind_trigger_when_node(&mut self, t: &Arc<Table>, node: &Node) -> Result<Expr> {
        let n = t.columns.len();
        let old = table_rel(t, "old", None);
        let new = Scope {
            rels: vec![table_rel(t, "new", None)],
        }
        .shifted(n)
        .rels
        .pop()
        .unwrap();
        let saved = std::mem::replace(
            &mut self.level().scope,
            Scope {
                rels: vec![old, new],
            },
        );
        let r = (|| {
            let e = self.in_clause(Clause::Where, |b| b.bind_expr(node))?;
            self.coerce_implicit(e, &DataType::Bool)
        })();
        self.level().scope = saved;
        r
    }

    /// (Constraints and defaults run with no privilege checks of their own.)
    pub(crate) fn table_rules(&mut self, t: &Arc<Table>) -> Result<TableRules> {
        let saved = std::mem::take(&mut self.perms);
        let r = self.table_rules_inner(t);
        let gen_funcs = std::mem::take(&mut self.perms.gen_funcs);
        self.perms = saved;
        // The functions generated columns call are run by the statement: EXECUTE on them
        // is checked (ExecInitFunc).
        for f in gen_funcs {
            if !self.perms.funcs.contains(&f) {
                self.perms.funcs.push(f);
            }
        }
        r
    }

    fn table_rules_inner(&mut self, t: &Arc<Table>) -> Result<TableRules> {
        let mut defaults = Vec::new();
        let mut generated = Vec::new();
        let mut domain_checks = Vec::new();
        for c in &t.columns {
            if c.dropped {
                defaults.push(None);
                generated.push(None);
                domain_checks.push(None);
                continue;
            }
            let domain = c.domain.and_then(|d| self.cat.domains.get(&d).cloned());
            let default_sql = match (&c.default, &c.identity, c.seq) {
                (Some(d), _, _) => Some(d.clone()),
                (None, Some(_), Some(seq)) => self.cat.sequences.get(&seq).map(|s| {
                    let name = self.cat.qualified_name(s.schema, &s.name, &[]);
                    format!("nextval('{}'::regclass)", name.replace('\'', "''"))
                }),
                _ => domain.as_ref().and_then(|d| d.default.clone()),
            };
            // A made base type's typdefault: its text through the type's input function.
            let type_default = match &c.ty {
                DataType::Base(b) if default_sql.is_none() && c.generated.is_none() => self
                    .cat
                    .base_types
                    .get(&b.oid)
                    .and_then(|d| d.default.clone()),
                _ => None,
            };
            defaults.push(match default_sql {
                Some(sql) if c.generated.is_none() => {
                    let n = ast::parse_expr(&sql)?;
                    // A default is simplified only when a statement uses it: what fails
                    // folding fails then.
                    let defer = std::mem::replace(&mut self.defer_fold, true);
                    let e = self
                        .bind_scalar(&n, Clause::Default)
                        .and_then(|e| self.coerce_assign(e, &c.ty, &c.name));
                    self.defer_fold = defer;
                    let e = e?;
                    // An identity column's sequence is read without privileges on it.
                    let e = if c.identity.is_some() && c.default.is_none() {
                        e.transform(&mut |x| {
                            Ok(match x {
                                Expr::Call(d, args, t) if d.name == "nextval" => Expr::Call(
                                    Arc::new(crate::sql::func::system::as_identity_nextval(&d)),
                                    args,
                                    t,
                                ),
                                x => x,
                            })
                        })?
                    } else {
                        e
                    };
                    Some(e)
                }
                _ => match type_default {
                    Some(text) => Some(Expr::Const(
                        crate::types::text::parse_value(&text, &c.ty, self.ctx.fmt())?,
                        c.ty.clone(),
                    )),
                    None => None,
                },
            });
            generated.push(match &c.generated {
                Some(sql) => {
                    let n = ast::parse_expr(sql)?;
                    let before = self.perms.funcs.len();
                    let e = self.bind_over_table(t, &n, Clause::Generated)?;
                    let called: Vec<u32> = self.perms.funcs[before.min(self.perms.funcs.len())..].to_vec();
                    self.perms.gen_funcs.extend(called);
                    Some(self.coerce_assign(e, &c.ty, &c.name)?)
                }
                None => None,
            });
            domain_checks.push(match c.domain {
                Some(d) => self.domain_coerce(d)?,
                None => None,
            });
        }
        let mut checks = Vec::new();
        for (name, sql) in t.enforced_checks() {
            let n = ast::parse_expr(sql)?;
            let mut e = self.bind_over_table(t, &n, Clause::Check)?;
            if let Some(c) = t.constraints.iter().find(|c| c.name == name && !c.row_types.is_empty()) {
                e = crate::sql::expr::with_row_type_check(e, &c.row_types);
            }
            checks.push((name.to_string(), self.coerce_bool(e, "CHECK")?));
        }
        // In name order (the relcache sorts them: CheckConstraintFetch).
        checks.sort_by(|a, b| a.0.cmp(&b.0));
        let indexes = self.bound_indexes(t)?;
        let triggers = self.bound_triggers(t)?;
        let view = self.cat.views.contains_key(&t.oid);
        let part_check = self.partition_levels(t, None)?;
        Ok(TableRules {
            table: t.clone(),
            defaults,
            generated,
            checks,
            indexes,
            domain_checks,
            triggers,
            rls_checks: Vec::new(),
            view,
            view_checks: Vec::new(),
            part_check,
        })
    }

    /// The table a DML statement writes; for an automatically updatable view, its base table
    /// and how the view maps onto it. `verbs`: "insert", "update", "delete from" (MERGE may
    /// have several, or none).
    fn dml_target(
        &mut self,
        rv: &pb::RangeVar,
        verbs: &[&str],
        merge: bool,
    ) -> Result<(Arc<Table>, Option<super::views::ViewBase>)> {
        use crate::catalog::acl;
        use crate::catalog::trigger_type as tt;
        let schema = if rv.schemaname.is_empty() {
            None
        } else {
            Some(rv.schemaname.as_str())
        };
        if let Some(s) = schema {
            self.check_schema_usage(s, rv.location)?;
        }
        // (setTargetTable: a transition table, an ephemeral relation, can't be written.)
        if schema.is_none() && self.ctx.enrs.borrow().iter().any(|e| e.name == rv.relname) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "relation \"{}\" cannot be the target of a modifying statement",
                    rv.relname
                ),
            ));
        }
        let privs: u32 = verbs
            .iter()
            .map(|v| match *v {
                "insert" => acl::INSERT,
                "update" => acl::UPDATE,
                _ => acl::DELETE,
            })
            .fold(0, |a, b| a | b);
        let event = |v: &str| match v {
            "insert" => tt::INSERT,
            "update" => tt::UPDATE,
            _ => tt::DELETE,
        };
        if self.refs.is_some()
            && let Some(r) = self.cat.find(schema, &rv.relname, &self.ctx.search_path)
        {
            let aliased = rv.alias.is_some();
            self.note_rel_ref(rv.location, r.oid(), super::refs::RelUse::Target, aliased);
        }
        match self.cat.find(schema, &rv.relname, &self.ctx.search_path) {
            Some(Relation::Table(t)) if merge && t.is_foreign() => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot execute MERGE on relation \"{}\"", rv.relname),
            )
            .detail("This operation is not supported for foreign tables.")),
            Some(Relation::Table(t)) => {
                self.require_rel(t.oid, privs);
                self.check_foreign_scan(&t)?;
                Ok((t, None))
            }
            // The rows of a statement on a view with rules (and CREATE RULE's check of its
            // actions): the view is written as itself.
            Some(Relation::View(v))
                if !v.materialized && (self.rule_target == Some(v.oid) || self.rule_validate) =>
            {
                self.require_rel(v.oid, privs);
                Ok((v.as_table(), None))
            }
            Some(Relation::View(v)) => {
                let has_instead = |b: &Self, ev: u16| {
                    b.cat.triggers.values().any(|x| {
                        x.table == v.oid && x.tgtype & tt::INSTEAD != 0 && x.tgtype & ev != 0
                    })
                };
                // INSTEAD OF row triggers do the work (a MERGE that only does nothing needs
                // none).
                if !v.materialized
                    && (verbs.is_empty() && merge
                        || !verbs.is_empty() && verbs.iter().all(|vb| has_instead(self, event(vb))))
                {
                    self.require_rel(v.oid, privs);
                    return Ok((v.as_table(), None));
                }
                if v.materialized && merge {
                    // (transformMergeStmt's relkind check.)
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("cannot execute MERGE on relation \"{}\"", v.name),
                    )
                    .detail("This operation is not supported for materialized views."));
                }
                if v.materialized {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("cannot change materialized view \"{}\"", v.name),
                    ));
                }
                let writes = verbs.iter().any(|vb| *vb != "delete from");
                let saved_events = std::mem::replace(
                    &mut self.view_events,
                    verbs.iter().map(|vb| event(vb)).fold(0, |a, b| a | b),
                );
                let found = self.auto_update_view(&v, writes, false);
                self.view_events = saved_events;
                let (name, why) = {
                    match found? {
                        // This view is updatable itself (whatever is under it): no partial
                        // set of INSTEAD OF triggers then.
                        Ok(_) if merge && verbs.iter().any(|vb| has_instead(self, event(vb))) => {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!("cannot merge into view \"{}\"", v.name),
                            )
                            .detail("MERGE is not supported for views with INSTEAD OF triggers for some actions but not all.")
                            .hint("To enable merging into the view, either provide a full set of INSTEAD OF triggers or drop the existing INSTEAD OF triggers."));
                        }
                        Ok(vb) => {
                            self.require_rel(v.oid, privs);
                            // Each view's owner needs the privilege on the relation under it.
                            self.require_view_layers(&vb, privs, None);
                            return Ok((vb.base.clone(), Some(vb)));
                        }
                        Err(e)
                            if merge
                                && e.0 != v.name
                                && verbs.iter().any(|vb| has_instead(self, event(vb))) =>
                        {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!("cannot merge into view \"{}\"", v.name),
                            )
                            .detail("MERGE is not supported for views with INSTEAD OF triggers for some actions but not all.")
                            .hint("To enable merging into the view, either provide a full set of INSTEAD OF triggers or drop the existing INSTEAD OF triggers."));
                        }
                        Err(e) => e,
                    }
                };
                // The first action no trigger on the view at fault handles.
                let fault = if name == v.name {
                    v.oid
                } else {
                    self.cat
                        .views
                        .values()
                        .find(|x| x.name == name && !x.materialized)
                        .map_or(v.oid, |x| x.oid)
                };
                let instead_on = |b: &Self, ev: u16| {
                    b.cat.triggers.values().any(|x| {
                        x.table == fault && x.tgtype & tt::INSTEAD != 0 && x.tgtype & ev != 0
                    })
                };
                let verb = verbs
                    .iter()
                    .copied()
                    .find(|vb| !instead_on(self, event(vb)))
                    .unwrap_or("update");
                let (msg, verb_ing, upper) = match verb {
                    "insert" => (
                        format!("cannot insert into view \"{name}\""),
                        "inserting into",
                        "INSERT",
                    ),
                    "update" => (
                        format!("cannot update view \"{name}\""),
                        "updating",
                        "UPDATE",
                    ),
                    _ => (
                        format!("cannot delete from view \"{name}\""),
                        "deleting from",
                        "DELETE",
                    ),
                };
                let hint = if merge {
                    format!(
                        "To enable {verb_ing} the view using MERGE, provide an INSTEAD OF {upper} trigger."
                    )
                } else {
                    format!(
                        "To enable {verb_ing} the view, provide an INSTEAD OF {upper} trigger or an unconditional ON {upper} DO INSTEAD rule."
                    )
                };
                Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, msg)
                    .detail(why)
                    .hint(hint))
            }
            Some(r) if merge => Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot execute MERGE on relation \"{}\"", rv.relname),
            )
            .detail(format!(
                "This operation is not supported for {}s.",
                r.kind_name()
            ))),
            Some(r) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot change {} \"{}\"", r.kind_name(), rv.relname),
            )),
            None => {
                // A system catalog (or a catalog's TOAST table): ExecCheckPermissions refuses
                // it, without a position.
                let catalog_toast = schema == Some("pg_toast")
                    && rv
                        .relname
                        .strip_prefix("pg_toast_")
                        .is_some_and(|n| n.parse::<u32>().is_ok_and(|o| o < 16384));
                if catalog_toast
                    || crate::catalog::system::SysTable::lookup(
                        schema,
                        &rv.relname,
                        &self.ctx.search_path,
                    )
                    .is_some()
                {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for table {}", rv.relname),
                    ));
                }
                Err(with_pos(
                    crate::catalog::no_such_relation(schema, &rv.relname),
                    rv.location,
                ))
            }
        }
    }

    /// RETURNING over the current scope.
    /// RETURNING. `target`: the target's scope entry name, the width of the row RETURNING
    /// sees, where the target's columns start in it and their count, for `old` / `new` (or
    /// the names WITH gives them), which read the old and new rows appended after it.
    fn bind_returning(
        &mut self,
        rc: &Option<pb::ReturningClause>,
        target: (&str, usize, usize, usize),
    ) -> Result<(Vec<Expr>, Vec<OutCol>, Option<OldNew>)> {
        let list = crate::sql::binder::ast::returning(rc);
        if list.is_empty() {
            return Ok((Vec::new(), Vec::new(), None));
        }
        let taken = |b: &Self, n: &str| b.scope().rels.iter().any(|r| !r.anonymous && r.alias == n);
        let (mut old, mut new): (Option<String>, Option<String>) = (None, None);
        for o in rc.iter().flat_map(|rc| &rc.options) {
            let Some(NodeEnum::ReturningOption(ro)) = inner(o) else {
                continue;
            };
            let is_old = ro.option == pb::ReturningOptionKind::ReturningOptionOld as i32;
            if (if is_old { &old } else { &new }).is_some() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "{} cannot be specified multiple times",
                            if is_old { "OLD" } else { "NEW" }
                        ),
                    ),
                    ro.location,
                ));
            }
            if taken(self, &ro.value)
                || old.as_deref() == Some(ro.value.as_str())
                || new.as_deref() == Some(ro.value.as_str())
            {
                return Err(with_pos(
                    Error::new(
                        code::DUPLICATE_ALIAS,
                        format!("table name \"{}\" specified more than once", ro.value),
                    ),
                    ro.location,
                ));
            }
            *(if is_old { &mut old } else { &mut new }) = Some(ro.value.clone());
        }
        let (alias, width, start, ncols) = target;
        let old_given = old.is_some();
        let old = old.or_else(|| {
            (!taken(self, "old") && new.as_deref() != Some("old")).then(|| "old".to_string())
        });
        let new = new.or_else(|| {
            (!taken(self, "new") && !(old_given && old.as_deref() == Some("new")))
                .then(|| "new".to_string())
        });
        let base = self
            .scope()
            .rels
            .iter()
            .find(|r| !r.anonymous && r.alias == alias)
            .cloned();
        let nrels = self.scope().rels.len();
        let aliases = (
            crate::sql::func::intern(old.as_deref().unwrap_or("old")),
            crate::sql::func::intern(new.as_deref().unwrap_or("new")),
        );
        if let Some(base) = &base {
            // Each row carries its system columns too.
            let block = ncols + crate::exec::scan::NUM_SYS_COLS;
            for (name, at) in [(old, width), (new, width + block + 1)] {
                let Some(name) = name else { continue };
                let mut r = base.clone();
                r.alias = name;
                r.schema = None;
                r.key_col = None;
                r.sys_col = Some(at + ncols);
                // Only the table's own columns: old / new rows carry no system columns.
                r.cols.retain(|c| {
                    c.expr
                        .columns()
                        .iter()
                        .all(|&i| (start..start + ncols).contains(&i))
                });
                for c in &mut r.cols {
                    c.expr = c.expr.clone().shift_cols(at as isize - start as isize);
                    // A view's expression columns: NULL too when there is no such row.
                    if !matches!(c.expr, Expr::Col(..)) {
                        let ty = c.expr.ty();
                        c.expr = Expr::Case {
                            operand: None,
                            whens: vec![(Expr::Col(at + block, DataType::Bool), c.expr.clone())],
                            else_: None,
                            ty,
                        };
                    }
                }
                r.exists_col = Some(at + block);
                self.level().scope.rels.push(r);
            }
        }
        let targets = self.in_clause(Clause::Returning, |b| b.bind_targets(list));
        self.level().scope.rels.truncate(nrels);
        let targets = targets?;
        if targets.is_empty() {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "RETURNING must have at least one column"),
                ast::location(&list[0]),
            ));
        }
        self.level().st = Default::default();
        let mut exprs = Vec::new();
        let mut cols = Vec::new();
        for t in targets {
            let e = if matches!(t.expr.ty(), DataType::Unknown) {
                self.coerce_implicit(t.expr, &DataType::Text)?
            } else {
                t.expr
            };
            let domain = self.expr_domain(&e, t.table, t.attnum);
            cols.push(OutCol {
                name: t.name,
                ty: e.ty(),
                table: t.table,
                attnum: t.attnum,
                domain_array: self.domain_array(domain),
                domain,
                collation: None,
            });
            exprs.push(e);
        }
        // Only when something reads them.
        let reads = exprs
            .iter()
            .any(|e| e.columns().iter().any(|&c| c >= width));
        Ok((
            exprs,
            cols,
            (reads && base.is_some()).then_some(OldNew {
                width,
                ncols: ncols + crate::exec::scan::NUM_SYS_COLS,
                sys: self.want_sys,
                old_alias: aliases.0,
                new_alias: aliases.1,
            }),
        ))
    }

    // ── INSERT ──

    pub(crate) fn bind_insert(&mut self, s: &pb::InsertStmt) -> Result<(Plan, Vec<OutCol>)> {
        if s.with_clause.is_some() {
            self.count_cte_refs(pg_query::NodeEnum::InsertStmt(Box::new(s.clone())));
        }
        match &s.with_clause {
            Some(w) => self.with_ctes(w, |b| b.bind_insert_body(s)),
            None => self.bind_insert_body(s),
        }
    }

    fn bind_insert_body(&mut self, s: &pb::InsertStmt) -> Result<(Plan, Vec<OutCol>)> {
        let rv = s.relation.as_ref().unwrap();
        let (t, view) = self.dml_target(rv, &["insert"], false)?;
        // Through an updatable view, the view's column defaults apply first.
        let with_defaults = view
            .as_ref()
            .and_then(|v| super::views::with_view_defaults(s, &v.view));
        let s = with_defaults.as_ref().unwrap_or(s);
        let alias = rv
            .alias
            .as_ref()
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| {
                view.as_ref()
                    .map_or_else(|| t.name.clone(), |v| v.view.name.clone())
            });
        let mut rules = self.table_rules(&t)?;
        if let Some(v) = &view {
            rules.view_checks = v.checks();
        }
        let rules = rules;
        let overriding = pb::OverridingKind::try_from(s.r#override)
            .unwrap_or(pb::OverridingKind::OverridingNotSet);
        let over_system = overriding == pb::OverridingKind::OverridingSystemValue;
        let over_user = overriding == pb::OverridingKind::OverridingUserValue;

        // Target columns. An updatable view without a column list takes as many columns
        // as values are given.
        let given = match &view {
            Some(_) if s.cols.is_empty() => {
                let sel = s.select_stmt.as_deref().and_then(|n| match inner(n) {
                    Some(NodeEnum::SelectStmt(sel)) => Some(sel.as_ref()),
                    _ => None,
                });
                match sel {
                    Some(sel) if is_plain_values(sel) => {
                        sel.values_lists.first().and_then(|l| match inner(l) {
                            Some(NodeEnum::List(items)) => Some(items.items.len()),
                            _ => None,
                        })
                    }
                    Some(sel) => Some(sel.target_list.len()),
                    None => Some(0),
                }
                .unwrap_or(0)
            }
            _ => 0,
        };
        let (mut target, paths, view_cols) =
            self.insert_targets(&s.cols, &t, view.as_ref(), given)?;
        let explicit_cols = !s.cols.is_empty();

        // Source rows.
        let sel = s.select_stmt.as_deref().and_then(|n| match inner(n) {
            Some(NodeEnum::SelectStmt(sel)) => Some(sel.as_ref()),
            _ => None,
        });
        let source = match sel {
            None => {
                // DEFAULT VALUES
                target.clear();
                Plan::One
            }
            // A rule action's VALUES, for each of the triggering statement's rows.
            Some(sel) if is_plain_values(sel) && super::rules::is_marked(&sel.from_clause) => {
                let (source, cols) = self.rule_values(
                    sel,
                    &target,
                    &paths,
                    &t,
                    &rules,
                    (over_system, over_user),
                    explicit_cols.then_some(s.cols.as_slice()),
                )?;
                target = cols;
                source
            }
            Some(sel) if is_plain_values(sel) => {
                let mut rows: Vec<Vec<Expr>> = Vec::new();
                let mut width = None;
                let mut merged = None;
                let mut srfs = Vec::new();
                for l in &sel.values_lists {
                    let Some(NodeEnum::List(items)) = inner(l) else {
                        continue;
                    };
                    // A single VALUES row may call set-returning functions.
                    let clause = if sel.values_lists.len() == 1 {
                        Clause::ValuesSingle
                    } else {
                        Clause::Values
                    };
                    // (`x.*` items stand for several: each expression's location.)
                    let (row, locs, found) = self.in_sibling(|b| {
                        let mut row = Vec::new();
                        let mut locs = Vec::new();
                        for item in &items.items {
                            let es = b.in_clause(clause, |b| b.bind_values_item(item))?;
                            locs.extend(std::iter::repeat_n(value_location(item), es.len()));
                            row.extend(es);
                        }
                        Ok((row, locs, std::mem::take(&mut b.level().st.srfs)))
                    })?;
                    if row.len() > target.len() {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "INSERT has more expressions than target columns",
                            ),
                            locs[target.len()],
                        ));
                    }
                    if explicit_cols && row.len() < target.len() {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "INSERT has more target columns than expressions",
                            ),
                            ast::location(&s.cols[row.len()]),
                        ));
                    }
                    if *width.get_or_insert(row.len()) != row.len() {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "VALUES lists must all be the same length",
                        ));
                    }
                    srfs = found;
                    self.insert_locs = locs.clone();
                    let r = self.insert_row(row, &target, &paths, &t, &rules, over_system, over_user);
                    self.insert_locs.clear();
                    let (row, cols) = r?;
                    merged = Some(cols);
                    rows.push(row);
                }
                let w = width.unwrap_or(0);
                target.truncate(w);
                if let Some(cols) = merged {
                    target = cols;
                }
                if srfs.is_empty() {
                    Plan::Values {
                        rows,
                        ncols: target.len(),
                        rti: Default::default(),
                    }
                } else {
                    let tx = |e: Expr| {
                        super::select::replace_placeholders(e, &|p| {
                            if let Expr::Srf(j, t) = p {
                                Some(Expr::Col(*j, t.clone()))
                            } else {
                                None
                            }
                        })
                    };
                    let exprs = rows
                        .pop()
                        .unwrap_or_default()
                        .into_iter()
                        .map(tx)
                        .collect::<Result<_>>()?;
                    Plan::ProjectSet {
                        input: Arc::new(Plan::One),
                        exprs,
                        srfs,
                    }
                }
            }
            Some(sel) => {
                // Untyped literals in the select list take the target columns' types.
                let (plan, cols) = self.in_sibling(|b| {
                    b.keep_unknown = true;
                    b.bind_select_stmt(sel)
                })?;
                if cols.len() > target.len() {
                    let loc = sel
                        .target_list
                        .get(target.len())
                        .map(ast::location)
                        .unwrap_or(-1);
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            "INSERT has more expressions than target columns",
                        ),
                        loc,
                    ));
                }
                if explicit_cols && cols.len() < target.len() {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            "INSERT has more target columns than expressions",
                        ),
                        ast::location(&s.cols[cols.len()]),
                    ));
                }
                target.truncate(cols.len());
                // (transformInsertStmt's hack: an unknown-typed constant or parameter of the
                // select list is copied up as it is, not read as the subquery's output, so
                // coercing it to the column's type makes a constant of that type.)
                let literal = |i: usize| match &plan {
                    Plan::Project { exprs, .. } => exprs
                        .get(i)
                        .filter(|e| {
                            matches!(e, Expr::Const(_, DataType::Unknown) | Expr::Param(_, DataType::Unknown))
                        })
                        .cloned(),
                    _ => None,
                };
                let vals = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| literal(i).unwrap_or_else(|| Expr::Col(i, c.ty.clone())))
                    .collect();
                // (A set operation's columns are at its leftmost leaf's: transformSetOperationStmt.)
                let mut leaf: &pb::SelectStmt = sel;
                while let Some(l) = leaf.larg.as_deref() {
                    leaf = l;
                }
                self.insert_locs = leaf
                    .target_list
                    .iter()
                    .map(|n| match inner(n) {
                        Some(NodeEnum::ResTarget(rt)) => {
                            rt.val.as_deref().map_or(rt.location, value_location)
                        }
                        _ => -1,
                    })
                    .collect();
                let r = self.insert_row(vals, &target, &paths, &t, &rules, over_system, over_user);
                self.insert_locs.clear();
                let (exprs, merged) = r?;
                target = merged;
                super::optimize::project(plan, exprs)
            }
        };

        match &view {
            Some(v) => {
                let idx = self.require_rel(v.view.oid, crate::catalog::acl::INSERT);
                self.note_target_cols(idx, &view_cols, true);
                self.require_view_layers(v, crate::catalog::acl::INSERT, Some((&view_cols, true)));
            }
            None => {
                let idx = self.require_rel(t.oid, crate::catalog::acl::INSERT);
                self.note_target_cols(idx, &target, true);
            }
        }
        // ON CONFLICT and RETURNING see the target table (or view) under its alias.
        let mut rel = match &view {
            Some(v) => v.scope_rel(&alias),
            None => table_rel(&t, &alias, None),
        };
        // (Under its own name, `schema.table.col` reaches it too, as in UPDATE / DELETE.)
        match &view {
            Some(v) if alias == v.view.name => {
                rel.schema = Some(self.cat.schema_name(v.view.schema).to_string())
            }
            None if alias == t.name => rel.schema = Some(self.cat.schema_name(t.schema).to_string()),
            _ => {}
        }
        let saved = std::mem::replace(
            &mut self.level().scope,
            Scope {
                rels: vec![rel.clone()],
            },
        );
        #[allow(clippy::type_complexity)]
        let r = (|| -> Result<(Option<OnConflict>, Vec<Expr>, Vec<OutCol>, Option<OldNew>)> {
            let oc = match &s.on_conflict_clause {
                Some(oc) => Some(self.bind_on_conflict(oc, &t, &rel, view.as_ref())?),
                None => None,
            };
            if self.want_sys {
                self.level().scope.rels[0].sys_col = Some(t.columns.len());
            }
            let width = t.columns.len()
                + if self.want_sys {
                    crate::exec::scan::NUM_SYS_COLS
                } else {
                    0
                };
            // EXCLUDED is in the range table but can't be referenced from RETURNING.
            let k = self.unreachable.len();
            if s.on_conflict_clause.as_ref().is_some_and(|c| {
                c.action == pb::OnConflictAction::OnconflictUpdate as i32
            }) {
                self.unreachable.push(table_rel(&t, "excluded", None));
            }
            let r = self.bind_returning(&s.returning_clause, (&alias, width, 0, t.columns.len()));
            self.unreachable.truncate(k);
            let (returning, cols, old_new) = r?;
            Ok((oc, returning, cols, old_new))
        })();
        self.level().scope = saved;
        let (on_conflict, returning, cols, old_new) = r?;
        let mut rules = rules;
        if self.rls_enabled(&t)? {
            self.rls_checks(&t, super::rls::INSERT, false, false, &mut rules.rls_checks)?;
            if !crate::sql::binder::ast::returning(&s.returning_clause).is_empty()
                && self.reads_table(t.oid)
            {
                self.rls_checks(&t, super::rls::SELECT, true, false, &mut rules.rls_checks)?;
            }
        }
        let plan = InsertPlan {
            rules,
            source: Arc::new(source),
            target,
            on_conflict,
            returning,
            old_new,
            overriding_system: over_system,
            overriding_user: over_user,
            sys: self.want_sys,
            route: self.partition_routing(&t)?,
            copy: None,
            // (Through an updatable view: its relation's alias in the view.)
            alias: match &view {
                Some(v) => v.base_alias(),
                None => rv.alias.as_ref().map(|a| a.aliasname.clone()),
            },
        };
        Ok((Plan::Insert(Box::new(plan)), cols))
    }

    /// INSERT target columns: (columns, each one's subscripts / field names with its
    /// position, and for an updatable view the view columns named). Without a column list,
    /// every column (a view: its first `given`).
    #[allow(clippy::type_complexity)]
    fn insert_targets(
        &mut self,
        cols: &[Node],
        t: &Arc<Table>,
        view: Option<&super::views::ViewBase>,
        given: usize,
    ) -> Result<(Vec<usize>, Vec<(Vec<Node>, i32)>, Vec<usize>)> {
        let mut target: Vec<usize> = Vec::new();
        // Per target: its subscripts / field names (`INSERT INTO t (a[1]) ...`).
        let mut paths: Vec<(Vec<Node>, i32)> = Vec::new();
        // An updatable view's columns map onto base columns; computed ones can't be written.
        let mut view_cols: Vec<usize> = Vec::new();
        if let Some(v) = view {
            let verb = if self.in_merge {
                "merge into"
            } else {
                "insert into"
            };
            // The columns written, checked view by view as `rewriteTargetView` does.
            let required: Vec<usize> = if cols.is_empty() {
                (0..given.min(v.cols.len())).collect()
            } else {
                cols.iter()
                    .filter_map(|c| match inner(c) {
                        Some(NodeEnum::ResTarget(rt)) => {
                            v.cols.iter().position(|x| x.name == rt.name)
                        }
                        _ => None,
                    })
                    .collect()
            };
            let nonupd = |name: &str| {
                let (view, col, why) = v.first_not_updatable(&required).unwrap_or_else(|| {
                    (v.view.name.clone(), name.to_string(), super::views::NON_COLUMN)
                });
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("cannot {verb} column \"{col}\" of view \"{view}\""),
                )
                .detail(why)
            };
            // Two view columns over one base column (`rewriteTargetListIU`).
            let twice = |b: usize| {
                Error::new(
                    code::SYNTAX_ERROR,
                    format!("multiple assignments to same column \"{}\"", t.columns[b].name),
                )
            };
            if cols.is_empty() {
                for (k, c) in v.cols.iter().enumerate().take(given.min(v.cols.len())) {
                    let Some(b) = c.base else {
                        return Err(nonupd(&c.name));
                    };
                    if target.contains(&b) {
                        return Err(twice(b));
                    }
                    target.push(b);
                    view_cols.push(k);
                }
            } else {
                for c in cols {
                    let Some(NodeEnum::ResTarget(rt)) = inner(c) else {
                        continue;
                    };
                    let Some(k) = v.cols.iter().position(|c| c.name == rt.name) else {
                        return Err(with_pos(
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    rt.name, v.view.name
                                ),
                            ),
                            rt.location,
                        ));
                    };
                    let Some(i) = v.cols[k].base else {
                        return Err(nonupd(&rt.name));
                    };
                    if let Some(p) = target.iter().position(|c| *c == i)
                        && view_cols.get(p) != Some(&k)
                        && (rt.indirection.is_empty() || paths[p].0.is_empty())
                    {
                        return Err(twice(i));
                    }
                    if let Some(p) = target.iter().position(|c| *c == i)
                        && (rt.indirection.is_empty() || paths[p].0.is_empty())
                    {
                        return Err(with_pos(
                            Error::new(
                                code::DUPLICATE_COLUMN,
                                format!("column \"{}\" specified more than once", rt.name),
                            ),
                            rt.location,
                        ));
                    }
                    target.push(i);
                    view_cols.push(k);
                    paths.push((rt.indirection.clone(), rt.location));
                }
            }
        } else if cols.is_empty() {
            target = t.live_columns().map(|(i, _)| i).collect();
        } else {
            for c in cols {
                let Some(NodeEnum::ResTarget(rt)) = inner(c) else {
                    continue;
                };
                let Some(i) = t.column_index(&rt.name) else {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_COLUMN,
                            format!(
                                "column \"{}\" of relation \"{}\" does not exist",
                                rt.name, t.name
                            ),
                        ),
                        rt.location,
                    ));
                };
                // `(a[1], a[2])` may name a column more than once; otherwise it's an error.
                if let Some(p) = target.iter().position(|c| *c == i)
                    && (rt.indirection.is_empty() || paths[p].0.is_empty())
                {
                    return Err(with_pos(
                        Error::new(
                            code::DUPLICATE_COLUMN,
                            format!("column \"{}\" specified more than once", rt.name),
                        ),
                        rt.location,
                    ));
                }
                if rt.location >= 0
                    && let Some(log) = self.ref_log()
                {
                    log.targets.push((rt.location, t.oid, i as i16 + 1));
                }
                // `INSERT INTO t (c.f, a[1])` assigns into a NULL constant of the column's
                // type (transformAssignedExpr), which the statement then depends on.
                if !rt.indirection.is_empty() && self.deps.is_some() {
                    let oid = crate::catalog::system::type_oid(&self.cat, &t.columns[i].ty);
                    if self.cat.user_type_name(oid).is_some()
                        && let Some(deps) = &mut self.deps
                        && !deps.contains(&(1247, oid))
                    {
                        deps.push((1247, oid));
                    }
                }
                target.push(i);
                paths.push((rt.indirection.clone(), rt.location));
            }
        }
        Ok((target, paths, view_cols))
    }

    /// One value for target column `col`: DEFAULT, identity and generated-column rules, then
    /// assignment coercion.
    fn insert_value(
        &mut self,
        e: Expr,
        t: &Table,
        rules: &TableRules,
        col: usize,
        over_system: bool,
        over_user: bool,
    ) -> Result<Expr> {
        let c = &t.columns[col];
        let is_default = matches!(e, Expr::Default(_));
        if c.generated.is_some() {
            if !is_default {
                return Err(Error::new(
                    code::GENERATED_ALWAYS,
                    format!(
                        "cannot insert a non-DEFAULT value into column \"{}\"",
                        c.name
                    ),
                )
                .detail(format!("Column \"{}\" is a generated column.", c.name)));
            }
            return Ok(Expr::null(c.ty.clone()));
        }
        if is_default || (over_user && c.identity.is_some()) {
            return Ok(rules.defaults[col]
                .clone()
                .unwrap_or_else(|| Expr::null(c.ty.clone())));
        }
        if c.identity == Some(Identity::Always) && !over_system {
            return Err(Error::new(
                code::GENERATED_ALWAYS,
                format!(
                    "cannot insert a non-DEFAULT value into column \"{}\"",
                    c.name
                ),
            )
            .detail(format!(
                "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
                c.name
            ))
            .hint("Use OVERRIDING SYSTEM VALUE to override."));
        }
        let Some(domain) = c.domain else {
            return self.coerce_assign(e, &c.ty, &c.name);
        };
        let lit = e.clone();
        let out = self.coerce_assign_to(e, &c.ty, Some(domain), &c.name)?;
        self.literal_domain_check(&lit, out, Some(domain))
    }

    fn bind_on_conflict(
        &mut self,
        oc: &pb::OnConflictClause,
        t: &Arc<Table>,
        rel: &ScopeRel,
        view: Option<&super::views::ViewBase>,
    ) -> Result<OnConflict> {
        let action = pb::OnConflictAction::try_from(oc.action)
            .unwrap_or(pb::OnConflictAction::OnconflictNothing);
        // Through an updatable view, the inference clause names the view's columns.
        let mut infer = oc.infer.clone();
        if let (Some(v), Some(inf)) = (view, infer.as_deref_mut()) {
            for e in &mut inf.index_elems {
                if let Some(NodeEnum::IndexElem(ie)) = e.node.as_mut()
                    && let Some(c) = v.cols.iter().find(|c| c.name == ie.name)
                    && let Some(b) = c.base
                {
                    ie.name = t.columns[b].name.clone();
                }
            }
        }
        let n = t.columns.len();
        // EXCLUDED: the proposed row (through a view: the view's columns over it).
        let excluded = Scope {
            rels: vec![match view {
                Some(v) => v.scope_rel("excluded"),
                None => table_rel(t, "excluded", None),
            }],
        }
        .shifted(n)
        .rels
        .pop()
        .unwrap();
        // The inference clause is analyzed first (with EXCLUDED in the range table for DO
        // UPDATE); the arbiter indexes are inferred when the statement is planned, after
        // the rest of it is analyzed.
        let inferred = match &infer {
            None => {
                if action == pb::OnConflictAction::OnconflictUpdate {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, "ON CONFLICT DO UPDATE requires inference specification or constraint name")
                            .hint("For example, ON CONFLICT (column_name)."),
                        oc.location,
                    ));
                }
                None
            }
            Some(inf) => {
                let update = action == pb::OnConflictAction::OnconflictUpdate;
                let k = self.unreachable.len();
                if update {
                    self.unreachable.push(excluded.clone());
                }
                let r = self.infer_elements(inf, t, rel, update);
                self.unreachable.truncate(k);
                Some(r?)
            }
        };
        let action = match action {
            pb::OnConflictAction::OnconflictUpdate => {
                let scope = Scope {
                    rels: vec![rel.clone(), excluded],
                };
                let saved = std::mem::replace(&mut self.level().scope, scope);
                let saved_conflict = std::mem::replace(&mut self.in_conflict, true);
                let r = (|| -> Result<ConflictAction> {
                    let sets = self.bind_set_list(&oc.target_list, t, view, 0)?;
                    let set_cols: Vec<usize> = sets.iter().map(|s| s.0).collect();
                    match view {
                        // Through a view: UPDATE on it, and under it as each view's owner,
                        // who reads all it mentions there.
                        Some(v) => {
                            let vcols: Vec<usize> = set_cols
                                .iter()
                                .filter_map(|b| v.cols.iter().position(|c| c.base == Some(*b)))
                                .collect();
                            let idx = self.require_rel(v.view.oid, crate::catalog::acl::UPDATE);
                            self.note_target_cols(idx, &vcols, false);
                            self.require_view_layers(
                                v,
                                crate::catalog::acl::UPDATE,
                                Some((&vcols, false)),
                            );
                            self.require_view_reads(v);
                        }
                        None => {
                            let idx = self.require_rel(t.oid, crate::catalog::acl::UPDATE);
                            self.note_target_cols(idx, &set_cols, false);
                        }
                    }
                    let filter = match oc.where_clause.as_deref() {
                        Some(w) => {
                            let e = self.in_clause(Clause::Where, |b| b.bind_expr(w))?;
                            Some(self.coerce_bool_at(e, "WHERE", w)?)
                        }
                        None => None,
                    };
                    let (mut rls_existing, mut rls_new) = (Vec::new(), Vec::new());
                    if self.rls_enabled(t)? {
                        use super::rls::{SELECT, UPDATE};
                        let reads = self.reads_table(t.oid);
                        self.rls_checks(t, UPDATE, true, true, &mut rls_existing)?;
                        if reads {
                            self.rls_checks(t, SELECT, true, true, &mut rls_existing)?;
                        }
                        self.rls_checks(t, UPDATE, false, false, &mut rls_new)?;
                        if reads {
                            self.rls_checks(t, SELECT, true, false, &mut rls_new)?;
                        }
                    }
                    Ok(ConflictAction::Update {
                        sets,
                        filter,
                        rls_existing,
                        rls_new,
                    })
                })();
                self.level().scope = saved;
                self.in_conflict = saved_conflict;
                r?
            }
            _ => ConflictAction::Nothing,
        };
        let arbiters = match inferred {
            None => Vec::new(),
            Some(Inferred::Indexes(v)) => v,
            Some(Inferred::Elems(elems, where_)) => {
                self.infer_arbiters(t, &elems, where_.as_ref())?
            }
        };
        Ok(OnConflict { arbiters, action })
    }

    /// transformOnConflictArbiter: the named constraint's index, or the inference elements
    /// and WHERE clause.
    fn infer_elements(
        &mut self,
        inf: &pb::InferClause,
        t: &Arc<Table>,
        rel: &ScopeRel,
        update: bool,
    ) -> Result<Inferred> {
        if !inf.conname.is_empty() {
            let con = t
                .constraints
                .iter()
                .find(|c| c.name == inf.conname)
                .ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "constraint \"{}\" for table \"{}\" does not exist",
                            inf.conname, t.name
                        ),
                    )
                })?;
            // The constrained columns are read (SELECT), as if named.
            let con_index = match con.kind {
                ConstraintKind::PrimaryKey { index }
                | ConstraintKind::Unique { index }
                | ConstraintKind::Exclusion { index, .. } => Some(index),
                _ => None,
            };
            if let Some(ix) = con_index.and_then(|o| self.cat.indexes.get(&o).cloned()) {
                for c in &ix.columns {
                    if let IndexKey::Column(i) = c.key {
                        self.note_col(t.oid, i as i16 + 1);
                    }
                }
            }
            return match con.kind {
                ConstraintKind::PrimaryKey { index } | ConstraintKind::Unique { index } => {
                    Ok(Inferred::Indexes(vec![index]))
                }
                ConstraintKind::Exclusion { .. } if update => Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    "ON CONFLICT DO UPDATE not supported with exclusion constraints",
                )),
                ConstraintKind::Exclusion { index, .. } => Ok(Inferred::Indexes(vec![index])),
                _ => Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    "constraint in ON CONFLICT clause has no associated index",
                )),
            };
        }
        // Columns / expressions named in the inference clause, each with the collation and
        // btree operator class it asks for.
        let mut elems: Vec<InferElem> = Vec::new();
        let saved = std::mem::replace(&mut self.level().scope, Scope { rels: vec![rel.clone()] });
        let mut where_ = None;
        let r = (|| -> Result<()> {
            for e in &inf.index_elems {
                let Some(NodeEnum::IndexElem(ie)) = inner(e) else {
                    continue;
                };
                let ex = if !ie.name.is_empty() {
                    // (A column reference at the inference clause's location.)
                    let Some(i) = t.column_index(&ie.name) else {
                        return Err(self.missing_column(None, &ie.name, inf.location));
                    };
                    self.note_col(t.oid, i as i16 + 1);
                    Expr::Col(i, t.columns[i].ty.clone())
                } else {
                    self.in_clause(Clause::IndexExpr, |b| {
                        b.bind_expr(ie.expr.as_deref().unwrap())
                    })?
                };
                let coll = super::ast::names(&ie.collation).last().cloned();
                let opclass = if ie.opclass.is_empty() {
                    None
                } else {
                    let parts = super::ast::names(&ie.opclass);
                    let c = self
                        .cat
                        .find_opclass(403, &parts, &self.ctx.search_path)?
                        .ok_or_else(|| {
                            Error::new(
                                code::UNDEFINED_OBJECT,
                                format!(
                                    "operator class \"{}\" does not exist for access method \"btree\"",
                                    parts.join(".")
                                ),
                            )
                        })?;
                    Some((c.family, c.intype))
                };
                elems.push(InferElem { expr: ex, coll, opclass });
            }
            if let Some(w) = inf.where_clause.as_deref() {
                let e = self.in_clause(Clause::Where, |b| b.bind_expr(w))?;
                where_ = Some(self.coerce_bool_at(e, "WHERE", w)?);
            }
            Ok(())
        })();
        self.level().scope = saved;
        r?;
        Ok(Inferred::Elems(elems, where_))
    }

    /// infer_arbiter_indexes: the unique indexes the inference elements and WHERE clause
    /// pick out.
    fn infer_arbiters(
        &mut self,
        t: &Arc<Table>,
        elems: &[InferElem],
        where_: Option<&Expr>,
    ) -> Result<Vec<u32>> {
        let ixs = self.bound_indexes(t)?;
        let infer_cols: std::collections::BTreeSet<usize> = elems
            .iter()
            .filter_map(|e| match e.expr {
                Expr::Col(i, _) => Some(i),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        for bix in &ixs {
            if !bix.index.unique || !bix.index.valid {
                continue;
            }
            let ncols = bix.index.columns.len().min(bix.keys.len());
            let plain: std::collections::BTreeSet<usize> = bix.index.columns[..ncols]
                .iter()
                .filter_map(|c| match c.key {
                    IndexKey::Column(i) => Some(i),
                    IndexKey::Expr(_) => None,
                })
                .collect();
            // Non-expression attributes must match.
            if plain != infer_cols {
                continue;
            }
            let idx_exprs: Vec<&Expr> = (0..ncols)
                .filter(|&k| matches!(bix.index.columns[k].key, IndexKey::Expr(_)))
                .map(|k| &bix.keys[k])
                .collect();
            let am = crate::catalog::system::access_method_oid(&bix.index.method);
            // infer_collation_opclass_match: some index attribute matches the element and
            // the collation / operator class it asks for.
            let coll_opclass_ok = |e: &InferElem| {
                if e.coll.is_none() && e.opclass.is_none() {
                    return true;
                }
                (0..ncols).any(|k| {
                    let col = &bix.index.columns[k];
                    let key = &bix.keys[k];
                    if let Some((family, intype)) = e.opclass {
                        let oc = match col.opclass {
                            0 => self.cat.default_opclass(am, &key.ty()),
                            o => o,
                        };
                        match self.cat.opclass_info(oc) {
                            Some(c) if c.family == family && c.intype == intype => {}
                            _ => return false,
                        }
                    }
                    if let Some(want) = &e.coll {
                        let have = col.collation.clone().or_else(|| match col.key {
                            IndexKey::Column(i) => t.columns.get(i).and_then(|c| c.collation.clone()),
                            IndexKey::Expr(_) => None,
                        });
                        if have.as_deref().unwrap_or("default") != want {
                            return false;
                        }
                    }
                    match (&e.expr, &col.key) {
                        (Expr::Col(i, _), IndexKey::Column(j)) => i == j,
                        (_, IndexKey::Expr(_)) => !matches!(e.expr, Expr::Col(..)) && e.expr.same_as(key),
                        _ => false,
                    }
                })
            };
            let mut ok = true;
            for e in elems {
                if !coll_opclass_ok(e) {
                    ok = false;
                    break;
                }
                if matches!(e.expr, Expr::Col(..))
                    || e.coll.is_some()
                    || e.opclass.is_some()
                    || idx_exprs.iter().any(|x| x.same_as(&e.expr))
                {
                    continue;
                }
                ok = false;
                break;
            }
            // No cataloged expression may be missing from the inference clause.
            if !ok
                || idx_exprs
                    .iter()
                    .any(|x| !elems.iter().any(|e| e.expr.same_as(x)))
            {
                continue;
            }
            // A partial index's predicate must follow from the inference WHERE clause.
            if let Some(pred) = &bix.predicate {
                match where_ {
                    Some(w) if crate::sql::paths::predicate_implied_by(pred, w) => {}
                    _ => continue,
                }
            }
            out.push(bix.index.oid);
        }
        if out.is_empty() {
            return Err(Error::new(
                code::INVALID_COLUMN_REFERENCE,
                "there is no unique or exclusion constraint matching the ON CONFLICT specification",
            ));
        }
        Ok(out)
    }

    /// `SET a = x, (b, c) = (y, z), d = DEFAULT`: (column, expression) pairs.
    /// `tgt`: the target's position in the scope.
    pub(crate) fn bind_set_list(
        &mut self,
        list: &[Node],
        t: &Arc<Table>,
        view: Option<&super::views::ViewBase>,
        tgt: usize,
    ) -> Result<Vec<(usize, Expr)>> {
        let mut rules: Option<TableRules> = None;
        let mut sets: Vec<(usize, Expr)> = Vec::new();
        // Multi-column assignments share one bound subquery per source.
        let mut multi_cache: Option<Vec<Expr>> = None;
        let mut plain_cols: Vec<usize> = Vec::new();
        for item in list {
            let Some(NodeEnum::ResTarget(rt)) = inner(item) else {
                continue;
            };
            let col = match view {
                // An updatable view's column: the base column under it.
                Some(v) => match v.cols.iter().position(|c| c.name == rt.name) {
                    Some(k) => v.cols[k].base.ok_or_else(|| {
                        let required: Vec<usize> = list
                            .iter()
                            .filter_map(|c| match inner(c) {
                                Some(NodeEnum::ResTarget(rt)) => {
                                    v.cols.iter().position(|x| x.name == rt.name)
                                }
                                _ => None,
                            })
                            .collect();
                        let (view, col, why) = v.first_not_updatable(&required).unwrap_or_else(
                            || (v.view.name.clone(), rt.name.clone(), v.cols[k].why),
                        );
                        let verb = if self.in_merge {
                            "merge into"
                        } else if self.in_conflict {
                            "insert into"
                        } else {
                            "update"
                        };
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("cannot {verb} column \"{col}\" of view \"{view}\""),
                        )
                        .detail(why)
                    })?,
                    None => {
                        return Err(with_pos(
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    rt.name, v.view.name
                                ),
                            ),
                            rt.location,
                        ));
                    }
                },
                None => {
                    match t.column_index(&rt.name) {
                        Some(i) => {
                            if rt.location >= 0
                                && let Some(log) = self.ref_log()
                            {
                                log.targets.push((rt.location, t.oid, i as i16 + 1));
                            }
                            i
                        }
                        None => {
                            let mut e = Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "column \"{}\" of relation \"{}\" does not exist",
                                    rt.name, t.name
                                ),
                            );
                            if !rt.indirection.is_empty()
                                && self
                                    .scope()
                                    .rels
                                    .get(tgt)
                                    .is_some_and(|r| r.alias == rt.name)
                            {
                                e = e.hint("SET target columns cannot be qualified with the relation name.");
                            }
                            return Err(with_pos(e, rt.location));
                        }
                    }
                }
            };
            // `a[i] = x, a[j] = y` chain; anything else assigning a column twice is an error.
            let prior = sets.iter().position(|(c, _)| *c == col);
            if let Some(p) = prior
                && (rt.indirection.is_empty()
                    || plain_cols.contains(&col)
                    || !(matches!(sets[p].1, Expr::Assign { .. })
                        || matches!(&sets[p].1, Expr::Call(d, ..) if d.name == "subscript" && d.oid == 0)))
            {
                // (Through a view: the base column's name.)
                let name = if view.is_some() {
                    t.columns[col].name.clone()
                } else {
                    rt.name.clone()
                };
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("multiple assignments to same column \"{name}\""),
                ));
            }
            if rt.indirection.is_empty() {
                plain_cols.push(col);
            }
            let c = &t.columns[col];
            let val = rt.val.as_deref().unwrap();
            let e = match inner(val) {
                Some(NodeEnum::MultiAssignRef(m)) => {
                    let src = m.source.as_deref().unwrap();
                    match inner(src) {
                        // `ROW(t.*)`: the row's fields, `.*` expanded (transformRowExpr).
                        Some(NodeEnum::RowExpr(r))
                            if r.args.iter().any(|a| {
                                matches!(inner(a), Some(NodeEnum::ColumnRef(c))
                                    if c.fields.last().is_some_and(|f| matches!(inner(f), Some(NodeEnum::AStar(_)))))
                            }) =>
                        {
                            if m.colno == 1 || multi_cache.is_none() {
                                let row = self.in_clause(Clause::UpdateSet, |b| b.bind_expr(src))?;
                                let fields = match row {
                                    Expr::Row(args, _) => args,
                                    e => vec![e],
                                };
                                if fields.len() as i32 != m.ncolumns {
                                    return Err(with_pos(
                                        Error::new(
                                            code::SYNTAX_ERROR,
                                            "number of columns does not match number of values",
                                        ),
                                        r.location,
                                    ));
                                }
                                multi_cache = Some(fields);
                            }
                            multi_cache.as_ref().unwrap()[m.colno as usize - 1].clone()
                        }
                        Some(NodeEnum::RowExpr(r)) => {
                            if r.args.len() as i32 != m.ncolumns {
                                return Err(with_pos(
                                    Error::new(
                                        code::SYNTAX_ERROR,
                                        "number of columns does not match number of values",
                                    ),
                                    r.location,
                                ));
                            }
                            self.in_clause(Clause::UpdateSet, |b| {
                                b.bind_expr(&r.args[m.colno as usize - 1])
                            })?
                        }
                        Some(NodeEnum::SubLink(_)) => {
                            if m.colno == 1 || multi_cache.is_none() {
                                let Some(NodeEnum::SubLink(sl)) = inner(src) else {
                                    unreachable!()
                                };
                                let (plan, cols, params) =
                                    self.bind_subquery(sl.subselect.as_deref().unwrap())?;
                                if cols.len() as i32 != m.ncolumns {
                                    return Err(with_pos(
                                        Error::new(
                                            code::SYNTAX_ERROR,
                                            "number of columns does not match number of values",
                                        ),
                                        sl.location,
                                    ));
                                }
                                // One subquery (a MULTIEXPR SubPlan, run once per row), each
                                // column a field of its row.
                                let id = self.fresh_id();
                                let one = cols.len() == 1;
                                let sq = Expr::Subquery(Box::new(crate::sql::expr::Subquery {
                                    id,
                                    kind: crate::sql::expr::SubqueryKind::Scalar,
                                    plan: plan.clone(),
                                    params: params.clone(),
                                    left: vec![],
                                    ops: vec![],
                                    ty: if one { cols[0].ty.clone() } else { DataType::Record },
                                    func: None,
                                    call: None,
                                    test_op: None,
                                    names: Default::default(),
                                    qual: false,
                                    tlist: false,
                                }));
                                let exprs = if one {
                                    vec![sq]
                                } else {
                                    cols.iter()
                                        .enumerate()
                                        .map(|(i, oc)| {
                                            Expr::Field(Box::new(sq.clone()), i, oc.ty.clone())
                                        })
                                        .collect()
                                };
                                multi_cache = Some(exprs);
                            }
                            multi_cache.as_ref().unwrap()[m.colno as usize - 1].clone()
                        }
                        _ => {
                            return Err(with_pos(
                                Error::new(
                                    code::SYNTAX_ERROR,
                                    "source for a multiple-column UPDATE item must be a sub-SELECT or ROW() expression",
                                ),
                                ast::location(src),
                            ));
                        }
                    }
                }
                _ => self.in_clause(Clause::UpdateSet, |b| b.bind_expr(val))?,
            };
            let is_default = matches!(e, Expr::Default(_));
            if c.generated.is_some() && !is_default {
                return Err(Error::new(
                    code::GENERATED_ALWAYS,
                    format!("column \"{}\" can only be updated to DEFAULT", c.name),
                )
                .detail(format!("Column \"{}\" is a generated column.", c.name)));
            }
            if c.identity == Some(Identity::Always) && !is_default {
                return Err(Error::new(
                    code::GENERATED_ALWAYS,
                    format!("column \"{}\" can only be updated to DEFAULT", c.name),
                )
                .detail(format!(
                    "Column \"{}\" is an identity column defined as GENERATED ALWAYS.",
                    c.name
                )));
            }
            let e = if is_default && rt.indirection.is_empty() {
                if c.generated.is_some() {
                    // Recomputed anyway.
                    continue;
                }
                if rules.is_none() {
                    rules = Some(self.table_rules(t)?);
                }
                rules.as_ref().unwrap().defaults[col]
                    .clone()
                    .unwrap_or_else(|| Expr::null(c.ty.clone()))
            } else if rt.indirection.is_empty() {
                let lit = if c.domain.is_some() {
                    e.clone()
                } else {
                    Expr::bool(false)
                };
                // (transformAssignedExpr: a type mismatch points at the expression.)
                let out = self.coerce_assign_to(e, &c.ty, c.domain, &c.name).map_err(|err| {
                    if err.position.is_none() && err.code == code::DATATYPE_MISMATCH {
                        with_pos(err, ast::location(val))
                    } else {
                        err
                    }
                })?;
                self.literal_domain_check(&lit, out, c.domain)?
            } else {
                let base = match prior {
                    Some(p) => sets.remove(p).1,
                    None => self.level().scope.rels[tgt].cols[col].expr.clone(),
                };
                self.in_clause(Clause::UpdateSet, |b| {
                    b.assign_path(base, &rt.indirection, e, &c.name, rt.location)
                })?
            };
            sets.push((col, e));
        }
        let st = std::mem::take(&mut self.level().st);
        if !st.aggs.is_empty() || !st.windows.is_empty() || !st.srfs.is_empty() {
            return Err(Error::new(
                code::GROUPING_ERROR,
                "aggregate functions are not allowed in UPDATE",
            ));
        }
        Ok(sets)
    }

    /// One source row's values for the INSERT targets: coerced, with `a[i]` / `c.f` targets
    /// folded into one value per column. Returns the values and their columns.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert_row(
        &mut self,
        vals: Vec<Expr>,
        target: &[usize],
        paths: &[(Vec<Node>, i32)],
        t: &Arc<Table>,
        rules: &TableRules,
        over_system: bool,
        over_user: bool,
    ) -> Result<(Vec<Expr>, Vec<usize>)> {
        let mut cols: Vec<usize> = Vec::with_capacity(vals.len());
        let mut out: Vec<Expr> = Vec::with_capacity(vals.len());
        for (k, (e, &col)) in vals.into_iter().zip(target).enumerate() {
            let (path, loc) = paths
                .get(k)
                .map(|(p, l)| (p.as_slice(), *l))
                .unwrap_or_default();
            if path.is_empty() {
                let at = self.insert_locs.get(k).copied().unwrap_or(-1);
                let v = self
                    .insert_value(e, t, rules, col, over_system, over_user)
                    .map_err(|err| {
                        if err.position.is_none() && err.code == code::DATATYPE_MISMATCH && at >= 0
                        {
                            with_pos(err, at)
                        } else {
                            err
                        }
                    })?;
                out.push(v);
                cols.push(col);
                continue;
            }
            let c = &t.columns[col];
            let pos = cols.iter().position(|x| *x == col);
            let base = match pos {
                Some(p) => out[p].clone(),
                None => Expr::null(c.ty.clone()),
            };
            let v = self.assign_path(base, path, e, &c.name, loc)?;
            match pos {
                Some(p) => out[p] = v,
                None => {
                    cols.push(col);
                    out.push(v);
                }
            }
        }
        Ok((out, cols))
    }

    /// jsonb subscript assignment (jsonb_subscript_transform): each subscript an integer or
    /// text, the value jsonb.
    fn assign_jsonb_path(
        &mut self,
        base: Expr,
        path: &[Node],
        value: Expr,
        column: &str,
    ) -> Result<Expr> {
        let implicit = |from: &DataType, to: &DataType| {
            from == to
                || crate::types::cast::cast_context(from, to)
                    == Some(crate::types::cast::CastContext::Implicit)
        };
        let mut subs = Vec::with_capacity(path.len());
        for p in path {
            let Some(NodeEnum::AIndices(ix)) = inner(p) else {
                continue;
            };
            if ix.is_slice {
                let at = ix
                    .uidx
                    .as_deref()
                    .or(ix.lidx.as_deref())
                    .map_or(-1, ast::location);
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        "jsonb subscript does not support slices",
                    ),
                    at,
                ));
            }
            let Some(n) = ix.uidx.as_deref() else { continue };
            let e = self.bind_expr(n)?;
            let et = e.ty();
            let e = if matches!(et, DataType::Unknown) {
                self.coerce_implicit(e, &DataType::Text)?
            } else if implicit(&et, &DataType::Int4) {
                self.coerce_implicit(e, &DataType::Int4)?
            } else if implicit(&et, &DataType::Text) {
                self.coerce_implicit(e, &DataType::Text)?
            } else {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("subscript type {} is not supported", et.display()),
                    )
                    .hint("jsonb subscript must be coercible to either integer or text."),
                    ast::location(n),
                ));
            };
            subs.push((Some(e), None, false));
        }
        let value = self.coerce_assign(value, &DataType::Jsonb, column)?;
        Ok(Expr::Assign {
            base: Box::new(base),
            subs,
            field: None,
            value: Box::new(value),
            ty: DataType::Jsonb,
        })
    }

    /// `base` with the element / slice / field named by `path` (a SET target's indirection)
    /// replaced by `value`.
    pub(crate) fn assign_path(
        &mut self,
        base: Expr,
        path: &[Node],
        value: Expr,
        column: &str,
        loc: i32,
    ) -> Result<Expr> {
        let Some(first) = path.first() else {
            let ty = base.ty();
            return self.coerce_assign(value, &ty, column);
        };
        if matches!(value, Expr::Default(_)) {
            let msg = if matches!(inner(first), Some(NodeEnum::String(_))) {
                "cannot set a subfield to DEFAULT"
            } else {
                "cannot set an array element to DEFAULT"
            };
            return Err(with_pos(Error::new(code::FEATURE_NOT_SUPPORTED, msg), loc));
        }
        let ty = base.ty();
        if let Some(NodeEnum::String(f)) = inner(first) {
            let DataType::Row(fields) = &ty else {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "cannot assign to field \"{}\" of column \"{column}\" because its type {} is not a composite type",
                            f.sval,
                            ty.display()
                        ),
                    ),
                    loc,
                ));
            };
            let Some(i) = fields.iter().position(|(n, _)| *n == f.sval) else {
                return Err(with_pos(
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!(
                            "cannot assign to field \"{}\" of column \"{column}\" because there is no such column in data type {}",
                            f.sval,
                            ty.display()
                        ),
                    ),
                    loc,
                ));
            };
            let inner_base = Expr::Field(Box::new(base.clone()), i, fields[i].1.clone());
            let value = if path.len() == 1 {
                let fty = fields[i].1.clone();
                let field = f.sval.clone();
                self.coerce_with(value, &fty, crate::types::cast::CastContext::Assignment, &|from| {
                    with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "subfield \"{field}\" is of type {} but expression is of type {}",
                                fty.without_typmod().display(),
                                from.display()
                            ),
                        )
                        .hint("You will need to rewrite or cast the expression."),
                        loc,
                    )
                })?
            } else {
                self.assign_path(inner_base, &path[1..], value, &f.sval, loc)?
            };
            return Ok(Expr::Assign {
                base: Box::new(base),
                subs: vec![],
                field: Some(i),
                value: Box::new(value),
                ty,
            });
        }
        // A run of subscripts.
        let n = path
            .iter()
            .take_while(|p| matches!(inner(p), Some(NodeEnum::AIndices(_))))
            .count();
        if matches!(ty, DataType::Jsonb) && n == path.len() {
            return self.assign_jsonb_path(base, path, value, column);
        }
        // A type whose SUBSCRIPT handler is native code (hstore): one text subscript, the
        // element assigned as text.
        if let Some(ns) = crate::ext::native_subscript(&self.cat, &ty) {
            let one = match path {
                [p] => match inner(p) {
                    Some(NodeEnum::AIndices(ix)) if !ix.is_slice => ix.uidx.as_deref(),
                    _ => None,
                },
                _ => None,
            };
            let Some(key) = one else {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("{} allows only one subscript", ns.what),
                ));
            };
            let at = super::ast::location(key);
            let key = self.bind_expr(key)?;
            let key = self.coerce_assign(key, &DataType::Text, column).map_err(|_| {
                with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("{} subscript must have type text", ns.what),
                    ),
                    at,
                )
            })?;
            let col = column.to_string();
            let value = self.coerce_with(value, &DataType::Text, crate::types::cast::CastContext::Assignment, &|from| {
                with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "subscripted assignment to \"{col}\" requires type text but expression is of type {}",
                            from.display()
                        ),
                    )
                    .hint("You will need to rewrite or cast the expression."),
                    loc,
                )
            })?;
            let def = super::expr::native_subscript_def(ns.assign, vec![ty.clone(), DataType::Text, DataType::Text], false);
            return Ok(Expr::Call(def, vec![base, key, value], ty));
        }
        let elem = match &ty {
            DataType::Array(elem) => (**elem).clone(),
            t if t.fixed_elem().is_some() => t.fixed_elem().unwrap(),
            _ => {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "cannot subscript type {} because it does not support subscripting",
                            ty.display()
                        ),
                    ),
                    loc,
                ));
            }
        };
        let mut subs = Vec::with_capacity(n);
        for p in &path[..n] {
            let Some(NodeEnum::AIndices(ix)) = inner(p) else {
                unreachable!()
            };
            let mut idx = |n: &Option<Box<Node>>| -> Result<Option<Expr>> {
                let Some(n) = n.as_deref() else {
                    return Ok(None);
                };
                let e = self.bind_expr(n)?;
                Ok(Some(
                    self.coerce_assign(e, &DataType::Int4, column)
                        .map_err(|_| {
                            Error::new(
                                code::DATATYPE_MISMATCH,
                                "array subscript must have type integer",
                            )
                        })?,
                ))
            };
            let upper = idx(&ix.uidx)?;
            let lower = idx(&ix.lidx)?;
            subs.push(if ix.is_slice {
                (lower, upper, true)
            } else {
                (upper, None, false)
            });
        }
        let slice = subs.iter().any(|s| s.2);
        if slice && ty.fixed_elem().is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "slices of fixed-length arrays not implemented",
            ));
        }
        let target = if slice { ty.clone() } else { elem };
        let rest = &path[n..];
        let value = if rest.is_empty() {
            let col = column.to_string();
            let tt = target.clone();
            self.coerce_with(value, &target, crate::types::cast::CastContext::Assignment, &|from| {
                with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "subscripted assignment to \"{col}\" requires type {} but expression is of type {}",
                            tt.without_typmod().display(),
                            from.display()
                        ),
                    )
                    .hint("You will need to rewrite or cast the expression."),
                    loc,
                )
            })?
        } else {
            let inner_base = Expr::Subscript(Box::new(base.clone()), subs.clone(), target);
            self.assign_path(inner_base, rest, value, column, loc)?
        };
        Ok(Expr::Assign {
            base: Box::new(base),
            subs,
            field: None,
            value: Box::new(value),
            ty,
        })
    }

    /// Scan of the DML target with the row key, joined with FROM / USING items. Sets the
    /// level's scope; returns the plan and the table-only WHERE (for READ COMMITTED rechecks).
    fn dml_source(
        &mut self,
        t: &Arc<Table>,
        alias: &str,
        from: &[Node],
        where_: Option<&Node>,
        updatable: Option<&super::views::ViewBase>,
    ) -> Result<(Plan, Option<Expr>)> {
        let n = t.columns.len();
        let view = self.cat.views.get(&t.oid).cloned();
        // (The views an automatically updatable view's statement goes through stay range
        // table entries of their own.)
        if let Some(v) = updatable {
            self.hidden_rtes += v.layers.len();
        }
        if view.is_some() || updatable.is_some() {
            self.want_sys = false;
        } else if !self.want_sys && self.policies_read_sys(t) && self.rls_enabled(t)? {
            self.want_sys = true;
        }
        let mut rel = match updatable {
            Some(v) => v.scope_rel(alias),
            None => table_rel(t, alias, None),
        };
        rel.key_col = Some(n);
        if self.want_sys {
            rel.sys_col = Some(n + 1);
        }
        match updatable {
            Some(v) if alias == v.view.name => {
                rel.schema = Some(self.cat.schema_name(v.view.schema).to_string())
            }
            None if alias == t.name => {
                rel.schema = Some(self.cat.schema_name(t.schema).to_string())
            }
            _ => {}
        }
        self.level().scope = Scope { rels: vec![rel] };
        let mut plan = match &view {
            // A view (with INSTEAD OF triggers): its rows, and an empty key.
            Some(v) => {
                let (p, cols) = self.bind_view_query(v)?;
                let p = self.view_source_level(v, p, &cols, &v.name);
                let mut exprs: Vec<Expr> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Expr::Col(i, c.ty.clone()))
                    .collect();
                exprs.push(Expr::null(DataType::Bytea));
                super::optimize::project(p, exprs)
            }
            None => {
                let scan = ScanPlan {
                    table: t.clone(),
                    index: None,
                    filter: None,
                    with_key: true,
                    reverse: false,
                    mark: None,
                    lock_order: 0,
                    limit: None,
                    sys: self.want_sys,
                    // (Through an updatable view: its relation's alias in the view.)
                    alias: match updatable {
                        Some(v) => v.base_alias(),
                        None => (alias != t.name).then(|| alias.to_string()),
                    },
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
                if self.dml_inh {
                    // The whole tree: the table's own rows and every member's, over its
                    // columns, through one Append (expand_inherited_rtentry). Through a view
                    // the members are named after the base table.
                    let refname = if updatable.is_some() { t.name.as_str() } else { alias };
                    self.expand_inheritance(scan, refname)?
                } else {
                    if updatable.is_none() && self.exclusion_mode() == "on" {
                        scan.excl = self.exclusion_constraints(t)?;
                        scan.part_excl = self.partition_levels(t, None)?;
                    }
                    Plan::Scan(Box::new(scan))
                }
            }
        };
        // An updatable view's WHERE conditions limit the rows touched.
        let view_filter = updatable.and_then(|v| v.filter());
        if let Some(v) = updatable {
            let (secure, plain) = v.filters_by_security();
            if let Some(f) = secure {
                plan = super::optimize::add_filter(&self.cat, plan, f);
            }
            if let Some(f) = plain {
                plan = self.dml_where(plan, f);
            }
        }
        for item in from {
            // (FROM items can't reference the target.)
            if let Some(r) = self.level().scope.rels.first_mut() {
                r.lateral = super::LAT_TARGET;
            }
            let r = self.bind_from_item(item, None);
            if let Some(r) = self.level().scope.rels.first_mut() {
                r.lateral = 0;
            }
            let (p, scope, params) = r?;
            if scope.rels.iter().any(|r| !r.anonymous && r.alias == alias) {
                return Err(Error::new(
                    code::DUPLICATE_ALIAS,
                    format!("table name \"{alias}\" specified more than once"),
                ));
            }
            let nleft = super::optimize::width(&plan);
            let nright = super::optimize::width(&p);
            self.level().scope.extend(scope.shifted(nleft));
            plan = super::optimize::join(
                &self.cat,
                crate::sql::plan::JoinKind::Inner,
                plan,
                p,
                None,
                params,
                nleft,
                nright,
            );
        }
        let mut recheck = view_filter;
        if let Some(w) = where_ {
            if let Some(NodeEnum::CurrentOfExpr(c)) = inner(w) {
                if view.is_some() || updatable.is_some() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "WHERE CURRENT OF on a view is not implemented",
                    ));
                }
                let cursor = self.cursor_name(&c.cursor_name);
                if !set_current_of(&mut plan, t.oid, cursor) {
                    return Err(crate::error::unsupported("WHERE CURRENT OF with FROM"));
                }
                return Ok((plan, None));
            }
            let pred = self.in_clause(Clause::Where, |b| b.bind_expr(w))?;
            let pred = self.coerce_bool_at(pred, "WHERE", w)?;
            let pred = self.simplify_where(pred);
            let st = std::mem::take(&mut self.level().st);
            if !st.aggs.is_empty() {
                return Err(super::expr::not_allowed("aggregate", Clause::Where));
            }
            let mut parts = Vec::new();
            super::optimize::conjuncts(pred.clone(), &mut parts);
            let mut own: Vec<Expr> = parts
                .into_iter()
                .filter(|p| p.columns().iter().all(|c| *c < n))
                .collect();
            own.extend(recheck.take());
            if !own.is_empty() {
                recheck = Some(Expr::And(own));
            }
            plan = self.dml_where(plan, pred);
        }
        Ok((plan, recheck))
    }

    /// An UPDATE / DELETE's WHERE clause over its target's scan: EXISTS / IN sublinks
    /// become semi / anti joins with the target (pull_up_sublinks), the rest filters it.
    fn dml_where(&mut self, plan: Plan, pred: Expr) -> Plan {
        let plain_target = !self.deparse
            && !self.unplanned
            && match &plan {
                Plan::Scan(s) => s.current_of.is_none() && s.table.partkey.is_none(),
                // (An inheritance / partition tree's members: the target relation is still
                // one relation of the join tree, its children an appendrel.)
                Plan::Append { inh: Some(_), .. } => true,
                _ => false,
            };
        if !plain_target {
            return super::optimize::add_filter(&self.cat, plan, pred);
        }
        let (p, rest) = super::sublink::pull_up(&self.cat, self.ctx, plan, pred, &self.unplanned_subs);
        match rest {
            Some(r) => super::optimize::add_filter(&self.cat, p, r),
            None => p,
        }
    }

    // ── MERGE ──

    pub(crate) fn bind_merge(&mut self, s: &pb::MergeStmt) -> Result<(Plan, Vec<OutCol>)> {
        if s.with_clause.as_ref().is_some_and(|w| w.recursive) {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "WITH RECURSIVE is not supported for MERGE statement",
            ));
        }
        if s.with_clause.is_some() {
            self.count_cte_refs(pg_query::NodeEnum::MergeStmt(Box::new(s.clone())));
        }
        match &s.with_clause {
            Some(w) => self.with_ctes(w, |b| b.bind_merge_body(s)),
            None => self.bind_merge_body(s),
        }
    }

    fn bind_merge_body(&mut self, s: &pb::MergeStmt) -> Result<(Plan, Vec<OutCol>)> {
        let whens: Vec<&pb::MergeWhenClause> = s
            .merge_when_clauses
            .iter()
            .filter_map(|n| match inner(n) {
                Some(NodeEnum::MergeWhenClause(w)) => Some(&**w),
                _ => None,
            })
            .collect();
        let mut terminal: Vec<MergeWhen> = Vec::new();
        for w in &whens {
            let k = merge_when(w);
            if terminal.contains(&k) {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "unreachable WHEN clause specified after unconditional WHEN clause",
                ));
            }
            if w.condition.is_none() {
                terminal.push(k);
            }
        }
        // The actions' commands, in WHEN order (what errors about the target name first).
        let mut verbs: Vec<&str> = Vec::new();
        for w in &whens {
            let v = match merge_cmd(w) {
                pb::CmdType::CmdInsert => "insert",
                pb::CmdType::CmdUpdate => "update",
                pb::CmdType::CmdDelete => "delete from",
                _ => continue,
            };
            if !verbs.contains(&v) {
                verbs.push(v);
            }
        }
        let rv = s.relation.as_ref().unwrap();
        let (t, view) = self.dml_target(rv, &verbs, true)?;
        // DO NOTHING needs SELECT on the target (transformMergeStmt), so that some
        // privilege is always checked.
        if whens.iter().any(|w| merge_cmd(w) == pb::CmdType::CmdNothing) {
            let oid = view.as_ref().map_or(t.oid, |v| v.view.oid);
            self.require_rel(oid, crate::catalog::acl::SELECT);
        }
        let alias = rv
            .alias
            .as_ref()
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| {
                view.as_ref()
                    .map_or_else(|| t.name.clone(), |v| v.view.name.clone())
            });
        // Through an updatable view, its column defaults apply to the INSERT actions first.
        let with_defaults: Option<Vec<pb::MergeWhenClause>> = view.as_ref().and_then(|v| {
            let mut changed = false;
            let fixed = whens
                .iter()
                .map(|w| {
                    let mut w = (*w).clone();
                    if merge_cmd(&w) == pb::CmdType::CmdInsert
                        && let Some(ns) = super::views::with_view_defaults(
                            &pb::InsertStmt {
                                cols: w.target_list.clone(),
                                select_stmt: Some(Box::new(ast::mk(NodeEnum::SelectStmt(
                                    Box::new(pb::SelectStmt {
                                        values_lists: vec![ast::mk(NodeEnum::List(pb::List {
                                            items: w.values.clone(),
                                        }))],
                                        ..Default::default()
                                    }),
                                )))),
                                ..Default::default()
                            },
                            &v.view,
                        )
                        && let Some(NodeEnum::SelectStmt(sel)) =
                            ns.select_stmt.as_deref().and_then(inner)
                        && let Some(NodeEnum::List(l)) = sel.values_lists.first().and_then(inner)
                    {
                        w.target_list = ns.cols.clone();
                        w.values = l.items.clone();
                        changed = true;
                    }
                    w
                })
                .collect();
            changed.then_some(fixed)
        });
        let whens: Vec<&pb::MergeWhenClause> = match &with_defaults {
            Some(f) => f.iter().collect(),
            None => whens,
        };
        let saved = std::mem::take(&mut self.level().scope);
        let saved_unreachable = std::mem::take(&mut self.unreachable);
        let saved_merge = std::mem::replace(&mut self.in_merge, true);
        let r = self.bind_merge_inner(s, &whens, &t, view.as_ref(), &alias, rv);
        self.in_merge = saved_merge;
        self.level().scope = saved;
        self.unreachable = saved_unreachable;
        r
    }

    fn bind_merge_inner(
        &mut self,
        s: &pb::MergeStmt,
        whens: &[&pb::MergeWhenClause],
        t: &Arc<Table>,
        view: Option<&super::views::ViewBase>,
        alias: &str,
        rv: &pb::RangeVar,
    ) -> Result<(Plan, Vec<OutCol>)> {
        use crate::sql::plan::{JoinKind, MergeAction, MergeCmd, MergePlan};
        let n = t.columns.len();
        // The source, with a flag column when NOT MATCHED BY SOURCE must tell rows without
        // one apart.
        // (The target is in the range table, but the source can't see it.)
        let target_rel = match view {
            Some(v) => v.scope_rel(alias),
            None => table_rel(t, alias, None),
        };
        let saved_unreachable = std::mem::replace(&mut self.unreachable, vec![target_rel]);
        let source = self.bind_from_item(s.source_relation.as_deref().unwrap(), None);
        self.unreachable = saved_unreachable;
        let (sp, sscope, _) = source?;
        if sscope.rels.iter().any(|r| !r.anonymous && r.alias == alias) {
            return Err(Error::new(
                code::DUPLICATE_ALIAS,
                format!("name \"{alias}\" specified more than once"),
            )
            .detail("The name is used both as MERGE target table and data source."));
        }
        let by_source = whens
            .iter()
            .any(|w| merge_when(w) == MergeWhen::NotMatchedBySource);
        let by_target = whens
            .iter()
            .any(|w| merge_when(w) == MergeWhen::NotMatchedByTarget);
        let m = super::optimize::width(&sp);
        let (sp, flag) = if by_source {
            let mut types = vec![DataType::Unknown; m];
            for r in &sscope.rels {
                for c in &r.cols {
                    if let Expr::Col(i, ty) = &c.expr
                        && *i < m
                    {
                        types[*i] = ty.clone();
                    }
                }
            }
            let mut exprs: Vec<Expr> = types
                .into_iter()
                .enumerate()
                .map(|(i, ty)| Expr::Col(i, ty))
                .collect();
            exprs.push(Expr::bool(true));
            (super::optimize::project(sp, exprs), Some(m))
        } else {
            (sp, None)
        };
        let off = m + flag.is_some() as usize;

        // The target, with its storage key (a constant for a view with INSTEAD OF triggers).
        let tview = self.cat.views.get(&t.oid).cloned();
        if tview.is_some() || view.is_some() {
            self.want_sys = false;
        }
        // A parent's rows come from its children too, which tableoid tells apart.
        // Through a view, whether its own FROM said ONLY.
        let inh = view.map_or(rv.inh, |v| v.base_inh())
            && tview.is_none()
            && (!self.cat.children(t.oid).is_empty() || t.partkey.is_some());
        if inh
            || (tview.is_none()
                && view.is_none()
                && self.policies_read_sys(t)
                && self.rls_enabled(t)?)
        {
            self.want_sys = true;
        }
        let mut trel = match view {
            Some(v) => v.scope_rel(alias),
            None => table_rel(t, alias, None),
        };
        trel.key_col = Some(n);
        if self.want_sys {
            trel.sys_col = Some(n + 1);
        }
        match view {
            Some(v) if alias == v.view.name => {
                trel.schema = Some(self.cat.schema_name(v.view.schema).to_string())
            }
            None if alias == t.name => {
                trel.schema = Some(self.cat.schema_name(t.schema).to_string())
            }
            _ => {}
        }
        let mut tp = match &tview {
            Some(v) => {
                let (p, cols) = self.bind_view_query(v)?;
                let p = self.view_source_level(v, p, &cols, &v.name);
                let mut exprs: Vec<Expr> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| Expr::Col(i, c.ty.clone()))
                    .collect();
                exprs.push(Expr::Const(
                    Value::Bytea(Arc::from(&[][..])),
                    DataType::Bytea,
                ));
                super::optimize::project(p, exprs)
            }
            None => {
                let scan = ScanPlan {
                    table: t.clone(),
                    index: None,
                    filter: None,
                    with_key: true,
                    reverse: false,
                    mark: None,
                    lock_order: 0,
                    limit: None,
                    sys: self.want_sys,
                    alias: (view.is_none() && alias != t.name).then(|| alias.to_string()),
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
                if inh {
                    // (Through a view: its relation's name in the view, not the MERGE's
                    // alias of the view.)
                    let base = match view {
                        Some(v) => v.base_alias().unwrap_or_else(|| t.name.clone()),
                        None => alias.to_string(),
                    };
                    self.expand_inheritance(scan, &base)?
                } else {
                    Plan::Scan(Box::new(scan))
                }
            }
        };
        if let Some(f) = view.and_then(|v| v.filter()) {
            tp = super::optimize::add_filter(&self.cat, tp, f);
        }
        let rls = self.rls_enabled(t)?;
        self.rls_sys = trel.sys_col;
        if rls && let Some(q) = self.rls_using(t, super::rls::SELECT)? {
            // Rows the user can't see never match.
            tp = super::optimize::add_filter(&self.cat, tp, q);
        }
        let nt = super::optimize::width(&tp);
        let trel = Scope { rels: vec![trel] }.shifted(off).rels.pop().unwrap();
        let mut full = sscope.clone();
        full.rels.push(trel.clone());
        let tgt = full.rels.len() - 1;

        // ON
        self.level().scope = full.clone();
        let on = s.join_condition.as_deref().unwrap();
        let cond = self.in_clause(Clause::JoinOn, |b| b.bind_expr(on))?;
        // Postgres doesn't coerce MERGE's join condition: a non-boolean value is read as a
        // boolean when it runs (an integer is true when not zero).
        let cond = match cond.ty() {
            DataType::Bool => cond,
            DataType::Unknown => self.coerce_bool_at(cond, "JOIN/ON", on)?,
            DataType::Int4 => self.coerce_with(
                cond,
                &DataType::Bool,
                crate::types::cast::CastContext::Explicit,
                &|_| crate::error::internal("int4 to boolean"),
            )?,
            _ => Expr::IsNull(Box::new(cond), true),
        };
        let kind = match (by_target, by_source) {
            (false, false) => JoinKind::Inner,
            (true, false) => JoinKind::Left,
            (false, true) => JoinKind::Right,
            (true, true) => JoinKind::Full,
        };
        let plan = super::optimize::join(
            &self.cat,
            kind,
            sp,
            tp,
            Some(cond.clone()),
            Vec::new(),
            off,
            nt,
        );
        let _ = rv;

        // WHEN clauses.
        let mut rules = self.table_rules(t)?;
        if let Some(v) = view {
            rules.view_checks = v.checks();
        }
        let reads = self.reads_table(t.oid);
        let mut actions = Vec::new();
        for w in whens {
            let when = merge_when(w);
            let (visible, hidden) = match when {
                MergeWhen::Matched => (full.rels.clone(), Vec::new()),
                MergeWhen::NotMatchedBySource => (vec![trel.clone()], sscope.rels.clone()),
                MergeWhen::NotMatchedByTarget => (sscope.rels.clone(), vec![trel.clone()]),
            };
            let tpos = match when {
                MergeWhen::Matched => tgt,
                MergeWhen::NotMatchedBySource => 0,
                MergeWhen::NotMatchedByTarget => usize::MAX,
            };
            self.level().scope = Scope { rels: visible };
            self.unreachable = hidden;
            let cond = match w.condition.as_deref() {
                Some(c) => {
                    let e = self.in_clause(Clause::MergeWhen, |b| b.bind_expr(c))?;
                    Some(self.coerce_bool_at(e, "WHEN", c)?)
                }
                None => None,
            };
            let cmd = match merge_cmd(w) {
                pb::CmdType::CmdUpdate => {
                    let sets = self.bind_set_list(&w.target_list, t, view, tpos)?;
                    let set_cols: Vec<usize> = sets.iter().map(|s| s.0).collect();
                    self.note_merge_cols(t, view, &set_cols, false);
                    let (mut rls_existing, mut rls_new) = (Vec::new(), Vec::new());
                    if rls {
                        use super::rls::{SELECT, UPDATE};
                        self.rls_checks(t, UPDATE, true, true, &mut rls_existing)?;
                        if reads {
                            self.rls_checks(t, SELECT, true, true, &mut rls_existing)?;
                        }
                        self.rls_checks(t, UPDATE, false, false, &mut rls_new)?;
                        if reads {
                            self.rls_checks(t, SELECT, true, false, &mut rls_new)?;
                        }
                    }
                    MergeCmd::Update {
                        sets,
                        rls_existing,
                        rls_new,
                    }
                }
                pb::CmdType::CmdDelete => {
                    let mut rls_existing = Vec::new();
                    if rls {
                        self.rls_checks(t, super::rls::DELETE, true, true, &mut rls_existing)?;
                        if reads {
                            self.rls_checks(t, super::rls::SELECT, true, true, &mut rls_existing)?;
                        }
                    }
                    MergeCmd::Delete { rls_existing }
                }
                pb::CmdType::CmdInsert => {
                    let overriding = pb::OverridingKind::try_from(w.r#override)
                        .unwrap_or(pb::OverridingKind::OverridingNotSet);
                    let over_system = overriding == pb::OverridingKind::OverridingSystemValue;
                    let over_user = overriding == pb::OverridingKind::OverridingUserValue;
                    let (mut target, paths, view_cols) =
                        self.insert_targets(&w.target_list, t, view, w.values.len())?;
                    if w.values.len() > target.len() {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "INSERT has more expressions than target columns",
                            ),
                            ast::location(&w.values[target.len()]),
                        ));
                    }
                    if !w.target_list.is_empty() && w.values.len() < target.len() {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "INSERT has more target columns than expressions",
                            ),
                            ast::location(&w.target_list[w.values.len()]),
                        ));
                    }
                    target.truncate(w.values.len());
                    let mut vals = Vec::new();
                    for v in &w.values {
                        vals.push(self.in_clause(Clause::ValuesSingle, |b| b.bind_expr(v))?);
                        if !self.level().st.srfs.is_empty() {
                            // Allowed by the parser, refused when it runs.
                            return Err(with_pos(
                                Error::new(
                                    code::FEATURE_NOT_SUPPORTED,
                                    "set-valued function called in context that cannot accept a set",
                                ),
                                ast::location(v),
                            ));
                        }
                    }
                    let (values, target) =
                        self.insert_row(vals, &target, &paths, t, &rules, over_system, over_user)?;
                    match view {
                        Some(_) => self.note_merge_cols(t, view, &view_cols, true),
                        None => self.note_merge_cols(t, None, &target, true),
                    }
                    let mut rls_new = Vec::new();
                    if rls {
                        self.rls_checks(t, super::rls::INSERT, false, false, &mut rls_new)?;
                        if !crate::sql::binder::ast::returning(&s.returning_clause).is_empty()
                            && reads
                        {
                            self.rls_checks(t, super::rls::SELECT, true, false, &mut rls_new)?;
                        }
                    }
                    MergeCmd::Insert {
                        target,
                        values,
                        rls_new,
                    }
                }
                _ => MergeCmd::Nothing,
            };
            self.level().st = Default::default();
            actions.push(MergeAction { when, cond, cmd });
        }
        self.unreachable = Vec::new();

        // RETURNING sees both, then merge_action().
        self.level().scope = full;
        let width = off + nt;
        let lvl = self.levels.len() - 1;
        let saved_action = self.merge_action.replace((lvl, width));
        let r = self.bind_returning(
            &s.returning_clause,
            (alias, width + 1, off, t.columns.len()),
        );
        self.merge_action = saved_action;
        let (returning, cols, old_new) = r?;
        let mut members = Vec::new();
        if inh {
            let my_temp = self.cat.schema_by_name("pg_temp").map(|s| s.oid);
            for oid in self.cat.expansion(t.oid).into_iter().skip(1) {
                let Some(child) = self.cat.table(oid).cloned() else {
                    continue;
                };
                // (Another session's temporary child isn't one.)
                if child.partkey.is_some() || child.temporary && Some(child.schema) != my_temp {
                    continue;
                }
                members.push(self.merge_member(t, &child, &actions)?);
            }
        }
        // (Through an updatable view, its base table's.)
        let route = if tview.is_none() {
            self.partition_routing(t)?
        } else {
            None
        };
        let plan = MergePlan {
            rules,
            source: Arc::new(plan),
            target: off,
            source_flag: flag,
            actions,
            join_cond: Some(cond),
            returning,
            old_new,
            sys: self.want_sys && tview.is_none() && (view.is_none() || inh),
            members,
            route,
            alias: rv.alias.as_ref().map(|a| a.aliasname.clone()),
        };
        Ok((Plan::Merge(Box::new(plan)), cols))
    }

    /// A MERGE target's child: its rules (row triggers only) and the actions over its rows.
    fn merge_member(
        &mut self,
        parent: &Arc<Table>,
        child: &Arc<Table>,
        actions: &[crate::sql::plan::MergeAction],
    ) -> Result<crate::sql::plan::MergeMember> {
        use crate::sql::plan::MergeCmd;
        let map: Vec<Option<usize>> = parent
            .columns
            .iter()
            .map(|c| {
                if c.dropped {
                    None
                } else {
                    child.column_index(&c.name)
                }
            })
            .collect();
        let col = |c: usize| map[c].unwrap_or(c);
        let rls = |checks: &[crate::sql::plan::RlsCheck]| -> Vec<crate::sql::plan::RlsCheck> {
            checks
                .iter()
                .map(|c| crate::sql::plan::RlsCheck {
                    expr: super::inherit::to_child(c.expr.clone(), parent, child),
                    ..c.clone()
                })
                .collect()
        };
        let mut rules = self.table_rules(child)?;
        rules
            .triggers
            .retain(|tr| tr.trig.tgtype & crate::catalog::trigger_type::ROW != 0);
        let mut update_cols = Vec::new();
        let cmds = actions
            .iter()
            .map(|a| match &a.cmd {
                MergeCmd::Update {
                    sets,
                    rls_existing,
                    rls_new,
                } => {
                    let sets: Vec<(usize, Expr)> =
                        sets.iter().map(|(c, e)| (col(*c), e.clone())).collect();
                    for (c, _) in &sets {
                        if !update_cols.contains(c) {
                            update_cols.push(*c);
                        }
                    }
                    MergeCmd::Update {
                        sets,
                        rls_existing: rls(rls_existing),
                        rls_new: rls(rls_new),
                    }
                }
                MergeCmd::Delete { rls_existing } => MergeCmd::Delete {
                    rls_existing: rls(rls_existing),
                },
                // New rows go into the table named.
                _ => MergeCmd::Nothing,
            })
            .collect();
        Ok(crate::sql::plan::MergeMember {
            rules,
            cmds,
            map,
            update_cols,
        })
    }

    /// Columns a MERGE action writes, for column privileges (on the view and its base table).
    fn note_merge_cols(
        &mut self,
        t: &Arc<Table>,
        view: Option<&super::views::ViewBase>,
        cols: &[usize],
        insert: bool,
    ) {
        use crate::catalog::acl;
        let p = if insert { acl::INSERT } else { acl::UPDATE };
        match view {
            Some(v) => {
                let vcols: Vec<usize> = if insert {
                    cols.to_vec()
                } else {
                    cols.iter()
                        .filter_map(|b| v.cols.iter().position(|c| c.base == Some(*b)))
                        .collect()
                };
                let idx = self.require_rel(v.view.oid, p);
                self.note_target_cols(idx, &vcols, insert);
                let _ = t;
                self.require_view_layers(v, p, Some((&vcols, insert)));
            }
            None => {
                let idx = self.require_rel(t.oid, p);
                self.note_target_cols(idx, cols, insert);
            }
        }
    }

    // ── UPDATE ──

    pub(crate) fn bind_update(&mut self, s: &pb::UpdateStmt) -> Result<(Plan, Vec<OutCol>)> {
        if s.with_clause.is_some() {
            self.count_cte_refs(pg_query::NodeEnum::UpdateStmt(Box::new(s.clone())));
        }
        match &s.with_clause {
            Some(w) => self.with_ctes(w, |b| b.bind_update_body(s)),
            None => self.bind_update_body(s),
        }
    }

    fn bind_update_body(&mut self, s: &pb::UpdateStmt) -> Result<(Plan, Vec<OutCol>)> {
        let rv = s.relation.as_ref().unwrap();
        let (t, view) = self.dml_target(rv, &["update"], false)?;
        let alias = rv
            .alias
            .as_ref()
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| {
                view.as_ref()
                    .map_or_else(|| t.name.clone(), |v| v.view.name.clone())
            });
        let inh = self.dml_inherits(rv, &t, view.as_ref());
        self.dml_inh = inh;
        let r = self.bind_update_one(s, &t, view, &alias);
        self.dml_inh = false;
        r
    }

    /// Whether an UPDATE / DELETE of `t` changes its inheritance tree: not ONLY (through a
    /// view, whether its own FROM said ONLY), and `t` has members or is partitioned.
    /// (relhassubclass still set after the last child went: an inheritance tree of the
    /// parent alone, as Postgres expands it.)
    fn dml_inherits(
        &self,
        rv: &pb::RangeVar,
        t: &Arc<Table>,
        view: Option<&super::views::ViewBase>,
    ) -> bool {
        view.map_or(rv.inh, |v| v.base_inh())
            && !self.cat.views.contains_key(&t.oid)
            && (!self.cat.children(t.oid).is_empty()
                || t.partkey.is_some()
                || self.cat.class_has_subclass.contains(&t.oid))
    }

    /// The members of `t`'s inheritance tree an UPDATE / DELETE's source reads (its
    /// descendants holding rows that pruning and constraint exclusion left), in expansion
    /// order: its result relations besides itself.
    fn dml_members(&self, t: &Arc<Table>, source: &Plan) -> Vec<Arc<Table>> {
        fn scanned(p: &Plan, out: &mut std::collections::HashSet<u32>) {
            if let Plan::Scan(s) = p {
                if s.with_key {
                    out.insert(s.table.oid);
                }
                return;
            }
            for c in crate::sql::explain::plan_inputs(p) {
                scanned(c, out);
            }
        }
        let mut read = std::collections::HashSet::new();
        scanned(source, &mut read);
        self.cat
            .expansion(t.oid)
            .into_iter()
            .skip(1)
            .filter(|oid| read.contains(oid))
            .filter_map(|oid| self.cat.table(oid).cloned())
            .filter(|c| c.partkey.is_none())
            .collect()
    }

    fn bind_update_one(
        &mut self,
        s: &pb::UpdateStmt,
        t: &Arc<Table>,
        view: Option<super::views::ViewBase>,
        alias: &str,
    ) -> Result<(Plan, Vec<OutCol>)> {
        let (t, alias) = (t.clone(), alias.to_string());
        let inh = self.dml_inh;
        let saved = std::mem::take(&mut self.level().scope);
        let r = (|| -> Result<(Plan, Vec<OutCol>)> {
            let (source, recheck) = self.dml_source(
                &t,
                &alias,
                &s.from_clause,
                s.where_clause.as_deref(),
                view.as_ref(),
            )?;
            let sets = self.bind_set_list(&s.target_list, &t, view.as_ref(), 0)?;
            let set_cols: Vec<usize> = sets.iter().map(|s| s.0).collect();
            match &view {
                Some(v) => {
                    let vcols: Vec<usize> = set_cols
                        .iter()
                        .filter_map(|b| v.cols.iter().position(|c| c.base == Some(*b)))
                        .collect();
                    let idx = self.require_rel(v.view.oid, crate::catalog::acl::UPDATE);
                    self.note_target_cols(idx, &vcols, false);
                    self.require_view_layers(v, crate::catalog::acl::UPDATE, Some((&vcols, false)));
                }
                None => {
                    let idx = self.require_rel(t.oid, crate::catalog::acl::UPDATE);
                    self.note_target_cols(idx, &set_cols, false);
                }
            }
            let width = super::optimize::width(&source);
            let (returning, cols, old_new) =
                self.bind_returning(&s.returning_clause, (&alias, width, 0, t.columns.len()))?;
            let mut rules = self.table_rules(&t)?;
            if let Some(v) = &view {
                rules.view_checks = v.checks();
            }
            // The members' rows are checked against the named table's policies.
            let mut source = source;
            self.rls_sys = self.want_sys.then_some(t.columns.len() + 1);
            if let Some(q) = self.rls_using(&t, super::rls::UPDATE)? {
                use super::rls::{SELECT, UPDATE};
                let reads = self.reads_table(t.oid);
                let mut quals = vec![q];
                if reads {
                    // (list_append_unique: the same quals once.)
                    if let Some(q) = self.rls_using(&t, SELECT)?
                        && !quals.iter().any(|x| x.same_as(&q))
                    {
                        quals.push(q);
                    }
                }
                source = super::optimize::add_security_filter(&self.cat, source, Expr::And(quals));
                self.rls_checks(&t, UPDATE, false, false, &mut rules.rls_checks)?;
                if reads {
                    self.rls_checks(&t, SELECT, true, false, &mut rules.rls_checks)?;
                }
            }
            let mut members = Vec::new();
            let mut route = None;
            if inh {
                for child in self.dml_members(&t, &source) {
                    members.push(self.update_member(&t, &child, &rules, &sets)?);
                }
                // Rows leaving their partition move through the partitioned table named.
                route = self.partition_routing(&t)?;
            }
            let plan = UpdatePlan {
                rules,
                source: Arc::new(source),
                sets,
                returning,
                old_new,
                recheck,
                sys: self.want_sys,
                route,
                inh,
                members,
                alias: view.is_none().then(|| alias.clone()),
            };
            Ok((Plan::Update(Box::new(plan)), cols))
        })();
        self.level().scope = saved;
        r
    }

    /// A member of the inheritance tree an UPDATE of `parent` changes: its own rules (row
    /// triggers only; the parent's row-level security checks and views' check options over
    /// its columns) and the assignments to its columns.
    fn update_member(
        &mut self,
        parent: &Arc<Table>,
        child: &Arc<Table>,
        main: &TableRules,
        sets: &[(usize, Expr)],
    ) -> Result<crate::sql::plan::UpdateMember> {
        let mut rules = self.table_rules(child)?;
        // Statement triggers fire for the table named in the statement only.
        rules
            .triggers
            .retain(|tr| tr.trig.tgtype & crate::catalog::trigger_type::ROW != 0);
        rules.rls_checks = main
            .rls_checks
            .iter()
            .map(|c| crate::sql::plan::RlsCheck {
                expr: super::inherit::to_child(c.expr.clone(), parent, child),
                ..c.clone()
            })
            .collect();
        rules.view_checks = main
            .view_checks
            .iter()
            .map(|(n, e)| (n.clone(), super::inherit::to_child(e.clone(), parent, child)))
            .collect();
        let map = column_map(parent, child);
        let sets = sets
            .iter()
            .map(|(c, e)| (map[*c].unwrap_or(*c), e.clone()))
            .collect();
        Ok(crate::sql::plan::UpdateMember { rules, sets, map })
    }

    // ── DELETE ──

    pub(crate) fn bind_delete(&mut self, s: &pb::DeleteStmt) -> Result<(Plan, Vec<OutCol>)> {
        if s.with_clause.is_some() {
            self.count_cte_refs(pg_query::NodeEnum::DeleteStmt(Box::new(s.clone())));
        }
        match &s.with_clause {
            Some(w) => self.with_ctes(w, |b| b.bind_delete_body(s)),
            None => self.bind_delete_body(s),
        }
    }

    fn bind_delete_body(&mut self, s: &pb::DeleteStmt) -> Result<(Plan, Vec<OutCol>)> {
        let rv = s.relation.as_ref().unwrap();
        let (t, view) = self.dml_target(rv, &["delete from"], false)?;
        let alias = rv
            .alias
            .as_ref()
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| {
                view.as_ref()
                    .map_or_else(|| t.name.clone(), |v| v.view.name.clone())
            });
        let inh = self.dml_inherits(rv, &t, view.as_ref());
        self.dml_inh = inh;
        let r = self.bind_delete_one(s, &t, view, &alias);
        self.dml_inh = false;
        r
    }

    fn bind_delete_one(
        &mut self,
        s: &pb::DeleteStmt,
        t: &Arc<Table>,
        view: Option<super::views::ViewBase>,
        alias: &str,
    ) -> Result<(Plan, Vec<OutCol>)> {
        let (t, alias) = (t.clone(), alias.to_string());
        let inh = self.dml_inh;
        let saved = std::mem::take(&mut self.level().scope);
        let r = (|| -> Result<(Plan, Vec<OutCol>)> {
            let (source, recheck) = self.dml_source(
                &t,
                &alias,
                &s.using_clause,
                s.where_clause.as_deref(),
                view.as_ref(),
            )?;
            let width = super::optimize::width(&source);
            let (returning, cols, old_new) =
                self.bind_returning(&s.returning_clause, (&alias, width, 0, t.columns.len()))?;
            let mut source = source;
            self.rls_sys = self.want_sys.then_some(t.columns.len() + 1);
            if let Some(q) = self.rls_using(&t, super::rls::DELETE)? {
                let mut quals = vec![q];
                if self.reads_table(t.oid) {
                    // (list_append_unique: the same quals once.)
                    if let Some(q) = self.rls_using(&t, super::rls::SELECT)?
                        && !quals.iter().any(|x| x.same_as(&q))
                    {
                        quals.push(q);
                    }
                }
                source = super::optimize::add_security_filter(&self.cat, source, Expr::And(quals));
            }
            let indexes = self.bound_indexes(&t)?;
            let triggers = self.bound_triggers(&t)?;
            let shown = view.is_none().then(|| alias.clone());
            let mut members = Vec::new();
            if inh {
                for child in self.dml_members(&t, &source) {
                    let mut triggers = self.bound_triggers(&child)?;
                    // Statement triggers fire for the table named in the statement only.
                    triggers.retain(|tr| tr.trig.tgtype & crate::catalog::trigger_type::ROW != 0);
                    members.push(crate::sql::plan::DeleteMember {
                        indexes: self.bound_indexes(&child)?,
                        triggers,
                        map: column_map(&t, &child),
                        table: child,
                    });
                }
            }
            let view = self.cat.views.contains_key(&t.oid);
            let plan = DeletePlan {
                table: t.clone(),
                triggers,
                source: Arc::new(source),
                returning,
                old_new,
                recheck,
                indexes,
                view,
                sys: self.want_sys,
                inh,
                members,
                alias: shown,
            };
            Ok((Plan::Delete(Box::new(plan)), cols))
        })();
        self.level().scope = saved;
        r
    }
}

/// A parent's columns as a member's (by name; none for a dropped column or one the member
/// lacks).
fn column_map(parent: &Table, child: &Table) -> Vec<Option<usize>> {
    parent
        .columns
        .iter()
        .map(|c| {
            if c.dropped {
                None
            } else {
                child.column_index(&c.name)
            }
        })
        .collect()
}

fn merge_when(w: &pb::MergeWhenClause) -> MergeWhen {
    match pb::MergeMatchKind::try_from(w.match_kind) {
        Ok(pb::MergeMatchKind::MergeWhenNotMatchedBySource) => MergeWhen::NotMatchedBySource,
        Ok(pb::MergeMatchKind::MergeWhenNotMatchedByTarget) => MergeWhen::NotMatchedByTarget,
        _ => MergeWhen::Matched,
    }
}

fn merge_cmd(w: &pb::MergeWhenClause) -> pb::CmdType {
    pb::CmdType::try_from(w.command_type).unwrap_or(pb::CmdType::CmdNothing)
}

/// `VALUES (...), (...)` with nothing else attached.
fn is_plain_values(s: &pb::SelectStmt) -> bool {
    !s.values_lists.is_empty()
        && s.sort_clause.is_empty()
        && s.limit_count.is_none()
        && s.limit_offset.is_none()
        && s.with_clause.is_none()
}

/// Point the DML target's scan at a cursor's current row.
fn set_current_of(p: &mut Plan, table: u32, cursor: Expr) -> bool {
    match p {
        Plan::Scan(s) if s.with_key && s.table.oid == table => {
            s.current_of = Some(cursor);
            true
        }
        Plan::Join { left, .. } => set_current_of(Arc::make_mut(left), table, cursor),
        Plan::Filter { input, .. } | Plan::Project { input, .. } => {
            set_current_of(Arc::make_mut(input), table, cursor)
        }
        // An inheritance tree: every member's scan (the one holding the cursor's row
        // returns it).
        Plan::Append {
            inh: Some(_),
            inputs,
            ..
        } => {
            fn member(p: &Plan) -> Option<u32> {
                match p {
                    Plan::Scan(s) if s.with_key => Some(s.table.oid),
                    Plan::Filter { input, .. } | Plan::Project { input, .. } => member(input),
                    _ => None,
                }
            }
            let mut any = false;
            for i in inputs.iter_mut() {
                if let Some(oid) = member(i) {
                    any |= set_current_of(Arc::make_mut(i), oid, cursor.clone());
                }
            }
            any
        }
        _ => false,
    }
}

impl Binder<'_, '_> {
    /// `WHERE CURRENT OF name`: in PL/pgSQL `name` is a cursor variable holding the portal's
    /// name; otherwise it is the portal's name.
    fn cursor_name(&mut self, name: &str) -> Expr {
        if let Some(&fl) = self.fn_levels.last()
            && let Some(args) = &self.levels[fl].fn_args
            && let Some(i) = self.levels[fl].fn_arg_index(name)
        {
            let e = Expr::Outer(1, i, args[i].1.clone());
            return self.lift(fl, e);
        }
        Expr::Const(crate::types::Value::Text(name.into()), DataType::Text)
    }
}

/// An ON CONFLICT inference element: a column or expression, with the collation and the
/// btree operator class (family, input type) it asks for.
pub(crate) struct InferElem {
    expr: Expr,
    coll: Option<String>,
    opclass: Option<(u32, u32)>,
}

/// An analyzed ON CONFLICT target.
enum Inferred {
    /// ON CONSTRAINT: its index.
    Indexes(Vec<u32>),
    /// Inference elements and the WHERE clause, matched against the indexes when planned.
    Elems(Vec<InferElem>, Option<Expr>),
}

/// Where a value's transformed expression is (exprLocation after analysis): a row cast to a
/// composite type is a RowExpr at the `::` (coerce_record_to_complex); any other cast's
/// leftmost part (a coerced literal keeps the literal's location, a cast function or
/// relabeling starts at its argument).
fn value_location(n: &Node) -> i32 {
    match inner(n) {
        Some(NodeEnum::TypeCast(tc))
            if tc.location >= 0
                && matches!(tc.arg.as_deref().and_then(inner), Some(NodeEnum::RowExpr(_))) =>
        {
            tc.location
        }
        _ => ast::location(n),
    }
}

/// Per key column of an index, the collation its entries are keyed under when that doesn't
/// sort by bytes or is nondeterministic (`keys::KeyColl`): the index's COLLATE, else the
/// column's, else the expression's (a COLLATE in it, else its columns' one collation), else
/// the database default; a range's, its subtype collation. None: by bytes (btree and hash
/// indexes only; pattern and made operator classes compare bytes).
pub(crate) fn key_colls(
    cat: &crate::catalog::Catalog,
    t: &Table,
    ix: &crate::catalog::Index,
    keys: &[Expr],
) -> Vec<Option<crate::storage::keys::KeyColl>> {
    use crate::types::collation;
    if !matches!(ix.method.as_str(), "btree" | "hash") {
        return Vec::new();
    }
    let v: Vec<_> = ix
        .columns
        .iter()
        .zip(keys)
        .map(|(c, k)| {
            if matches!(c.opclass, 4217..=4219)
                || (c.opclass != 0 && cat.opclasses.values().any(|o| o.oid == c.opclass && !o.default))
            {
                return None;
            }
            let ty = k.ty();
            let elem = match &ty {
                DataType::Array(e) => (**e).clone(),
                t => t.clone(),
            };
            let coll = match &elem {
                DataType::Range(rt) | DataType::Multirange(rt) if rt.collation != 0 && rt.subtype.collatable() => {
                    if rt.collation == collation::DEFAULT_OID && rt.coll_def.is_none() {
                        Some(collation::default_for(cat))
                    } else {
                        rt.coll()
                    }
                }
                _ if ty.collatable() => {
                    let name = c.collation.clone().or_else(|| expr_collation(t, k)).unwrap_or_else(|| "default".into());
                    collation::by_name(cat, &name)
                }
                _ => None,
            };
            let coll = coll.filter(|c| !c.bytewise() || !c.deterministic)?;
            Some(crate::storage::keys::KeyColl {
                coll,
                bpchar: matches!(elem, DataType::Bpchar(_)),
            })
        })
        .collect();
    if v.iter().any(Option::is_some) { v } else { Vec::new() }
}

/// `key_colls` of an index over plain columns (its expression keys keyed by bytes).
pub(crate) fn column_key_colls(
    cat: &crate::catalog::Catalog,
    t: &Table,
    ix: &crate::catalog::Index,
) -> Vec<Option<crate::storage::keys::KeyColl>> {
    let keys: Vec<Expr> = ix
        .columns
        .iter()
        .map(|c| match c.key {
            IndexKey::Column(i) => Expr::Col(i, t.columns[i].ty.clone()),
            IndexKey::Expr(_) => Expr::Const(Value::Null, DataType::Int4),
        })
        .collect();
    key_colls(cat, t, ix, &keys)
}

/// The collation of an index expression without a COLLATE clause of its own (as far as it
/// isn't the default): a COLLATE in it, else the one collation of its collatable columns.
fn expr_collation(t: &Table, k: &Expr) -> Option<String> {
    let mut e = k;
    loop {
        match e {
            Expr::Collate(_, name, _) => return Some(name.clone()),
            Expr::Relabel(x, _) => e = x,
            _ => break,
        }
    }
    let mut found: Option<String> = None;
    let mut many = false;
    k.walk(&mut |x| {
        if let Expr::Col(i, ty) = x
            && ty.collatable()
        {
            let c = t.columns.get(*i).and_then(|c| c.collation.clone()).unwrap_or_else(|| "default".into());
            match &found {
                Some(f) if *f != c => many = true,
                _ => found = Some(c),
            }
        }
    });
    if many { None } else { found }
}
