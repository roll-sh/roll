//! `print_function_sqlbody`: a SQL-standard function body printed from its statements, and the
//! INSERT / UPDATE / DELETE they may be (`get_insert_query_def` …), over the view printer.

use super::super::{FN_ARGS, NO_INDENT, RTABLES, STD};
use super::{Nest, Nested, ViewLevel, Views, column_names, from_slots, merged_columns};
use crate::catalog::{Function, Table, quote_ident};
use crate::exec::ExecCtx;
use crate::sql::binder::{Binder, Clause, Level, Scope, ast, table_rel};
use crate::sql::expr::Expr;
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// Clears the printer's per-thread state when the body is done.
struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        FN_ARGS.set(None);
        RTABLES.set(0);
        NO_INDENT.set(false);
    }
}

/// `RETURN expr` (printed without indentation, as Postgres does) or `BEGIN ATOMIC … END`.
pub(in crate::sql::ruleutils) fn function_body(ctx: &ExecCtx, f: &Function) -> Option<String> {
    let kind = f.sql_body?;
    let stmts = crate::sql::parse::parse(&f.body).ok()?;
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    b.level().barrier = true;
    // The body's level: its arguments are parameters, printed by name.
    let mut lvl = Level::new();
    lvl.barrier = true;
    lvl.fn_name = f.name.clone();
    lvl.params = f
        .args
        .iter()
        .enumerate()
        .map(|(i, (_, t))| Expr::Param(i, t.clone()))
        .collect();
    lvl.fn_args = Some(f.args.clone());
    b.levels.push(lvl);
    b.fn_levels.push(1);
    let _reset = Reset;
    FN_ARGS.set(Some((
        f.name.clone(),
        f.args.iter().map(|(n, _)| n.clone()).collect(),
    )));
    RTABLES.set(0);
    let mut v = Views {
        b: &mut b,
        ctx,
        pretty: false,
        wrap: 0,
        used: Vec::new(),
        enclosing: vec![Vec::new()],
    };
    if kind == 'r' {
        NO_INDENT.set(true);
        v.wrap = usize::MAX;
        let [stmt] = stmts.as_slice() else {
            return None;
        };
        let mut buf = String::new();
        v.return_stmt(stmt.stmt.as_deref()?, &mut buf, 0)?;
        return Some(buf);
    }
    let mut buf = String::from("BEGIN ATOMIC\n");
    for (i, stmt) in stmts.iter().enumerate() {
        let node = stmt.stmt.as_deref()?;
        v.used.clear();
        match node.node.as_ref()? {
            NodeEnum::SelectStmt(_) if f.atomic_returns.contains(&i) => {
                v.return_stmt(node, &mut buf, 1)?
            }
            NodeEnum::SelectStmt(_) => v.query(node, &mut buf, 1, &[], Nest::Body)?,
            NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_) => v.dml(node, &mut buf, 1)?,
            _ => return None,
        }
        buf.push_str(";\n");
    }
    buf.push_str("END");
    Some(buf)
}

/// An UPDATE (or ON CONFLICT DO UPDATE) assignment.
enum SetItem {
    /// `col[subscripts].field = expr`: the column, its indirection, the value.
    One(String, Vec<Indir>, Expr),
    /// `(col, …) = (SELECT …)`: the targets and the subquery.
    Multi(Vec<(String, Vec<Indir>)>, Box<pb::Node>),
}

/// A target column's subscript or field.
enum Indir {
    Subscript(Option<Expr>, Option<Expr>, bool),
    Field(String),
}

/// A MERGE action's command.
enum MergeCmd {
    /// The target columns, OVERRIDING, and the values (None: DEFAULT VALUES).
    Insert(Vec<(String, Vec<Indir>)>, &'static str, Option<Vec<Expr>>),
    Update(Vec<SetItem>),
    Delete,
    Nothing,
}

/// Where an INSERT's rows come from.
enum Source<'n> {
    Default,
    /// A single VALUES row, coerced to the target columns.
    Row(Vec<Expr>),
    /// Several VALUES rows.
    Rows(Vec<Vec<Expr>>),
    Select(&'n pb::Node),
}

impl Views<'_, '_, '_> {
    /// `RETURN expr` (kept as `SELECT expr`) at `indent`.
    fn return_stmt(&mut self, n: &pb::Node, buf: &mut String, indent: i32) -> Option<()> {
        let Some(NodeEnum::SelectStmt(s)) = n.node.as_ref() else {
            return None;
        };
        let [t] = s.target_list.as_slice() else {
            return None;
        };
        let Some(NodeEnum::ResTarget(rt)) = t.node.as_ref() else {
            return None;
        };
        let val = rt.val.as_deref()?;
        let lvl = self.b.level();
        let saved = std::mem::take(&mut lvl.st);
        let e = self.b.in_clause(Clause::Select, |b| b.bind_expr(val)).ok();
        let aggs = std::mem::replace(&mut self.b.level().st, saved).aggs;
        let e = e?;
        let e = if matches!(e.ty(), DataType::Unknown) {
            self.b.coerce_implicit(e, &DataType::Text).ok()?
        } else {
            e
        };
        let level = ViewLevel {
            outer: self.outer_names(),
            ..ViewLevel::default()
        };
        self.print(&level, indent, buf, &aggs, |d| {
            if !NO_INDENT.get() {
                d.buf.push(' ');
                d.indent += STD;
            }
            d.buf.push_str("RETURN ");
            d.expr(&e, false)
        })
    }

