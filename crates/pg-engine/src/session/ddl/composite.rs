//! ALTER TYPE on composite types: ADD / DROP / ALTER ATTRIBUTE and RENAME ATTRIBUTE. A
//! composite's fields are embedded in every type that uses it (`DataType::Row`), so a change
//! is carried to the composites, tables, views, domains and functions using it, and stored
//! values of the type are rewritten to the new layout.

use super::Ddl;
use crate::catalog::CompositeType;
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::inner;
use crate::sql::binder::resolve_type_name;
use crate::types::{DataType, RowType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::collections::HashMap;
use std::sync::Arc;

/// For each field of a changed composite, the old field it comes from (`None`: added).
type Layout = Vec<Option<usize>>;

impl Ddl<'_, '_> {
    /// The composite type an ALTER TYPE names.
    fn composite(&self, rv: &pb::RangeVar) -> Result<Arc<CompositeType>> {
        let schemas: Vec<u32> = if rv.schemaname.is_empty() {
            self.cat.effective_path(self.search_path())
        } else {
            vec![self.cat.schema_oid(&rv.schemaname)?]
        };
        for s in schemas {
            if let Some(c) = self
                .cat
                .composites
                .values()
                .find(|c| c.schema == s && c.name == rv.relname)
            {
                return Ok(c.clone());
            }
        }
        match self.lookup(rv)? {
            Some(_) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a composite type", rv.relname),
            )),
            None => Err(crate::catalog::no_such_relation(
                Self::rv_schema(rv),
                &rv.relname,
            )),
        }
    }

    /// ALTER TYPE name ADD / DROP / ALTER ATTRIBUTE ...
    pub(super) fn alter_composite(&mut self, s: &pb::AlterTableStmt) -> Result<String> {
        use pb::AlterTableType as A;
        let rv = s.relation.as_ref().unwrap();
        let comp = self.composite(rv)?;
        self.require_owner(comp.owner, "type", &comp.name)?;
        let cmds: Vec<&pb::AlterTableCmd> = s
            .cmds
            .iter()
            .filter_map(|c| match inner(c) {
                Some(NodeEnum::AlterTableCmd(c)) => Some(&**c),
                _ => None,
            })
            .collect();
        let mut fields = comp.fields.clone();
        let mut dims = (*comp).clone();
        let mut layout: Layout = (0..fields.len()).map(Some).collect();
        let missing = |name: &str| {
            Error::new(
                code::UNDEFINED_COLUMN,
                format!(
                    "column \"{name}\" of relation \"{}\" does not exist",
                    comp.name
                ),
            )
        };
        // Like Postgres: drops, then type changes, then additions.
        for pass in [A::AtDropColumn, A::AtAlterColumnType, A::AtAddColumn] {
            for cmd in cmds
                .iter()
                .filter(|c| A::try_from(c.subtype).ok() == Some(pass))
            {
                match pass {
                    A::AtDropColumn => {
                        let Some(i) = fields.iter().position(|(n, _)| *n == cmd.name) else {
                            if cmd.missing_ok {
                                self.notice(format!(
                                    "column \"{}\" of relation \"{}\" does not exist, skipping",
                                    cmd.name, comp.name
                                ));
                                continue;
                            }
                            return Err(missing(&cmd.name));
                        };
                        // CHECK constraints reading the attribute depend on it.
                        let cascade = cmd.behavior == pb::DropBehavior::DropCascade as i32;
                        self.drop_attribute_dependents(&comp, layout[i].unwrap_or(i), cascade)?;
                        dims.dims.remove(&fields[i].0);
                        fields.remove(i);
                        layout.remove(i);
                    }
                    A::AtAlterColumnType => {
                        let Some(i) = fields.iter().position(|(n, _)| *n == cmd.name) else {
                            return Err(missing(&cmd.name));
                        };
                        let Some(NodeEnum::ColumnDef(cd)) = cmd.def.as_deref().and_then(inner)
                        else {
                            continue;
                        };
                        let tn = cd.type_name.as_ref().ok_or_else(|| {
                            crate::error::internal("ALTER ATTRIBUTE without a type")
                        })?;
                        if let Some((t, c)) = self.column_using(comp.oid) {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "cannot alter type \"{}\" because column \"{t}.{c}\" uses it",
                                    comp.name
                                ),
                            ));
                        }
                        let (ty, dom) = resolve_type_name(&self.cat, self.search_path(), tn)?;
                        self.require_type_usage(&ty, dom)?;
                        fields[i].1 = ty;
                        dims.set_dims(&fields[i].0, &fields[i].1, tn.array_bounds.len() as i16);
                    }
                    _ => {
                        let Some(NodeEnum::ColumnDef(cd)) = cmd.def.as_deref().and_then(inner)
                        else {
                            continue;
                        };
                        if fields.iter().any(|(n, _)| *n == cd.colname) {
                            return Err(Error::new(
                                code::DUPLICATE_COLUMN,
                                format!(
                                    "column \"{}\" of relation \"{}\" already exists",
                                    cd.colname, comp.name
                                ),
                            ));
                        }
                        let tn = cd.type_name.as_ref().ok_or_else(|| {
                            crate::error::internal("ADD ATTRIBUTE without a type")
                        })?;
                        let (ty, dom) = resolve_type_name(&self.cat, self.search_path(), tn)?;
                        self.require_type_usage(&ty, dom)?;
                        if ty.contains_row(comp.oid) {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "composite type {} cannot be made a member of itself",
                                    comp.name
                                ),
                            ));
                        }
                        dims.set_dims(&cd.colname, &ty, tn.array_bounds.len() as i16);
                        fields.push((cd.colname.clone(), ty));
                        layout.push(None);
                    }
                }
            }
        }
        // A domain over the type checks its constraints against the new attribute types
        // (ATPostAlterTypeParse remakes them).
        if cmds
            .iter()
            .any(|c| A::try_from(c.subtype).ok() == Some(A::AtAlterColumnType))
        {
            let new_base = CompositeType {
                fields: fields.clone(),
                ..(*comp).clone()
            }
            .data_type();
            let mut domains: Vec<Arc<crate::catalog::Domain>> = self
                .cat
                .domains
                .values()
                .filter(|d| d.base.row_oid() == Some(comp.oid))
                .cloned()
                .collect();
            domains.sort_by_key(|d| d.oid);
            for d in domains {
                for (_, sql) in &d.checks {
                    // The stored (cooked) expression: its constants keep the types they got.
                    let cooked = crate::sql::ruleutils::domain_check(self.ctx, &d.base, sql, false);
                    let node = crate::sql::binder::ast::parse_expr(cooked.as_deref().unwrap_or(sql))?;
                    self.binder()
                        .bind_domain_check(&node, &new_base)
                        .map_err(|e| Error { position: None, ..e })?;
                }
            }
        }
        for c in &cmds {
            let t = A::try_from(c.subtype).unwrap_or(A::Undefined);
            if !matches!(t, A::AtDropColumn | A::AtAlterColumnType | A::AtAddColumn) {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "ALTER action {} cannot be performed on relation \"{}\"",
                        alter_action_name(t),
                        comp.name
                    ),
                )
                .detail("This operation is not supported for composite types."));
            }
        }
        // Typed tables change with their type, given CASCADE.
        let typed = self.typed_tables(comp.oid);
        if !typed.is_empty() {
            if !cmds
                .iter()
                .any(|c| c.behavior == pb::DropBehavior::DropCascade as i32)
            {
                return Err(typed_table_error(&comp.name));
            }
            for t in typed {
                let mut nt = (*t).clone();
                self.typed_cascade = true;
                let r = (|| -> Result<()> {
                    for pass in [A::AtDropColumn, A::AtAlterColumnType, A::AtAddColumn] {
                        for cmd in cmds
                            .iter()
                            .filter(|c| A::try_from(c.subtype).ok() == Some(pass))
                        {
                            let before = nt.clone();
                            nt = self.alter_table_cmd(nt.clone(), cmd)?;
                            // (A new attribute reaches the typed table's inheritors too.)
                            if pass == A::AtAddColumn {
                                self.inh_recurse(&before, &nt, cmd, true)?;
                            }
                        }
                    }
                    Ok(())
                })();
                self.typed_cascade = false;
                r?;
                self.flush_type_rebuilds(Some(&mut nt))?;
                self.flush_stat_rebuilds();
                self.cat.put_table(nt);
            }
            // Then the typed tables' new TOAST tables and rewrites (ATRewriteTables).
            for oid in std::mem::take(&mut self.rewrites) {
                self.fire_table_rewrite(oid)?;
                self.ctx.txn.borrow_mut().mark_new_storage(oid);
                if let Some(c) = self.cat.tables.get(&oid).cloned() {
                    let mut c = (*c).clone();
                    self.cat.add_toast(&mut c);
                    self.cat.rewrite_heap(&mut c, false);
                    let xmin = self.recent_xmin();
                    self.set_frozen_xid(&c, xmin);
                    self.cat.put_table(c);
                }
            }
        }
        self.retype_composite(
            CompositeType {
                fields,
                dims: dims.dims,
                ..(*comp).clone()
            },
            layout,
        )?;
        self.save()?;
        Ok("ALTER TYPE".into())
    }

    /// The CHECK constraints that read attribute `attr` (its original position) of
    /// composite `comp`: an error naming them, or (CASCADE) dropped with a NOTICE.
    fn drop_attribute_dependents(
        &mut self,
        comp: &CompositeType,
        attr: usize,
        cascade: bool,
    ) -> Result<()> {
        let mut tables: Vec<Arc<crate::catalog::Table>> = self.cat.tables.values().cloned().collect();
        tables.sort_by_key(|t| t.oid);
        let mut deps: Vec<(u32, u32, String)> = Vec::new();
        for t in &tables {
            for c in &t.constraints {
                let crate::catalog::ConstraintKind::Check { expr, .. } = &c.kind else {
                    continue;
                };
                let Ok(e) = self.bind_check(t, expr) else {
                    continue;
                };
                let mut uses = false;
                e.walk(&mut |x| {
                    if let crate::sql::expr::Expr::Field(arg, k, _) = x
                        && *k == attr
                        && arg.ty().row_oid() == Some(comp.oid)
                    {
                        uses = true;
                    }
                });
                if uses {
                    deps.push((t.oid, c.oid, format!("constraint {} on table {}", c.name, t.name)));
                }
            }
        }
        // A domain over the type: its CHECK constraints reading the attribute.
        let mut domains: Vec<Arc<crate::catalog::Domain>> = self
            .cat
            .domains
            .values()
            .filter(|d| d.base.row_oid() == Some(comp.oid))
            .cloned()
            .collect();
        domains.sort_by_key(|d| d.oid);
        let mut domain_deps: Vec<(u32, String)> = Vec::new();
        for d in &domains {
            for (name, sql) in &d.checks {
                let Ok(node) = crate::sql::binder::ast::parse_expr(sql) else {
                    continue;
                };
                let Ok(e) = self.binder().bind_domain_check(&node, &d.base) else {
                    continue;
                };
                let mut uses = false;
                e.walk(&mut |x| {
                    if let crate::sql::expr::Expr::Field(arg, k, _) = x
                        && *k == attr
                        && arg.ty().row_oid() == Some(comp.oid)
                    {
                        uses = true;
                    }
                });
                if uses {
                    domain_deps.push((d.oid, name.clone()));
                    deps.push((0, d.oid, format!("constraint {name}")));
                }
            }
        }
        if deps.is_empty() {
            return Ok(());
        }
        let what = format!(
            "column {} of composite type {}",
            comp.fields[attr].0, comp.name
        );
        let lines: Vec<String> = deps.iter().map(|d| d.2.clone()).collect();
        if !cascade {
            return Err(Error::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {what} because other objects depend on it"),
            )
            .detail(
                lines
                    .iter()
                    .map(|l| format!("{l} depends on {what}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .hint("Use DROP ... CASCADE to drop the dependent objects too."));
        }
        if let [one] = lines.as_slice() {
            self.notice(format!("drop cascades to {one}"));
        } else {
            self.ctx.notice(
                crate::session::notice(format!("drop cascades to {} other objects", lines.len())).detail(
                    lines
                        .iter()
                        .map(|l| format!("drop cascades to {l}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            );
        }
        for (doid, name) in domain_deps {
            if let Some(d) = self.cat.domains.get(&doid).cloned() {
                let mut d = (*d).clone();
                d.checks.retain(|c| c.0 != name);
                self.cat.domains.insert(doid, Arc::new(d));
            }
        }
        for (toid, coid, _) in deps {
            if let Some(t) = self.cat.tables.get(&toid).cloned() {
                let mut t = (*t).clone();
                t.constraints.retain(|c| c.oid != coid);
                self.cat.put_table(t);
            }
        }
        Ok(())
    }

    /// Tables created OF composite type `oid`.
    fn typed_tables(&self, oid: u32) -> Vec<Arc<crate::catalog::Table>> {
        let mut v: Vec<_> = self
            .cat
            .tables
            .values()
            .filter(|t| t.of_type == oid)
            .cloned()
            .collect();
        v.sort_by_key(|t| t.oid);
        v
    }

    /// ALTER TYPE name RENAME ATTRIBUTE a TO b.
    pub(super) fn rename_attribute(
        &mut self,
        s: &pb::RenameStmt,
        newname: String,
    ) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let comp = self.composite(rv)?;
        self.require_owner(comp.owner, "type", &comp.name)?;
        let mut fields = comp.fields.clone();
        let Some(i) = fields.iter().position(|(n, _)| *n == s.subname) else {
            return Err(Error::new(
                code::UNDEFINED_COLUMN,
                format!("column \"{}\" does not exist", s.subname),
            ));
        };
        if fields.iter().any(|(n, _)| *n == newname) {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                format!(
                    "column \"{newname}\" of relation \"{}\" already exists",
                    comp.name
                ),
            ));
        }
        let typed = self.typed_tables(comp.oid);
        if !typed.is_empty() {
            if s.behavior != pb::DropBehavior::DropCascade as i32 {
                return Err(typed_table_error(&comp.name));
            }
            for t in typed {
                let rs = pb::RenameStmt {
                    rename_type: pb::ObjectType::ObjectColumn as i32,
                    relation_type: pb::ObjectType::ObjectTable as i32,
                    relation: Some(pb::RangeVar {
                        schemaname: self.cat.schema_name(t.schema).to_string(),
                        relname: t.name.clone(),
                        inh: true,
                        relpersistence: "p".into(),
                        location: -1,
                        ..Default::default()
                    }),
                    subname: s.subname.clone(),
                    newname: newname.clone(),
                    ..Default::default()
                };
                self.typed_cascade = true;
                let r = self.rename(&rs);
                self.typed_cascade = false;
                r?;
            }
        }
        fields[i].0 = newname;
        let layout = (0..fields.len()).map(Some).collect();
        self.retype_composite(
            CompositeType {
                fields,
                ..(*comp).clone()
            },
            layout,
        )?;
        self.save()?;
        Ok("ALTER TYPE".into())
    }

    /// A table column whose type holds composite `oid` (directly, in an array or nested in
    /// another composite): (table, column).
    pub(super) fn column_using(&self, oid: u32) -> Option<(String, String)> {
        let mut tables: Vec<_> = self.cat.tables.values().cloned().collect();
        tables.sort_by_key(|t| t.oid);
        let col = tables.iter().find_map(|t| {
            t.columns
                .iter()
                .find(|c| !c.dropped && c.ty.contains_row(oid))
                .map(|c| (t.oid, t.name.clone(), c.name.clone()))
        });
        // An index's expression column of the type counts too (named as
        // FigureIndexColname named it).
        let mut ix_col: Option<(u32, String, String)> = None;
        for t in &tables {
            let Ok(bixs) = self.binder().bound_indexes_all(t) else {
                continue;
            };
            for b in bixs {
                for (k, key) in b.keys.iter().enumerate() {
                    let Some(crate::catalog::IndexKey::Expr(e)) =
                        b.index.columns.get(k).map(|c| &c.key)
                    else {
                        continue;
                    };
                    if key.ty().contains_row(oid)
                        && ix_col.as_ref().is_none_or(|x| b.index.oid < x.0)
                    {
                        let name = crate::sql::binder::ast::parse_expr(e)
                            .ok()
                            .and_then(|n| crate::sql::binder::figure_index_colname(&n))
                            .unwrap_or_else(|| "expr".into());
                        ix_col = Some((b.index.oid, b.index.name.clone(), name));
                    }
                }
            }
        }
        match (col, ix_col) {
            (Some(c), Some(i)) => Some(if i.0 < c.0 { (i.1, i.2) } else { (c.1, c.2) }),
            (Some(c), None) => Some((c.1, c.2)),
            (None, Some(i)) => Some((i.1, i.2)),
            (None, None) => None,
        }
    }

    /// Install `comp`'s new fields: in the catalog's composites, in every type, column and
    /// signature embedding it, and in stored rows. `layout` maps its new fields to the old.
    fn retype_composite(&mut self, comp: CompositeType, layout: Layout) -> Result<()> {
        let mut types: HashMap<u32, Arc<RowType>> = HashMap::new();
        let mut layouts: HashMap<u32, Layout> = HashMap::new();
        let DataType::Row(rt) = comp.data_type() else {
            unreachable!()
        };
        types.insert(comp.oid, rt);
        layouts.insert(comp.oid, layout);
        self.cat.composites.insert(comp.oid, Arc::new(comp));
        self.retype_rows(types, layouts)
    }

    /// After ALTER TABLE changed `old`'s columns: the values of its row type stored
    /// elsewhere (a column of it, …) take the new layout, the added columns null.
    pub(super) fn retype_relation_rows(&mut self, old: &crate::catalog::Table) -> Result<()> {
        if old.rowtype == 0 || self.column_using(old.rowtype).is_none() {
            return Ok(());
        }
        let Some(DataType::Row(rt)) = self.cat.row_type_of(old.oid) else {
            return Ok(());
        };
        let before: Vec<(String, DataType)> = old
            .live_columns()
            .map(|(_, c)| (c.name.clone(), c.ty.clone()))
            .collect();
        if rt.fields == before {
            return Ok(());
        }
        let Some(new) = self.cat.tables.get(&old.oid).cloned() else {
            return Ok(());
        };
        // New field j: the old field of the same column (by attnum), if it had one.
        let old_pos: Vec<usize> = old.live_columns().map(|(i, _)| i).collect();
        let layout: Layout = new
            .live_columns()
            .map(|(i, _)| old_pos.iter().position(|&p| p == i))
            .collect();
        let mut types: HashMap<u32, Arc<RowType>> = HashMap::new();
        let mut layouts: HashMap<u32, Layout> = HashMap::new();
        types.insert(old.rowtype, rt);
        layouts.insert(old.rowtype, layout);
        self.retype_rows(types, layouts)
    }

    /// Install the changed row types `types` in every type, column and signature embedding
    /// them, and in stored rows (`layouts` map their new fields to the old).
    fn retype_rows(
        &mut self,
        mut types: HashMap<u32, Arc<RowType>>,
        layouts: HashMap<u32, Layout>,
    ) -> Result<()> {
        // Composites containing a changed one change too, transitively.
        loop {
            let mut more = false;
            let comps: Vec<Arc<CompositeType>> = self
                .cat
                .composites
                .values()
                .filter(|c| !types.contains_key(&c.oid))
                .cloned()
                .collect();
            for c in comps {
                let fields: Vec<(String, DataType)> = c
                    .fields
                    .iter()
                    .map(|(n, t)| (n.clone(), subst(t, &types).unwrap_or_else(|| t.clone())))
                    .collect();
                if fields != c.fields {
                    let nc = CompositeType {
                        fields,
                        ..(*c).clone()
                    };
                    let DataType::Row(rt) = nc.data_type() else {
                        unreachable!()
                    };
                    types.insert(nc.oid, rt);
                    self.cat.composites.insert(nc.oid, Arc::new(nc));
                    more = true;
                }
            }
            if !more {
                break;
            }
        }
        // Tables, with their stored values.
        let tables: Vec<_> = self.cat.tables.values().cloned().collect();
        for t in tables {
            let mut nt = (*t).clone();
            let mut changed = false;
            for c in &mut nt.columns {
                if let Some(ty) = subst(&c.ty, &types) {
                    c.ty = ty;
                    changed = true;
                }
            }
            if !changed {
                continue;
            }
            if self.has_rows(&t)? {
                let (old_cols, new_cols): (Vec<DataType>, Vec<DataType>) = (
                    t.columns.iter().map(|c| c.ty.clone()).collect(),
                    nt.columns.iter().map(|c| c.ty.clone()).collect(),
                );
                let layouts = &layouts;
                let start = super::index_prefix(t.oid, 0);
                let end = super::prefix_end(&start);
                let entries: Vec<(Vec<u8>, bytes::Bytes)> = self
                    .ctx
                    .txn
                    .borrow()
                    .scan(&start, &end)
                    .collect::<Result<_>>()?;
                for (k, v) in entries {
                    self.ctx.check()?;
                    let row = super::decode_table_row(&t, &v)?;
                    let row: Vec<Value> = row
                        .into_iter()
                        .enumerate()
                        .map(|(i, v)| convert(v, &old_cols[i], &new_cols[i], layouts))
                        .collect();
                    let xid = self.ctx.txn.borrow_mut().cur_xid();
                    self.ctx
                        .txn
                        .borrow_mut()
                        .put(k, crate::exec::scan::encode_table_row(&row, xid))?;
                }
            }
            self.cat.put_table(nt);
        }
        // Views, domains and function signatures.
        for v in self.cat.views.values_mut() {
            if v.columns.iter().any(|(_, t)| subst(t, &types).is_some()) {
                let mut nv = (**v).clone();
                for (_, t) in &mut nv.columns {
                    if let Some(ty) = subst(t, &types) {
                        *t = ty;
                    }
                }
                *v = Arc::new(nv);
            }
        }
        for d in self.cat.domains.values_mut() {
            if let Some(ty) = subst(&d.base, &types) {
                let mut nd = (**d).clone();
                nd.base = ty;
                *d = Arc::new(nd);
            }
        }
        for f in self.cat.functions.values_mut() {
            let touches = subst(&f.ret, &types).is_some()
                || f.args.iter().any(|(_, t)| subst(t, &types).is_some())
                || f.out_cols.iter().any(|(_, t)| subst(t, &types).is_some());
            if touches {
                let mut nf = (**f).clone();
                if let Some(ty) = subst(&nf.ret, &types) {
                    nf.ret = ty;
                }
                for (_, t) in nf.args.iter_mut() {
                    if let Some(ty) = subst(t, &types) {
                        *t = ty;
                    }
                }
                for (_, t) in nf.out_cols.iter_mut() {
                    if let Some(ty) = subst(t, &types) {
                        *t = ty;
                    }
                }
                *f = Arc::new(nf);
            }
        }
        Ok(())
    }
}

fn typed_table_error(name: &str) -> Error {
    Error::new(
        code::DEPENDENT_OBJECTS_STILL_EXIST,
        format!("cannot alter type \"{name}\" because it is the type of a typed table"),
    )
    .hint("Use ALTER ... CASCADE to alter the typed tables too.")
}

/// `ty` with the changed composites in `types` swapped in; `None` when it holds none of them.
fn subst(ty: &DataType, types: &HashMap<u32, Arc<RowType>>) -> Option<DataType> {
    match ty {
        DataType::Row(r) if r.oid != 0 && types.contains_key(&r.oid) => {
            Some(DataType::Row(types[&r.oid].clone()))
        }
        DataType::Row(r) => {
            let fields: Vec<Option<DataType>> =
                r.fields.iter().map(|(_, t)| subst(t, types)).collect();
            if fields.iter().all(Option::is_none) {
                return None;
            }
            let fields = r
                .fields
                .iter()
                .zip(fields)
                .map(|((n, t), s)| (n.clone(), s.unwrap_or_else(|| t.clone())))
                .collect();
            Some(DataType::Row(Arc::new(RowType {
                fields,
                ..(**r).clone()
            })))
        }
        DataType::Array(e) => subst(e, types).map(|e| DataType::Array(Box::new(e))),
        _ => None,
    }
}

/// A stored value of type `old` in the layout of `new`.
fn convert(v: Value, old: &DataType, new: &DataType, layouts: &HashMap<u32, Layout>) -> Value {
    match (v, old, new) {
        (Value::Record(items), DataType::Row(o), DataType::Row(n)) => {
            let layout = layouts.get(&n.oid).filter(|_| n.oid != 0);
            let out: Vec<Value> = n
                .fields
                .iter()
                .enumerate()
                .map(|(j, (_, nt))| {
                    let src = match layout {
                        Some(l) => l[j],
                        None => Some(j),
                    };
                    match src.and_then(|i| items.get(i).cloned().map(|v| (v, &o.fields[i].1))) {
                        Some((v, ot)) => convert(v, ot, nt, layouts),
                        None => Value::Null,
                    }
                })
                .collect();
            Value::Record(Arc::new(out))
        }
        (Value::Array(a), DataType::Array(oe), DataType::Array(ne)) => {
            let mut a = (*a).clone();
            a.items = a
                .items
                .into_iter()
                .map(|v| convert(v, oe, ne, layouts))
                .collect();
            a.elem = (**ne).clone();
            Value::Array(Arc::new(a))
        }
        (v, _, _) => v,
    }
}

/// An ALTER TABLE subcommand's name, for errors.
fn alter_action_name(t: pb::AlterTableType) -> &'static str {
    use pb::AlterTableType as A;
    match t {
        A::AtColumnDefault => "ALTER COLUMN ... SET DEFAULT",
        A::AtSetNotNull => "ALTER COLUMN ... SET NOT NULL",
        A::AtDropNotNull => "ALTER COLUMN ... DROP NOT NULL",
        A::AtAddConstraint => "ADD CONSTRAINT",
        A::AtDropConstraint => "DROP CONSTRAINT",
        A::AtChangeOwner => "OWNER TO",
        _ => "this",
    }
}