    /// An INSERT, UPDATE or DELETE at `indent`: a query level of its own (with a range table),
    /// its WITH queries visible.
    pub(super) fn dml(&mut self, n: &pb::Node, buf: &mut String, indent: i32) -> Option<()> {
        let with = match n.node.as_ref()? {
            NodeEnum::InsertStmt(s) => s.with_clause.as_ref(),
            NodeEnum::UpdateStmt(s) => s.with_clause.as_ref(),
            NodeEnum::DeleteStmt(s) => s.with_clause.as_ref(),
            NodeEnum::MergeStmt(s) => s.with_clause.as_ref(),
            _ => return None,
        };
        let lvl = self.b.level();
        let saved = (std::mem::take(&mut lvl.scope), std::mem::take(&mut lvl.st));
        let used = self.used.len();
        RTABLES.set(RTABLES.get() + 1);
        if with.is_some() {
            self.b.ctes.push(Vec::new());
        }
        let r = self.dml_body(n, with, buf, indent);
        if with.is_some() {
            self.b.ctes.pop();
        }
        RTABLES.set(RTABLES.get() - 1);
        self.used.truncate(used);
        let lvl = self.b.level();
        (lvl.scope, lvl.st) = saved;
        r
    }

    fn dml_body(
        &mut self,
        n: &pb::Node,
        with: Option<&pb::WithClause>,
        buf: &mut String,
        indent: i32,
    ) -> Option<()> {
        let mut indent = indent;
        if let Some(w) = with {
            // The statement's own target is named first (`set_rtable_names` runs over its
            // range table before the WITH queries').
            let rv = match n.node.as_ref()? {
                NodeEnum::InsertStmt(s) => s.relation.as_ref(),
                NodeEnum::UpdateStmt(s) => s.relation.as_ref(),
                NodeEnum::DeleteStmt(s) => s.relation.as_ref(),
                NodeEnum::MergeStmt(s) => s.relation.as_ref(),
                _ => None,
            };
            let own = rv
                .map(|rv| rv.alias.as_ref().map_or(&rv.relname, |a| &a.aliasname).clone())
                .filter(|n| !self.used.contains(n));
            if let Some(o) = &own {
                self.used.push(o.clone());
            }
            self.b.define_ctes(w).ok()?;
            self.with_clause(w, buf, &mut indent)?;
            if let Some(o) = &own {
                self.used.retain(|x| x != o);
            }
        }
        match n.node.as_ref()? {
            NodeEnum::InsertStmt(s) => self.insert(s, buf, indent),
            NodeEnum::UpdateStmt(s) => self.update(s, buf, indent),
            NodeEnum::DeleteStmt(s) => self.delete(s, buf, indent),
            NodeEnum::MergeStmt(s) => self.merge(s, buf, indent),
            _ => None,
        }
    }

    /// The table a statement writes.
    fn target_table(&self, rv: &pb::RangeVar) -> Option<Arc<Table>> {
        let full = if rv.schemaname.is_empty() {
            quote_ident(&rv.relname)
        } else {
            format!(
                "{}.{}",
                quote_ident(&rv.schemaname),
                quote_ident(&rv.relname)
            )
        };
        let oid = self.ctx.fmt().names.clone()?.rel_oid(&full).ok()?;
        self.b.cat.tables.get(&oid).cloned()
    }

    /// A range-table name for `base`, unique among the levels being printed.
    fn claim(&mut self, base: &str) -> String {
        let mut name = base.to_string();
        let mut i = 0;
        while self.used.contains(&name) {
            i += 1;
            name = crate::catalog::suffixed_name(base, &format!("_{i}"));
        }
        self.used.push(name.clone());
        name
    }

    /// A WHERE-like condition, bound and made boolean.
    fn condition(&mut self, n: Option<&pb::Node>) -> Option<Option<Expr>> {
        let Some(n) = n else { return Some(None) };
        self.b
            .in_clause(Clause::Where, |b| {
                let e = b.bind_expr(n)?;
                b.coerce_bool(e, "WHERE")
            })
            .ok()
            .map(Some)
    }

    /// SET items (`get_update_query_targetlist_def`): plain `col = expr` only.
    fn set_list(&mut self, list: &[pb::Node], t: &Arc<Table>) -> Option<Vec<SetItem>> {
        let mut plain = true;
        for item in list {
            let Some(NodeEnum::ResTarget(rt)) = item.node.as_ref() else {
                return None;
            };
            if !rt.indirection.is_empty()
                || matches!(
                    rt.val.as_deref().and_then(|v| v.node.as_ref()),
                    Some(NodeEnum::MultiAssignRef(_) | NodeEnum::SetToDefault(_))
                )
            {
                plain = false;
            }
        }
        if plain {
            let sets = self.b.bind_set_list(list, t, None, 0).ok()?;
            return sets
                .into_iter()
                .map(|(c, e)| Some(SetItem::One(t.columns.get(c)?.name.clone(), Vec::new(), e)))
                .collect();
        }
        // Item by item: `(a, b[1]) = (SELECT …)` stays a multiple assignment; `(a, b) =
        // (x, y)` is two (as Postgres stores them).
        let mut out = Vec::new();
        let mut i = 0;
        while i < list.len() {
            let Some(NodeEnum::ResTarget(rt)) = list[i].node.as_ref() else {
                return None;
            };
            let (name, indir, ty) = self.set_target(rt, t)?;
            match rt.val.as_deref().and_then(|v| v.node.as_ref())? {
                NodeEnum::MultiAssignRef(m) => {
                    let n = usize::try_from(m.ncolumns).ok()?;
                    match m.source.as_deref().and_then(|v| v.node.as_ref())? {
                        NodeEnum::SubLink(sl) => {
                            let mut targets = vec![(name, indir)];
                            for item in list.get(i + 1..i + n)? {
                                let Some(NodeEnum::ResTarget(rt)) = item.node.as_ref() else {
                                    return None;
                                };
                                let (name, indir, _) = self.set_target(rt, t)?;
                                targets.push((name, indir));
                            }
                            out.push(SetItem::Multi(targets, Box::new(sl.subselect.as_deref()?.clone())));
                            i += n;
                            continue;
                        }
                        NodeEnum::RowExpr(r) => {
                            for k in 0..n {
                                let Some(NodeEnum::ResTarget(rt)) = list.get(i + k)?.node.as_ref()
                                else {
                                    return None;
                                };
                                let (name, indir, ty) = self.set_target(rt, t)?;
                                let e = self.set_value(r.args.get(k)?, &ty, &name)?;
                                out.push(SetItem::One(name, indir, e));
                            }
                            i += n;
                            continue;
                        }
                        _ => return None,
                    }
                }
                NodeEnum::SetToDefault(_) => out.push(SetItem::One(name, indir, Expr::Default(ty))),
                _ => {
                    let e = self.set_value(rt.val.as_deref()?, &ty, &name)?;
                    out.push(SetItem::One(name, indir, e));
                }
            }
            i += 1;
        }
        Some(out)
    }

    /// An assignment's target: the column, its subscripts and fields, and the type assigned.
    fn set_target(&mut self, rt: &pb::ResTarget, t: &Table) -> Option<(String, Vec<Indir>, DataType)> {
        let col = t.columns.get(t.column_index(&rt.name)?)?;
        let mut ty = col.ty.clone();
        let mut out = Vec::new();
        for n in &rt.indirection {
            match n.node.as_ref()? {
                NodeEnum::AIndices(ix) => {
                    let mut bind = |n: Option<&pb::Node>| -> Option<Option<Expr>> {
                        match n {
                            None => Some(None),
                            Some(n) => {
                                let e = self.b.bind_expr(n).ok()?;
                                Some(Some(self.b.coerce_implicit(e, &DataType::Int4).ok()?))
                            }
                        }
                    };
                    let lo = bind(ix.lidx.as_deref())?;
                    let hi = bind(ix.uidx.as_deref())?;
                    if !ix.is_slice && let DataType::Array(e) = &ty {
                        ty = (**e).clone();
                    }
                    out.push(Indir::Subscript(lo, hi, ix.is_slice));
                }
                NodeEnum::String(f) => {
                    let DataType::Row(fields) = &ty else {
                        return None;
                    };
                    ty = fields.iter().find(|(n, _)| *n == f.sval)?.1.clone();
                    out.push(Indir::Field(f.sval.clone()));
                }
                _ => return None,
            }
        }
        Some((col.name.clone(), out, ty))
    }

    /// An assigned value, coerced as the assignment does.
    fn set_value(&mut self, n: &pb::Node, ty: &DataType, name: &str) -> Option<Expr> {
        let e = self.b.in_clause(Clause::Other, |b| b.bind_expr(n)).ok()?;
        self.b.coerce_assign(e, ty, name).ok()
    }

    /// RETURNING's select list, bound over `scope`.
    fn returning(
        &mut self,
        rc: &Option<pb::ReturningClause>,
        scope: &Scope,
    ) -> Option<Vec<(Expr, String, Option<String>)>> {
        if rc.as_ref().is_some_and(|r| !r.options.is_empty()) {
            return None;
        }
        let items = ast::returning(rc);
        if items.is_empty() {
            return Some(Vec::new());
        }
        let s = pb::SelectStmt {
            target_list: items.to_vec(),
            ..Default::default()
        };
        Some(self.bind_select(&s, scope, &Nest::Body)?.targets)
    }

    /// RETURNING of an UPDATE, DELETE or MERGE (`merge`): its items over `scope` plus the
    /// target's old and new rows (as WITH names them), and MERGE_ACTION(); with how those
    /// read (row position, shown name) and the WITH clause to show.
    /// The last item is the scope RETURNING was bound over, which its sublinks are bound
    /// over again when printed.
    #[allow(clippy::type_complexity)]
    fn returning_old_new(
        &mut self,
        rc: &Option<pb::ReturningClause>,
        scope: &Scope,
        t: &Arc<Table>,
        merge: bool,
    ) -> Option<(Vec<(Expr, String, Option<String>)>, Vec<(usize, String)>, String, Scope)> {
        let items = ast::returning(rc);
        if items.is_empty() {
            return Some((Vec::new(), Vec::new(), String::new(), self.b.scope().clone()));
        }
        let (mut old, mut new) = ("old".to_string(), "new".to_string());
        // A default name taken already (a rule action's OLD / NEW) isn't RETURNING's.
        let taken = |b: &Binder, n: &str| {
            b.levels
                .iter()
                .any(|l| l.scope.rels.iter().any(|r| !r.anonymous && r.alias == n))
        };
        let (mut old_on, mut new_on) = (!taken(self.b, "old"), !taken(self.b, "new"));
        let mut with = Vec::new();
        for o in rc.iter().flat_map(|r| &r.options) {
            let Some(NodeEnum::ReturningOption(ro)) = o.node.as_ref() else {
                return None;
            };
            if ro.option == pb::ReturningOptionKind::ReturningOptionOld as i32 {
                old = ro.value.clone();
                old_on = true;
            } else {
                new = ro.value.clone();
                new_on = true;
            }
        }
        if old != "old" {
            with.push(format!("OLD AS {}", quote_ident(&old)));
        }
        if new != "new" {
            with.push(format!("NEW AS {}", quote_ident(&new)));
        }
        let width = scope
            .rels
            .iter()
            .flat_map(|r| r.cols.iter())
            .filter_map(|c| match c.expr {
                Expr::Col(i, _) => Some(i + 1),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let n = t.columns.len();
        let mut extra = Vec::new();
        // MERGE: the source's columns come first in `*` (its range table's order).
        let mut rels: Vec<crate::sql::binder::ScopeRel> = if merge {
            let mut v = scope.rels.clone();
            if !v.is_empty() {
                let target = v.remove(0);
                v.push(target);
            }
            v
        } else {
            scope.rels.clone()
        };
        for (k, (name, on)) in [(old, old_on), (new, new_on)].into_iter().enumerate() {
            if !on {
                continue;
            }
            let at = width + k * n;
            let mut r = table_rel(t, &name, None);
            for c in &mut r.cols {
                c.expr = c.expr.clone().shift_cols(at as isize);
                if !c.hidden {
                    extra.push((
                        match c.expr {
                            Expr::Col(i, _) => i,
                            _ => continue,
                        },
                        format!("{}.{}", quote_ident(&name), quote_ident(&c.name)),
                    ));
                }
            }
            // (Not part of `*`, nor read unqualified.)
            r.qualified_only = true;
            rels.push(r);
        }
        let action = width + 2 * n;
        extra.push((action, "MERGE_ACTION()".to_string()));
        let lvl = self.b.levels.len() - 1;
        let saved = std::mem::replace(&mut self.b.level().scope, Scope { rels });
        let had = self.b.merge_action;
        if merge {
            self.b.merge_action = Some((lvl, action));
        }
        let s = pb::SelectStmt {
            target_list: items.to_vec(),
            ..Default::default()
        };
        let scope_now = self.b.scope().clone();
        let targets = self.bind_select(&s, &scope_now, &Nest::Body).map(|x| x.targets);
        self.b.merge_action = had;
        self.b.level().scope = saved;
        let with = if with.is_empty() {
            String::new()
        } else {
            format!(" WITH ({})", with.join(", "))
        };
        Some((targets?, extra, with, scope_now))
    }

    /// Print a data-modifying statement with its RETURNING's scope in place (`ext`): a
    /// sublink there reads the old / new rows.
    fn print_returning(
        &mut self,
        ext: Scope,
        level: &ViewLevel,
        indent: i32,
        buf: &mut String,
        f: impl FnOnce(&mut super::super::Deparser) -> Option<()>,
    ) -> Option<()> {
        let saved = std::mem::replace(&mut self.b.level().scope, ext);
        let r = self.print(level, indent, buf, &[], f);
        self.b.level().scope = saved;
        r
    }

    /// `get_insert_query_def`.
    fn insert(&mut self, s: &pb::InsertStmt, buf: &mut String, indent: i32) -> Option<()> {
        let rv = s.relation.as_ref()?;
        let t = self.target_table(rv)?;
        let refname = self.claim(rv.alias.as_ref().map_or(&rv.relname, |a| &a.aliasname));
        // The target columns: as listed, else the table's from the first.
        let mut cols: Vec<usize> = Vec::new();
        // (Per listed column: its subscripts and fields, and the type assigned.)
        let mut indirs: Vec<(Vec<Indir>, DataType)> = Vec::new();
        for c in &s.cols {
            let Some(NodeEnum::ResTarget(rt)) = c.node.as_ref() else {
                return None;
            };
            cols.push(t.column_index(&rt.name)?);
            let (_, indir, ty) = self.set_target(rt, &t)?;
            indirs.push((indir, ty));
        }
        let listed = !cols.is_empty();
        let live: Vec<usize> = t.live_columns().map(|(i, _)| i).collect();
        let source = match s.select_stmt.as_deref() {
            None => Source::Default,
            Some(node) => {
                let Some(NodeEnum::SelectStmt(sel)) = node.node.as_ref() else {
                    return None;
                };
                let plain = !sel.values_lists.is_empty()
                    && sel.with_clause.is_none()
                    && sel.sort_clause.is_empty()
                    && sel.limit_count.is_none()
                    && sel.limit_offset.is_none();
                if plain {
                    let mut rows = Vec::new();
                    for row in &sel.values_lists {
                        let Some(NodeEnum::List(l)) = row.node.as_ref() else {
                            return None;
                        };
                        let mut out = Vec::new();
                        for (k, item) in l.items.iter().enumerate() {
                            let c =
                                &t.columns[if listed { *cols.get(k)? } else { *live.get(k)? }];
                            let c = &crate::catalog::Column {
                                ty: indirs.get(k).map_or_else(|| c.ty.clone(), |x| x.1.clone()),
                                ..c.clone()
                            };
                            if matches!(item.node, Some(NodeEnum::SetToDefault(_))) {
                                out.push(Expr::Default(c.ty.clone()));
                                continue;
                            }
                            let e = self
                                .b
                                .in_clause(Clause::Values, |b| b.bind_expr(item))
                                .ok()?;
                            // One row's items print without their implicit coercion to the
                            // column (a typed constant stays as written).
                            if sel.values_lists.len() == 1
                                && let Expr::Const(_, ty) = &e
                                && !matches!(ty, DataType::Unknown)
                                && *ty != c.ty
                            {
                                self.b.coerce_assign(e.clone(), &c.ty, &c.name).ok()?;
                                out.push(e);
                                continue;
                            }
                            out.push(self.b.coerce_assign(e, &c.ty, &c.name).ok()?);
                        }
                        rows.push(out);
                    }
                    if !listed {
                        cols = live.get(..rows.first()?.len())?.to_vec();
                    }
                    if rows.len() == 1 {
                        Source::Row(rows.pop()?)
                    } else {
                        Source::Rows(rows)
                    }
                } else {
                    if !listed {
                        let n = self.b.in_sibling(|b| b.bind_select_stmt(sel)).ok()?.1.len();
                        cols = live.get(..n)?.to_vec();
                    }
                    Source::Select(node)
                }
            }
        };
        // ON CONFLICT: the arbiter, and DO UPDATE's assignments over the target and EXCLUDED.
        let width = t.columns.len();
        let rel = table_rel(&t, &refname, None);
        let excluded = Scope {
            rels: vec![table_rel(&t, "excluded", None)],
        }
        .shifted(width)
        .rels
        .pop()?;
        let conflict = match s.on_conflict_clause.as_deref() {
            None => None,
            Some(oc) => {
                let arbiter = match oc.infer.as_deref() {
                    None => String::new(),
                    Some(inf) if !inf.conname.is_empty() => {
                        format!(" ON CONSTRAINT {}", quote_ident(&inf.conname))
                    }
                    Some(inf) => {
                        let names: Option<Vec<String>> = inf
                            .index_elems
                            .iter()
                            .map(|e| match e.node.as_ref() {
                                Some(NodeEnum::IndexElem(ie)) if !ie.name.is_empty() => {
                                    let mut s = quote_ident(&ie.name);
                                    let coll = ast::names(&ie.collation);
                                    if let Some(c) = coll.last() {
                                        s.push_str(" COLLATE ");
                                        s.push_str(&match coll.as_slice() {
                                            [sc, c] if sc != "pg_catalog" => {
                                                format!("{}.{}", quote_ident(sc), quote_ident(c))
                                            }
                                            _ => format!("\"{}\"", c.replace('"', "\"\"")),
                                        });
                                    }
                                    if let Some(o) = ast::names(&ie.opclass).last() {
                                        s.push(' ');
                                        s.push_str(&quote_ident(o));
                                    }
                                    Some(s)
                                }
                                _ => None,
                            })
                            .collect();
                        let mut s = format!("({})", names?.join(", "));
                        // A partial index's predicate: the target's columns, unprefixed.
                        if let Some(w) = inf.where_clause.as_deref() {
                            self.b.level().scope = Scope {
                                rels: vec![rel.clone()],
                            };
                            let e = self.condition(Some(w))??;
                            let cols: Vec<String> =
                                t.columns.iter().map(|c| quote_ident(&c.name)).collect();
                            let text = super::super::deparse(&e, &cols, self.ctx, self.pretty)?;
                            s.push_str("\n  WHERE ");
                            s.push_str(&text);
                        }
                        s
                    }
                };
                let update = oc.action == pb::OnConflictAction::OnconflictUpdate as i32;
                let (sets, filter) = if update {
                    self.used.push("excluded".into());
                    self.b.level().scope = Scope {
                        rels: vec![rel.clone(), excluded.clone()],
                    };
                    (
                        self.set_list(&oc.target_list, &t)?,
                        self.condition(oc.where_clause.as_deref())?,
                    )
                } else {
                    (Vec::new(), None)
                };
                Some((arbiter, update, sets, filter))
            }
        };
        let scope = Scope {
            rels: vec![rel.clone()],
        };
        self.b.level().scope = scope.clone();
        let returning = self.returning(&s.returning_clause, &scope)?;
        let both = Scope {
            rels: vec![rel, excluded],
        };
        let names = [refname.clone(), "excluded".into()];
        let (cols_shown, qual) = column_names(&both, &names, true);
        let level = ViewLevel {
            cols: cols_shown,
            qual,
            outer: self.outer_names(),
            refs: vec![refname],
            rows: super::whole_rows(&both, &names),
            ..ViewLevel::default()
        };
        let overriding = match pb::OverridingKind::try_from(s.r#override).ok()? {
            pb::OverridingKind::OverridingSystemValue => "OVERRIDING SYSTEM VALUE ",
            pb::OverridingKind::OverridingUserValue => "OVERRIDING USER VALUE ",
            _ => "",
        };
        let wrap = self.wrap;
        self.print(&level, indent, buf, &[], |d| {
            d.buf.push(' ');
            d.indent += STD;
            d.buf.push_str("INSERT INTO ");
            d.relation(
                &pb::RangeVar {
                    alias: None,
                    ..rv.clone()
                },
                &t.name,
                false,
            )?;
            if rv.alias.is_some() {
                d.buf.push_str(" AS ");
                d.buf.push_str(&quote_ident(d.view?.refs.first()?));
            }
            d.buf.push(' ');
            if !matches!(source, Source::Default) {
                d.buf.push('(');
                for (k, &i) in cols.iter().enumerate() {
                    if k > 0 {
                        d.buf.push_str(", ");
                    }
                    let indir = indirs.get(k).map_or(&[][..], |x| &x.0[..]);
                    d.set_target(&t.columns[i].name, indir)?;
                }
                d.buf.push_str(") ");
            }
            d.buf.push_str(overriding);
            match &source {
                Source::Default => d.buf.push_str("DEFAULT VALUES"),
                Source::Row(es) => {
                    d.keyword("VALUES (", -STD, STD, 2);
                    for (i, e) in es.iter().enumerate() {
                        if i > 0 {
                            d.buf.push_str(", ");
                        }
                        d.expr_top(e, false)?;
                    }
                    d.buf.push(')');
                }
                Source::Rows(rows) => {
                    d.buf.push_str("VALUES ");
                    for (i, row) in rows.iter().enumerate() {
                        if i > 0 {
                            d.buf.push_str(", ");
                        }
                        d.buf.push('(');
                        for (j, e) in row.iter().enumerate() {
                            if j > 0 {
                                d.buf.push(',');
                            }
                            if d.whole_row(e).is_some() {
                                d.expr_toplevel(e)?;
                            } else {
                                d.expr(e, false)?;
                            }
                        }
                        d.buf.push(')');
                    }
                }
                Source::Select(node) => {
                    let qual = d.view?.qual.clone();
                    d.nested.as_deref_mut()?.query(
                        node,
                        &mut d.buf,
                        d.indent,
                        &qual,
                        Nest::Insert,
                    )?;
                }
            }
            if let Some((arbiter, update, sets, filter)) = &conflict {
                d.buf.push_str(" ON CONFLICT");
                d.buf.push_str(arbiter);
                if *update {
                    d.buf.push_str(" DO UPDATE SET ");
                    d.assignments(sets)?;
                    if let Some(w) = filter {
                        d.keyword(" WHERE ", -STD, STD, 1);
                        d.expr(w, false)?;
                    }
                } else {
                    d.buf.push_str(" DO NOTHING");
                }
            }
            d.returning_list(&returning, wrap)
        })
    }

    /// The target and FROM / USING items of an UPDATE or DELETE, bound: the scope's range-table
    /// names and the level printing it.
    fn dml_from(
        &mut self,
        rv: &pb::RangeVar,
        from: &[pb::Node],
    ) -> Option<(Arc<Table>, Scope, ViewLevel)> {
        let t = self.target_table(rv)?;
        let mut items = vec![pb::Node {
            node: Some(NodeEnum::RangeVar(rv.clone())),
        }];
        items.extend(from.iter().cloned());
        self.b.bind_from(&items, None).ok()?;
        let scope = self.b.scope().clone();
        let mut slots = Vec::new();
        for item in &items {
            from_slots(item, &mut slots, &|n| self.is_cte(n), &|j| {
                self.natural_using(j)
            })?;
        }
        let (refs, joins) = self.rel_names(&scope, &slots)?;
        let (cols, qual) = column_names(&scope, &refs, true);
        let merged = merged_columns(&scope, &slots, &refs);
        let level = ViewLevel {
            cols,
            qual,
            outer: self.outer_names(),
            refs,
            merged,
            joins,
            ..ViewLevel::default()
        };
        Some((t, scope, level))
    }

    /// `get_update_query_def`.
    fn update(&mut self, s: &pb::UpdateStmt, buf: &mut String, indent: i32) -> Option<()> {
        let rv = s.relation.as_ref()?;
        let (t, scope, mut level) = self.dml_from(rv, &s.from_clause)?;
        let sets = self.set_list(&s.target_list, &t)?;
        let where_ = self.condition(s.where_clause.as_deref())?;
        let (returning, extra, with, ext) =
            self.returning_old_new(&s.returning_clause, &scope, &t, false)?;
        extend_cols(&mut level, extra);
        // (Outer references bound since: a rule action's OLD / NEW.)
        level.outer = self.outer_names();
        let wrap = self.wrap;
        self.print_returning(ext, &level, indent, buf, |d| {
            d.buf.push(' ');
            d.indent += STD;
            d.buf.push_str("UPDATE ");
            let refname = d.view?.refs.first()?.clone();
            d.relation(rv, &refname, false)?;
            d.buf.push_str(" SET ");
            d.assignments(&sets)?;
            d.print_from_list(" FROM ", &s.from_clause, 1, wrap)?;
            if let Some(w) = &where_ {
                d.keyword(" WHERE ", -STD, STD, 1);
                d.expr(w, false)?;
            }
            d.returning_list_with(&returning, &with, wrap)
        })
    }

    /// `get_merge_query_def`.
    fn merge(&mut self, s: &pb::MergeStmt, buf: &mut String, indent: i32) -> Option<()> {
        use pb::{CmdType as C, MergeMatchKind as K};
        let rv = s.relation.as_ref()?;
        let source = s.source_relation.as_deref()?;
        let (t, scope, mut level) = self.dml_from(rv, std::slice::from_ref(source))?;
        let on = self.condition(s.join_condition.as_deref())??;
        let mut actions = Vec::new();
        let mut by_source = false;
        for w in &s.merge_when_clauses {
            let Some(NodeEnum::MergeWhenClause(w)) = w.node.as_ref() else {
                return None;
            };
            let kind = K::try_from(w.match_kind).ok()?;
            by_source |= kind == K::MergeWhenNotMatchedBySource;
            let cond = self.condition(w.condition.as_deref())?;
            let cmd = match C::try_from(w.command_type).ok()? {
                C::CmdInsert => {
                    let mut targets = Vec::new();
                    if w.target_list.is_empty() {
                        for (_, c) in t.live_columns().take(w.values.len()) {
                            targets.push((c.name.clone(), Vec::new(), c.ty.clone()));
                        }
                    } else {
                        for n in &w.target_list {
                            let Some(NodeEnum::ResTarget(rt)) = n.node.as_ref() else {
                                return None;
                            };
                            targets.push(self.set_target(rt, &t)?);
                        }
                    }
                    let values = if w.values.is_empty() {
                        None
                    } else {
                        let mut out = Vec::new();
                        for (v, (name, _, ty)) in w.values.iter().zip(&targets) {
                            out.push(match v.node.as_ref()? {
                                NodeEnum::SetToDefault(_) => Expr::Default(ty.clone()),
                                _ => self.set_value(v, ty, name)?,
                            });
                        }
                        Some(out)
                    };
                    let overriding = match pb::OverridingKind::try_from(w.r#override).ok()? {
                        pb::OverridingKind::OverridingSystemValue => " OVERRIDING SYSTEM VALUE",
                        pb::OverridingKind::OverridingUserValue => " OVERRIDING USER VALUE",
                        _ => "",
                    };
                    let cols = targets.into_iter().map(|(n, i, _)| (n, i)).collect();
                    MergeCmd::Insert(cols, overriding, values)
                }
                C::CmdUpdate => MergeCmd::Update(self.set_list(&w.target_list, &t)?),
                C::CmdDelete => MergeCmd::Delete,
                C::CmdNothing => MergeCmd::Nothing,
                _ => return None,
            };
            actions.push((kind, cond, cmd));
        }
        let (returning, extra, with, ext) =
            self.returning_old_new(&s.returning_clause, &scope, &t, true)?;
        extend_cols(&mut level, extra);
        level.outer = self.outer_names();
        let wrap = self.wrap;
        self.print_returning(ext, &level, indent, buf, |d| {
            d.buf.push(' ');
            d.indent += STD;
            d.buf.push_str("MERGE INTO ");
            let refname = d.view?.refs.first()?.clone();
            d.relation(rv, &refname, false)?;
            d.print_from_list(" USING ", std::slice::from_ref(source), 1, wrap)?;
            d.keyword(" ON ", -STD, STD, 2);
            d.expr(&on, false)?;
            for (kind, cond, cmd) in &actions {
                d.keyword(" WHEN ", -STD, STD, 2);
                d.buf.push_str(match kind {
                    K::MergeWhenMatched => "MATCHED",
                    K::MergeWhenNotMatchedBySource => "NOT MATCHED BY SOURCE",
                    _ if by_source => "NOT MATCHED BY TARGET",
                    _ => "NOT MATCHED",
                });
                if let Some(c) = cond {
                    d.keyword(" AND ", -STD, STD, 3);
                    d.expr(c, false)?;
                }
                d.keyword(" THEN ", -STD, STD, 3);
                match cmd {
                    MergeCmd::Insert(cols, overriding, values) => {
                        d.buf.push_str("INSERT");
                        if !cols.is_empty() {
                            d.buf.push_str(" (");
                            for (k, (name, indir)) in cols.iter().enumerate() {
                                if k > 0 {
                                    d.buf.push_str(", ");
                                }
                                d.set_target(name, indir)?;
                            }
                            d.buf.push(')');
                        }
                        d.buf.push_str(overriding);
                        match values {
                            Some(vs) => {
                                d.keyword(" VALUES (", -STD, STD, 4);
                                for (k, e) in vs.iter().enumerate() {
                                    if k > 0 {
                                        d.buf.push_str(", ");
                                    }
                                    d.expr_top(e, false)?;
                                }
                                d.buf.push(')');
                            }
                            None => d.buf.push_str(" DEFAULT VALUES"),
                        }
                    }
                    MergeCmd::Update(sets) => {
                        d.buf.push_str("UPDATE SET ");
                        d.assignments(sets)?;
                    }
                    MergeCmd::Delete => d.buf.push_str("DELETE"),
                    MergeCmd::Nothing => d.buf.push_str("DO NOTHING"),
                }
            }
            d.returning_list_with(&returning, &with, wrap)
        })
    }

    /// `get_delete_query_def`.
    fn delete(&mut self, s: &pb::DeleteStmt, buf: &mut String, indent: i32) -> Option<()> {
        let rv = s.relation.as_ref()?;
        let (t, scope, mut level) = self.dml_from(rv, &s.using_clause)?;
        let where_ = self.condition(s.where_clause.as_deref())?;
        let (returning, extra, with, ext) =
            self.returning_old_new(&s.returning_clause, &scope, &t, false)?;
        extend_cols(&mut level, extra);
        level.outer = self.outer_names();
        let wrap = self.wrap;
        self.print_returning(ext, &level, indent, buf, |d| {
            d.buf.push(' ');
            d.indent += STD;
            d.buf.push_str("DELETE FROM ");
            let refname = d.view?.refs.first()?.clone();
            d.relation(rv, &refname, false)?;
            d.print_from_list(" USING ", &s.using_clause, 1, wrap)?;
            if let Some(w) = &where_ {
                d.keyword(" WHERE ", -STD, STD, 1);
                d.expr(w, false)?;
            }
            d.returning_list_with(&returning, &with, wrap)
        })
    }
}

impl super::super::Deparser<'_, '_> {
    /// `col = expr, …` (`get_update_query_targetlist_def`).
    fn assignments(&mut self, sets: &[SetItem]) -> Option<()> {
        for (i, item) in sets.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            match item {
                SetItem::One(name, indir, e) => {
                    self.set_target(name, indir)?;
                    self.buf.push_str(" = ");
                    self.expr(e, false)?;
                }
                SetItem::Multi(targets, query) => {
                    self.buf.push('(');
                    for (k, (name, indir)) in targets.iter().enumerate() {
                        if k > 0 {
                            self.buf.push_str(", ");
                        }
                        self.set_target(name, indir)?;
                    }
                    self.buf.push_str(") = (");
                    let qual = self.view?.qual.clone();
                    let nested = self.nested.as_deref_mut()?;
                    nested.query(query, &mut self.buf, self.indent, &qual, Nest::Sublink)?;
                    self.buf.push(')');
                }
            }
        }
        Some(())
    }

    /// An assignment's target column with its subscripts and fields.
    fn set_target(&mut self, name: &str, indir: &[Indir]) -> Option<()> {
        self.buf.push_str(&quote_ident(name));
        for i in indir {
            match i {
                Indir::Subscript(lo, hi, slice) => {
                    self.buf.push('[');
                    // (A plain subscript is the upper bound's.)
                    if let Some(lo) = lo {
                        self.expr(lo, true)?;
                    }
                    if *slice {
                        self.buf.push(':');
                    }
                    if let Some(hi) = hi {
                        self.expr(hi, true)?;
                    }
                    self.buf.push(']');
                }
                Indir::Field(f) => {
                    self.buf.push('.');
                    self.buf.push_str(&quote_ident(f));
                }
            }
        }
        Some(())
    }

    /// `get_returning_clause`.
    fn returning_list(
        &mut self,
        targets: &[(Expr, String, Option<String>)],
        wrap: usize,
    ) -> Option<()> {
        if targets.is_empty() {
            return Some(());
        }
        self.keyword(" RETURNING", -STD, STD, 1);
        self.target_list(targets, wrap)
    }

    /// `get_returning_clause` with its WITH (OLD AS …, NEW AS …).
    fn returning_list_with(
        &mut self,
        targets: &[(Expr, String, Option<String>)],
        with: &str,
        wrap: usize,
    ) -> Option<()> {
        if targets.is_empty() {
            return Some(());
        }
        self.keyword(" RETURNING", -STD, STD, 1);
        self.buf.push_str(with);
        self.target_list(targets, wrap)
    }
}

/// Names for row positions past a level's own (RETURNING's old / new rows, MERGE_ACTION()).
fn extend_cols(level: &mut ViewLevel, extra: Vec<(usize, String)>) {
    for (i, name) in extra {
        if level.cols.len() <= i {
            level.cols.resize(i + 1, String::new());
            level.qual.resize(i + 1, String::new());
        }
        level.cols[i] = name.clone();
        level.qual[i] = name;
    }
}
