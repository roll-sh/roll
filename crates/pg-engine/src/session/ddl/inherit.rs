//! Table inheritance: CREATE TABLE ... INHERITS (merging the parents' columns and constraints
//! with the local definitions, as Postgres's MergeAttributes does).

use super::Ddl;
use super::table::Pending;
use crate::catalog::{Column, Constraint, ConstraintKind, Relation, Sequence, Table};
use crate::error::{Error, Result, code};
use pg_query::protobuf as pb;
use std::sync::Arc;

/// Columns gathered from the parents, before the local definitions are merged in.
pub(super) struct Inherited {
    /// Number of inherited columns (they come first).
    pub ncols: usize,
    /// Per inherited column: the parents' defaults disagree.
    conflict_default: Vec<bool>,
    conflict_generated: Vec<bool>,
}

impl Ddl<'_, '_> {
    /// The INHERITS list, checked: tables only, each once, owned by the current user.
    pub(super) fn inherit_parents(&self, list: &[pb::Node], temp: bool) -> Result<Vec<Arc<Table>>> {
        let mut out: Vec<Arc<Table>> = Vec::new();
        for n in list {
            let Some(pg_query::NodeEnum::RangeVar(rv)) = crate::sql::binder::ast::inner(n) else {
                continue;
            };
            let t = match self.lookup(rv)? {
                Some(Relation::Table(t)) if !self.cat.views.contains_key(&t.oid) => t,
                Some(_) => {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "inherited relation \"{}\" is not a table or foreign table",
                            rv.relname
                        ),
                    ));
                }
                None => {
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                }
            };
            if t.partkey.is_some() {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot inherit from partitioned table \"{}\"", t.name),
                ));
            }
            if t.partbound.is_some() {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot inherit from partition \"{}\"", t.name),
                ));
            }
            if t.temporary && !temp {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot inherit from temporary relation \"{}\"", t.name),
                ));
            }
            self.require_owner(t.owner, "table", &t.name)?;
            if out.iter().any(|p| p.oid == t.oid) {
                return Err(Error::new(
                    code::DUPLICATE_TABLE,
                    format!(
                        "relation \"{}\" would be inherited from more than once",
                        t.name
                    ),
                ));
            }
            out.push(t);
        }
        Ok(out)
    }

    /// Start a child table with its parents' columns and inheritable constraints.
    pub(super) fn inherit_from(
        &mut self,
        t: &mut Table,
        parents: &[Arc<Table>],
    ) -> Result<Inherited> {
        let mut conflict_default = Vec::new();
        let mut conflict_generated = Vec::new();
        for p in parents {
            for (pi, pc) in p.live_columns() {
                // A NO INHERIT not-null stays with the parent.
                let inh_not_null = pc.not_null
                    && !p.constraints.iter().any(|k| {
                        k.no_inherit
                            && matches!(k.kind, ConstraintKind::NotNull { column } if column == pi)
                    });
                match t.column_index(&pc.name) {
                    Some(i) => {
                        self.notice(format!(
                            "merging multiple inherited definitions of column \"{}\"",
                            pc.name
                        ));
                        let c = &mut t.columns[i];
                        if c.ty != pc.ty || c.domain != pc.domain {
                            return Err(Error::new(
                                code::DATATYPE_MISMATCH,
                                format!("inherited column \"{}\" has a type conflict", pc.name),
                            )
                            .detail(format!(
                                "{} versus {}",
                                self.column_type(c),
                                self.column_type(pc)
                            )));
                        }
                        if collation_of(c) != collation_of(pc) {
                            return Err(Error::new(
                                code::COLLATION_MISMATCH,
                                format!(
                                    "inherited column \"{}\" has a collation conflict",
                                    pc.name
                                ),
                            )
                            .detail(format!(
                                "\"{}\" versus \"{}\"",
                                collation_of(c),
                                collation_of(pc)
                            )));
                        }
                        let storage = |c: &Column| {
                            c.storage
                                .unwrap_or_else(|| crate::catalog::system::typstorage(&c.ty))
                        };
                        if storage(c) != storage(pc) {
                            return Err(Error::new(
                                code::DATATYPE_MISMATCH,
                                format!(
                                    "inherited column \"{}\" has a storage parameter conflict",
                                    pc.name
                                ),
                            )
                            .detail(format!(
                                "{} versus {}",
                                super::table::storage_name(storage(c)),
                                super::table::storage_name(storage(pc))
                            )));
                        }
                        // (A parent with no compression method set takes the other's.)
                        match (c.compression, pc.compression) {
                            (None, m) => c.compression = m,
                            (Some(a), Some(b)) if a != b => {
                                return Err(Error::new(
                                    code::DATATYPE_MISMATCH,
                                    format!(
                                        "column \"{}\" has a compression method conflict",
                                        pc.name
                                    ),
                                )
                                .detail(format!(
                                    "{} versus {}",
                                    super::table::compression_name(c.compression),
                                    super::table::compression_name(pc.compression)
                                )));
                            }
                            _ => {}
                        }
                        c.not_null |= inh_not_null;
                        if c.default != pc.default {
                            conflict_default[i] = true;
                        }
                        // One parent's column generated and another's not: an error now
                        // (two expressions are resolved by the child's own).
                        if c.generated.is_some() != pc.generated.is_some() {
                            return Err(Error::new(
                                code::DATATYPE_MISMATCH,
                                format!("inherited column \"{}\" has a generation conflict", pc.name),
                            ));
                        }
                        if c.generated != pc.generated {
                            conflict_generated[i] = true;
                        }
                        c.inhcount += 1;
                    }
                    None => {
                        let mut c = Column::new(pc.name.clone(), pc.ty.clone());
                        c.not_null = inh_not_null;
                        c.default = pc.default.clone();
                        c.generated = pc.generated.clone();
                        c.virtual_gen = pc.virtual_gen;
                        c.collation = pc.collation.clone();
                        c.domain = pc.domain;
                        c.storage = pc.storage;
                        c.compression = pc.compression;
                        c.inhcount = 1;
                        c.islocal = false;
                        t.columns.push(c);
                        conflict_default.push(false);
                        conflict_generated.push(false);
                    }
                }
            }
        }
        // Inherited defaults are stored with the table, before inherited checks (cooked
        // constraints, in StoreConstraints); local ones come after.
        for (i, c) in t.columns.iter_mut().enumerate() {
            if (c.default.is_some() || c.generated.is_some())
                && !conflict_default[i]
                && !conflict_generated[i]
            {
                c.def_oid = self.cat.alloc_oid();
            }
        }
        for p in parents {
            // A parent's constraints in name order (as the relcache and the
            // pg_constraint_conrelid_contypid_conname_index scan give them).
            let mut cons: Vec<&Constraint> = p.constraints.iter().collect();
            cons.sort_by(|a, b| a.name.cmp(&b.name));
            for pc in cons {
                match &pc.kind {
                    ConstraintKind::Check {
                        expr,
                        no_inherit: false,
                    } => {
                        if let Some(c) = t.constraints.iter_mut().find(|c| c.name == pc.name) {
                            match &c.kind {
                                ConstraintKind::Check { expr: e, .. } if e == expr => {
                                    // (MergeCheckConstraint: enforced if any parent's is.)
                                    if pc.enforced && !c.enforced {
                                        c.enforced = true;
                                        c.validated = true;
                                    }
                                    c.inhcount += 1
                                }
                                _ => {
                                    return Err(Error::new(
                                        code::DUPLICATE_OBJECT,
                                        format!(
                                            "check constraint name \"{}\" appears multiple times but with different expressions",
                                            pc.name
                                        ),
                                    ));
                                }
                            }
                            continue;
                        }
                        // (A new table has no rows: an enforced copy is valid.)
                        let mut c = Constraint::new(
                            self.cat.alloc_oid(),
                            pc.name.clone(),
                            pc.kind.clone(),
                            pc.enforced,
                        );
                        c.enforced = pc.enforced;
                        c.inhcount = 1;
                        c.islocal = false;
                        t.constraints.push(c);
                    }
                    ConstraintKind::NotNull { column } if !pc.no_inherit => {
                        let Some(i) = t.column_index(&p.columns[*column].name) else {
                            continue;
                        };
                        if let Some(c) = t.constraints.iter_mut().find(
                            |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i),
                        ) {
                            c.inhcount += 1;
                            continue;
                        }
                        // Its oid comes after the table's own not-nulls' (create_table gives
                        // it, and a name of its own if another took this one by then).
                        let cname = pc.name.clone();
                        // A new child's copy is valid even under a NOT VALID one (its
                        // rows are new).
                        let mut c = Constraint::new(
                            0,
                            cname,
                            ConstraintKind::NotNull { column: i },
                            true,
                        );
                        c.inhcount = 1;
                        c.islocal = false;
                        t.constraints.push(c);
                    }
                    _ => {}
                }
            }
        }
        t.inherits = parents.iter().map(|p| p.oid).collect();
        Ok(Inherited {
            ncols: t.columns.len(),
            conflict_default,
            conflict_generated,
        })
    }

    /// A local column definition of a child table: merged into the inherited column of the
    /// same name, or added after them. `pos` is its 1-based position among the local columns.
    pub(super) fn child_column_def(
        &mut self,
        t: &mut Table,
        inh: &mut Inherited,
        cd: &pb::ColumnDef,
        pos: usize,
        pending: &mut Vec<Pending>,
        seqs: &mut Vec<Sequence>,
    ) -> Result<()> {
        let name = crate::catalog::truncate_ident(&cd.colname);
        let Some(e) = t
            .column_index(&name)
            .filter(|e| *e < inh.ncols && !t.columns[*e].islocal)
        else {
            return self.column_def(t, cd, pending, seqs);
        };
        if e + 1 == pos {
            self.notice(format!(
                "merging column \"{name}\" with inherited definition"
            ));
        } else {
            self.ctx.notice(
                crate::session::notice(format!(
                    "moving and merging column \"{name}\" with inherited definition"
                ))
                .detail("User-specified column moved to the position of the inherited column."),
            );
        }
        // Build the local definition on its own, then fold it into the inherited column.
        let mut scratch = t.clone();
        scratch.columns[e].dropped = true;
        let n = scratch.columns.len();
        let (np, ns) = (pending.len(), seqs.len());
        self.column_def(&mut scratch, cd, pending, seqs)?;
        let local = scratch.columns.pop().expect("new column");
        for p in &mut pending[np..] {
            p.remap_column(n, e);
        }
        for s in &mut seqs[ns..] {
            if let Some((_, c)) = &mut s.owned_by
                && *c == n
            {
                *c = e;
            }
        }
        let c = &t.columns[e];
        if local.ty != c.ty || local.domain != c.domain {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!("column \"{name}\" has a type conflict"),
            )
            .detail(format!(
                "{} versus {}",
                self.column_type(c),
                self.column_type(&local)
            )));
        }
        if local.collation.is_some() && collation_of(&local) != collation_of(c) {
            return Err(Error::new(
                code::COLLATION_MISMATCH,
                format!("column \"{name}\" has a collation conflict"),
            )
            .detail(format!(
                "\"{}\" versus \"{}\"",
                collation_of(c),
                collation_of(&local)
            )));
        }
        // MergeChildAttribute: a compression method given must agree. (A STORAGE clause is
        // only resolved later, so the inherited storage stays, unchecked.)
        if let (Some(a), Some(b)) = (c.compression, local.compression)
            && a != b
        {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!("column \"{name}\" has a compression method conflict"),
            )
            .detail(format!(
                "{} versus {}",
                super::table::compression_name(c.compression),
                super::table::compression_name(local.compression)
            )));
        }
        if c.generated.is_some() {
            if local.generated.is_some() && local.virtual_gen != c.virtual_gen {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!("column \"{name}\" inherits from generated column of different kind"),
                )
                .detail(kind_detail(c, &local)));
            }
            if local.default.is_some() && local.seq.is_none() {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column \"{name}\" inherits from generated column but specifies default"
                    ),
                ));
            }
            if local.identity.is_some() {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column \"{name}\" inherits from generated column but specifies identity"
                    ),
                ));
            }
        } else if local.generated.is_some() {
            return Err(Error::new(
                code::INVALID_COLUMN_DEFINITION,
                format!("child column \"{name}\" specifies generation expression"),
            )
            .hint("A child table column cannot be generated unless its parent column is."));
        }
        let c = &mut t.columns[e];
        c.not_null |= local.not_null;
        if local.default.is_some() {
            c.default = local.default;
            c.def_oid = 0;
            inh.conflict_default[e] = false;
        }
        if local.generated.is_some() {
            c.generated = local.generated;
            c.def_oid = 0;
            c.virtual_gen = local.virtual_gen;
            inh.conflict_generated[e] = false;
        }
        if local.identity.is_some() {
            c.identity = local.identity;
        }
        if local.seq.is_some() {
            c.seq = local.seq;
        }
        if local.collation.is_some() {
            c.collation = local.collation;
        }
        if c.compression.is_none() {
            c.compression = local.compression;
        }
        c.islocal = true;
        Ok(())
    }

    /// After the local columns: inherited columns whose parents disagree must be resolved.
    pub(super) fn check_inherited(&self, t: &Table, inh: &Inherited) -> Result<()> {
        for i in 0..inh.ncols {
            if inh.conflict_generated[i] {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column \"{}\" inherits conflicting generation expressions",
                        t.columns[i].name
                    ),
                )
                .hint("To resolve the conflict, specify a generation expression explicitly."));
            }
            if inh.conflict_default[i] {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "column \"{}\" inherits conflicting default values",
                        t.columns[i].name
                    ),
                )
                .hint("To resolve the conflict, specify a default explicitly."));
            }
        }
        Ok(())
    }

    /// A column's declared type, as error details show it.
    fn column_type(&self, c: &Column) -> String {
        match c.domain.and_then(|d| self.cat.domains.get(&d)) {
            Some(d) => d.name.clone(),
            None => c.ty.display(),
        }
    }
}

fn collation_of(c: &Column) -> &str {
    c.collation.as_deref().unwrap_or("default")
}

impl Pending {
    /// Point a pending constraint at column `to` instead of `from`.
    pub(super) fn remap_column(&mut self, from: usize, to: usize) {
        let fix = |c: &mut usize| {
            if *c == from {
                *c = to;
            }
        };
        match self {
            Pending::NotNull { col, .. } => fix(col),
            Pending::Check { col, .. } => {
                if let Some(c) = col {
                    fix(c);
                }
            }
            Pending::Index { cols, include, .. } => {
                cols.iter_mut().chain(include.iter_mut()).for_each(fix)
            }
            Pending::Fk(f) => f.cols.iter_mut().for_each(fix),
            Pending::Exclusion(_) | Pending::Like { .. } => {}
        }
    }
}

/// "Parent column is VIRTUAL, child column is STORED."
pub(super) fn kind_detail(parent: &Column, child: &Column) -> String {
    let kind = |c: &Column| if c.virtual_gen { "VIRTUAL" } else { "STORED" };
    format!(
        "Parent column is {}, child column is {}.",
        kind(parent),
        kind(child)
    )
}

fn inherited(msg: String) -> Error {
    Error::new(code::INVALID_TABLE_DEFINITION, msg)
}

impl Ddl<'_, '_> {
    pub(super) fn child_tables(&self, oid: u32) -> Vec<Table> {
        self.cat
            .children(oid)
            .iter()
            .filter_map(|c| self.cat.table(*c))
            .map(|t| (**t).clone())
            .collect()
    }

    /// Checks before an ALTER TABLE subcommand runs on the named table: inherited columns and
    /// constraints belong to the parent, and ONLY can't leave the children inconsistent.
    /// Whether a partitioned table's key uses a column (directly or in an expression).
    pub(super) fn partkey_uses(&self, t: &Table, col: usize) -> bool {
        let Some(key) = &t.partkey else { return false };
        key.cols.iter().any(|k| match (k.col, &k.expr) {
            (Some(i), _) => i == col,
            (_, Some(e)) => {
                let ta = Arc::new(t.clone());
                crate::sql::binder::ast::parse_expr(e)
                    .and_then(|n| {
                        self.binder().bind_over_table(
                            &ta,
                            &n,
                            crate::sql::binder::Clause::PartitionKey,
                        )
                    })
                    .is_ok_and(|x| x.columns().contains(&col))
            }
            _ => false,
        })
    }

    /// The relation whose partition key uses column `col` of `t` — `t`, or (recursing) a
    /// partitioned descendant's of the same name.
    fn partkey_user(&self, t: &Table, col: &str, recurse: bool) -> Option<String> {
        if let Some(i) = t.column_index(col)
            && self.partkey_uses(t, i)
        {
            return Some(t.name.clone());
        }
        if !recurse {
            return None;
        }
        self.cat.inheritors(t.oid).into_iter().skip(1).find_map(|o| {
            let c = self.cat.tables.get(&o)?;
            let i = c.column_index(col)?;
            (c.partkey.is_some() && self.partkey_uses(c, i)).then(|| c.name.clone())
        })
    }

    pub(super) fn inh_precheck(
        &self,
        t: &Table,
        cmd: &pb::AlterTableCmd,
        recurse: bool,
    ) -> Result<()> {
        use pb::AlterTableType as A;
        let has_children = !self.cat.children(t.oid).is_empty();
        let col = t.column_index(&cmd.name).map(|i| &t.columns[i]);
        match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
            A::AtAddColumn => {
                let Some(pg_query::NodeEnum::ColumnDef(cd)) =
                    cmd.def.as_deref().and_then(crate::sql::binder::ast::inner)
                else {
                    return Ok(());
                };
                if t.partbound.is_some() {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "cannot add column to a partition",
                    ));
                }
                if has_children && t.column_index(&cd.colname).is_none() {
                    if !recurse {
                        return Err(inherited("column must be added to child tables too".into()));
                    }
                    let identity = cd.constraints.iter().any(|c| matches!(crate::sql::binder::ast::inner(c), Some(pg_query::NodeEnum::Constraint(k)) if k.contype == pb::ConstrType::ConstrIdentity as i32));
                    // (A partitioned table's partitions share its identity column.)
                    if identity && t.partkey.is_none() {
                        return Err(inherited(
                            "cannot recursively add identity column to table that has child tables"
                                .into(),
                        ));
                    }
                }
            }
            A::AtAddIdentity | A::AtSetIdentity | A::AtDropIdentity
                if t.partkey.is_some() && !recurse =>
            {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    match A::try_from(cmd.subtype) {
                        Ok(A::AtAddIdentity) => {
                            "cannot add identity to a column of only the partitioned table"
                        }
                        Ok(A::AtDropIdentity) => {
                            "cannot drop identity from a column of only the partitioned table"
                        }
                        _ => "cannot change identity column of only the partitioned table",
                    },
                )
                .hint("Do not specify the ONLY keyword."));
            }
            A::AtDropExpression => {
                if !recurse && has_children {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "ALTER TABLE / DROP EXPRESSION must be applied to child tables too",
                    ));
                }
                if col.is_some_and(|c| c.inhcount > 0) {
                    return Err(inherited(
                        "cannot drop generation expression from inherited column".into(),
                    ));
                }
            }
            A::AtDropColumn => {
                if !recurse && has_children && t.partkey.is_some() && col.is_some() {
                    return Err(Error::new(
                        code::INVALID_TABLE_DEFINITION,
                        "cannot drop column from only the partitioned table when partitions exist",
                    )
                    .hint("Do not specify the ONLY keyword."));
                }
                if col.is_some_and(|c| c.inhcount > 0) {
                    return Err(inherited(format!(
                        "cannot drop inherited column \"{}\"",
                        cmd.name
                    )));
                }
                if let Some(name) = self.partkey_user(t, &cmd.name, recurse) {
                    return Err(inherited(format!(
                        "cannot drop column \"{}\" because it is part of the partition key of relation \"{name}\"",
                        cmd.name
                    )));
                }
            }
            A::AtAlterColumnType => {
                let loc = match cmd.def.as_deref().and_then(crate::sql::binder::ast::inner) {
                    Some(pg_query::NodeEnum::ColumnDef(cd)) => cd.location,
                    _ => -1,
                };
                if col.is_some_and(|c| c.inhcount > 0) {
                    return Err(crate::sql::binder::ast::with_pos(
                        inherited(format!("cannot alter inherited column \"{}\"", cmd.name)),
                        loc,
                    ));
                }
                if let Some(name) = self.partkey_user(t, &cmd.name, recurse) {
                    return Err(crate::sql::binder::ast::with_pos(
                        inherited(format!(
                            "cannot alter column \"{}\" because it is part of the partition key of relation \"{name}\"",
                            cmd.name
                        )),
                        loc,
                    ));
                }
                if col.is_some() && has_children && !recurse {
                    return Err(inherited(format!(
                        "type of inherited column \"{}\" must be changed in child tables too",
                        cmd.name
                    )));
                }
                // A child may inherit the column from outside this hierarchy too.
                if col.is_some() && has_children && recurse {
                    let all = self.cat.inheritors(t.oid);
                    for c in all.iter().skip(1) {
                        let Some(ct) = self.cat.tables.get(c) else { continue };
                        let numparents = ct.inherits.iter().filter(|p| all.contains(p)).count();
                        if let Some(j) = ct.column_index(&cmd.name)
                            && ct.columns[j].inhcount as usize > numparents
                        {
                            return Err(inherited(format!(
                                "cannot alter inherited column \"{}\" of relation \"{}\"",
                                cmd.name, ct.name
                            )));
                        }
                    }
                }
            }
            A::AtDropNotNull => {
                // ATExecDropNotNull refuses an identity column first.
                if col.is_some_and(|c| c.identity.is_some()) {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "column \"{}\" of relation \"{}\" is an identity column",
                            cmd.name, t.name
                        ),
                    ));
                }
                // A partition's column stays NOT NULL while its parent's is.
                if let Some(c) = col
                    && c.not_null
                    && c.identity.is_none()
                    && t.partbound.is_some()
                    && let Some(p) = t.inherits.first().and_then(|p| self.cat.tables.get(p))
                    && p.column_index(&cmd.name)
                        .is_some_and(|j| p.columns[j].not_null)
                {
                    return Err(inherited(format!(
                        "column \"{}\" is marked NOT NULL in parent table",
                        cmd.name
                    )));
                }
                if let Some(i) = t.column_index(&cmd.name)
                    && let Some(c) = t.constraints.iter().find(
                        |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i),
                    )
                    && c.inhcount > 0
                {
                    return Err(inherited(format!(
                        "cannot drop inherited constraint \"{}\" of relation \"{}\"",
                        c.name, t.name
                    )));
                }
            }
            // ATExecSetNotNull: a partitioned table's new not-null needs its partitions'.
            A::AtSetNotNull
                if !recurse
                    && has_children
                    && t.partkey.is_some()
                    && t.column_index(&cmd.name).is_some_and(|i| {
                        !t.constraints.iter().any(
                            |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i),
                        )
                    }) =>
            {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    "constraint must be added to child tables too",
                )
                .hint("Do not specify the ONLY keyword."));
            }
            A::AtAddConstraint => {
                let Some(pg_query::NodeEnum::Constraint(c)) =
                    cmd.def.as_deref().and_then(crate::sql::binder::ast::inner)
                else {
                    return Ok(());
                };
                if c.contype == pb::ConstrType::ConstrCheck as i32
                    && !c.is_no_inherit
                    && has_children
                    && !recurse
                {
                    return Err(inherited(
                        "constraint must be added to child tables too".into(),
                    ));
                }
            }
            A::AtDropConstraint => {
                if let Some(c) = t.constraints.iter().find(|c| c.name == cmd.name)
                    && c.inhcount > 0
                {
                    return Err(inherited(format!(
                        "cannot drop inherited constraint \"{}\" of relation \"{}\"",
                        c.name, t.name
                    )));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// After a subcommand ran on the named table (`before` → `t`): carry it to the children.
    pub(super) fn inh_recurse(
        &mut self,
        before: &Table,
        t: &Table,
        cmd: &pb::AlterTableCmd,
        recurse: bool,
    ) -> Result<()> {
        use pb::AlterTableType as A;
        if self.cat.children(t.oid).is_empty() {
            return Ok(());
        }
        let cascade = cmd.behavior == pb::DropBehavior::DropCascade as i32;
        match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
            A::AtAddColumn if recurse && t.columns.len() > before.columns.len() => {
                self.add_column_children(before, t, cmd)?
            }
            A::AtAlterColumnType
            | A::AtColumnDefault
            | A::AtSetStatistics
            | A::AtSetOptions
            | A::AtResetOptions
            | A::AtSetStorage
            | A::AtSetCompression
            | A::AtSetExpression
            | A::AtDropExpression
                if recurse =>
            {
                for oid in self.cat.inheritors(t.oid).into_iter().skip(1) {
                    let Some(child) = self.cat.table(oid).map(|c| (**c).clone()) else {
                        continue;
                    };
                    if child.column_index(&cmd.name).is_none() {
                        continue;
                    }
                    let child = self.alter_cmd(child, cmd)?;
                    self.cat.put_table(child);
                }
            }
            A::AtSetNotNull if recurse => {
                let Some(i) = t.column_index(&cmd.name) else {
                    return Ok(());
                };
                let name = t
                    .constraints
                    .iter()
                    .find(|c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i))
                    .map(|c| c.name.clone());
                let had = before.constraints.iter().any(
                    |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i),
                );
                if had {
                    // The children have it already: SET NOT NULL validates their copies.
                    self.validate_not_null_children(t.oid, &cmd.name)?;
                } else if let Some(name) = name {
                    self.not_null_children(t.oid, &cmd.name, &name)?;
                }
            }
            A::AtDropNotNull => {
                let Some(i) = before.column_index(&cmd.name) else {
                    return Ok(());
                };
                if let Some(c) = before
                    .constraints
                    .iter()
                    .find(|c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i))
                    && !c.no_inherit
                    && !t.constraints.iter().any(|x| x.oid == c.oid)
                {
                    self.drop_constraint_children(t.oid, &c.name, recurse, cascade)?;
                }
            }
            A::AtAddConstraint if recurse => {
                let added: Vec<Constraint> = t
                    .constraints
                    .iter()
                    .filter(|c| !before.constraints.iter().any(|b| b.oid == c.oid))
                    .cloned()
                    .collect();
                for c in added {
                    if matches!(
                        c.kind,
                        ConstraintKind::Check {
                            no_inherit: false,
                            ..
                        }
                    ) {
                        self.check_children(t.oid, &c)?;
                    }
                    // A not-null one reaches the children too.
                    if let ConstraintKind::NotNull { column } = c.kind
                        && !c.no_inherit
                    {
                        let col = t.columns[column].name.clone();
                        self.not_null_children_valid(t.oid, &col, &c.name, c.validated, true)?;
                    }
                }
            }
            A::AtDropConstraint => {
                // (A NO INHERIT one has no copies to drop.)
                if let Some(c) = before.constraints.iter().find(|c| c.name == cmd.name)
                    && matches!(
                        c.kind,
                        ConstraintKind::Check {
                            no_inherit: false,
                            ..
                        } | ConstraintKind::NotNull { .. }
                    )
                    && !c.no_inherit
                {
                    self.drop_constraint_children(t.oid, &cmd.name, recurse, cascade)?;
                }
            }
            // QueueCheckConstraintValidation: the children's copies are validated too.
            A::AtValidateConstraint if recurse => {
                let was_valid = before
                    .constraints
                    .iter()
                    .find(|c| c.name == cmd.name)
                    .is_none_or(|c| {
                        c.validated
                            || !matches!(
                                c.kind,
                                ConstraintKind::Check {
                                    no_inherit: false,
                                    ..
                                } | ConstraintKind::NotNull { .. }
                            )
                            || c.no_inherit
                    });
                if was_valid {
                    return Ok(());
                }
                for oid in self.cat.inheritors(t.oid).into_iter().skip(1) {
                    let Some(child) = self.cat.table(oid).map(|c| (**c).clone()) else {
                        continue;
                    };
                    if child.constraints.iter().any(|c| c.name == cmd.name) {
                        let child = self.alter_cmd(child, cmd)?;
                        self.cat.put_table(child);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// ADD COLUMN on a parent: each child gets the column (or merges its own of the name).
    fn add_column_children(
        &mut self,
        before: &Table,
        parent: &Table,
        cmd: &pb::AlterTableCmd,
    ) -> Result<()> {
        let Some(pg_query::NodeEnum::ColumnDef(cd)) =
            cmd.def.as_deref().and_then(crate::sql::binder::ast::inner)
        else {
            return Ok(());
        };
        let pcol = parent.columns.last().expect("added column").clone();
        let new_cons: Vec<Constraint> = parent
            .constraints
            .iter()
            .filter(|c| !before.constraints.iter().any(|b| b.oid == c.oid))
            .cloned()
            .collect();
        // Children don't get the parent's indexes or foreign keys.
        let mut ccmd = cmd.clone();
        let mut ccd = (**cd).clone();
        // (A partitioned table's partitions share its identity column and sequence.)
        let partitioned = parent.partkey.is_some();
        ccd.constraints.retain(|c| {
            use pb::ConstrType as C;
            matches!(crate::sql::binder::ast::inner(c), Some(pg_query::NodeEnum::Constraint(k))
                if (matches!(C::try_from(k.contype), Ok(C::ConstrNull | C::ConstrNotnull | C::ConstrDefault | C::ConstrCheck | C::ConstrGenerated))
                    || partitioned && C::try_from(k.contype) == Ok(C::ConstrIdentity))
                    && !k.is_no_inherit)
        });
        ccmd.def = Some(Box::new(pb::Node {
            node: Some(pg_query::NodeEnum::ColumnDef(Box::new(ccd))),
        }));
        let child_oids: Vec<u32> = self.cat.children(parent.oid).to_vec();
        for coid in child_oids {
            // (As it is now: a diamond's child may have got the column on the way.)
            let Some(mut child) = self.cat.table(coid).map(|t| Table::clone(t)) else {
                continue;
            };
            if let Some(i) = child.column_index(&pcol.name) {
                if child.columns[i].ty != pcol.ty || child.columns[i].domain != pcol.domain {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "child table \"{}\" has different type for column \"{}\"",
                            child.name, pcol.name
                        ),
                    ));
                }
                self.notice(format!(
                    "merging definition of column \"{}\" for child \"{}\"",
                    pcol.name, child.name
                ));
                child.columns[i].inhcount += 1;
                // The column's CHECK constraints reach it all the same (merging with a copy
                // it has).
                for pc in new_cons.iter().filter(|c| matches!(c.kind, ConstraintKind::Check { .. })) {
                    let ConstraintKind::Check { expr, .. } = &pc.kind else { continue };
                    if let Some(c) = child.constraints.iter_mut().find(|c| c.name == pc.name) {
                        if matches!(&c.kind, ConstraintKind::Check { expr: e, .. } if e == expr) {
                            self.notice(format!(
                                "merging constraint \"{}\" with inherited definition",
                                pc.name
                            ));
                            c.inhcount += 1;
                        }
                        continue;
                    }
                    let mut c = Constraint::new(
                        self.cat.alloc_oid(),
                        pc.name.clone(),
                        pc.kind.clone(),
                        pc.validated,
                    );
                    c.enforced = pc.enforced;
                    c.inhcount = 1;
                    c.islocal = false;
                    child.constraints.push(c);
                }
                self.cat.put_table(child);
                continue;
            }
            let old = child.clone();
            let mut child = self.alter_cmd(child, &ccmd)?;
            let idx = child.columns.len() - 1;
            child.columns[idx].inhcount = 1;
            child.columns[idx].islocal = false;
            for c in child
                .constraints
                .iter_mut()
                .filter(|c| !old.constraints.iter().any(|b| b.oid == c.oid))
            {
                let from = new_cons.iter().find(|p| match (&p.kind, &c.kind) {
                    (ConstraintKind::NotNull { .. }, ConstraintKind::NotNull { .. }) => true,
                    (
                        ConstraintKind::Check { expr: a, .. },
                        ConstraintKind::Check { expr: b, .. },
                    ) => a == b,
                    _ => false,
                });
                if let Some(p) = from {
                    c.name = p.name.clone();
                }
                c.inhcount = 1;
                c.islocal = false;
            }
            self.cat.put_table(child.clone());
            if !self.cat.children(child.oid).is_empty() {
                self.add_column_children(&old, &child, &ccmd)?;
            }
        }
        Ok(())
    }

    /// SET NOT NULL on a parent: the children get the constraint (named as the parent's).
    pub(super) fn not_null_children(&mut self, parent: u32, col: &str, name: &str) -> Result<()> {
        self.not_null_children_valid(parent, col, name, true, false)
    }

    /// SET NOT NULL over an existing (perhaps NOT VALID) not-null: the children's copies
    /// are checked and become valid.
    fn validate_not_null_children(&mut self, parent: u32, col: &str) -> Result<()> {
        for mut child in self.child_tables(parent) {
            let Some(i) = child.column_index(col) else {
                continue;
            };
            let Some(k) = child.constraints.iter().position(
                |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i),
            ) else {
                continue;
            };
            if !child.constraints[k].validated {
                self.check_not_null(&child, i)?;
                child.constraints[k].validated = true;
            }
            let oid = child.oid;
            self.cat.put_table(child);
            self.validate_not_null_children(oid, col)?;
        }
        Ok(())
    }

    /// `not_null_children`; `valid` false for a NOT VALID one (the children's rows aren't
    /// checked, and their new copies are NOT VALID too).
    pub(super) fn not_null_children_valid(
        &mut self,
        parent: u32,
        col: &str,
        name: &str,
        valid: bool,
        hint: bool,
    ) -> Result<()> {
        let kids: Vec<u32> = self.child_tables(parent).iter().map(|c| c.oid).collect();
        for oid in kids {
            // Fresh: an earlier child's recursion may have reached it (multiple inheritance).
            let Some(mut child) = self.cat.table(oid).map(|c| (**c).clone()) else {
                continue;
            };
            let Some(i) = child.column_index(col) else {
                continue;
            };
            let cname = child.name.clone();
            if let Some(c) = child
                .constraints
                .iter_mut()
                .find(|c| matches!(c.kind, ConstraintKind::NotNull { column } if column == i))
            {
                if c.no_inherit {
                    let e = Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \"{cname}\"", c.name));
                    return Err(if hint {
                        e.hint("You might need to make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT.")
                    } else {
                        e
                    });
                }
                c.inhcount += 1;
                self.cat.put_table(child);
                continue;
            }
            if valid {
                self.check_not_null(&child, i)?;
            }
            child.columns[i].not_null = true;
            let mut c = Constraint::new(
                self.cat.alloc_oid(),
                name.to_string(),
                ConstraintKind::NotNull { column: i },
                true,
            );
            c.inhcount = 1;
            c.islocal = false;
            c.validated = valid;
            child.constraints.push(c);
            let oid = child.oid;
            self.cat.put_table(child);
            self.not_null_children_valid(oid, col, name, valid, hint)?;
        }
        Ok(())
    }

    /// ADD CONSTRAINT ... CHECK on a parent: each child gets it (or merges its own).
    fn check_children(&mut self, parent: u32, con: &Constraint) -> Result<()> {
        let ConstraintKind::Check { expr, .. } = &con.kind else {
            return Ok(());
        };
        for mut child in self.child_tables(parent) {
            if let Some(c) = child.constraints.iter_mut().find(|c| c.name == con.name) {
                match &c.kind {
                    ConstraintKind::Check { expr: e, no_inherit } if e == expr => {
                        // (MergeWithExistingConstraint, for an inherited copy.)
                        let what = if *no_inherit {
                            Some("non-inherited")
                        } else if con.enforced && con.validated && c.enforced && !c.validated {
                            Some("NOT VALID")
                        } else if con.enforced && !c.enforced {
                            Some("NOT ENFORCED")
                        } else {
                            None
                        };
                        if let Some(what) = what {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!(
                                    "constraint \"{}\" conflicts with {what} constraint on relation \"{}\"",
                                    con.name, child.name
                                ),
                            ));
                        }
                        self.notice(format!(
                            "merging constraint \"{}\" with inherited definition",
                            con.name
                        ));
                        c.inhcount += 1;
                        // A partition's constraint is inherited once and never local.
                        if child.partbound.is_some() {
                            c.inhcount = 1;
                            c.islocal = false;
                        }
                        self.cat.put_table(child);
                        continue;
                    }
                    _ => {
                        return Err(Error::new(
                            code::DUPLICATE_OBJECT,
                            format!(
                                "constraint \"{}\" for relation \"{}\" already exists",
                                con.name, child.name
                            ),
                        ));
                    }
                }
            }
            if con.validated {
                let e = self.bind_check(&child, expr)?;
                self.validate_check(&child, &con.name, &e)?;
            }
            let mut c = Constraint::new(
                self.cat.alloc_oid(),
                con.name.clone(),
                con.kind.clone(),
                con.validated,
            );
            c.enforced = con.enforced;
            c.inhcount = 1;
            c.islocal = false;
            child.constraints.push(c);
            let oid = child.oid;
            self.cat.put_table(child);
            self.check_children(oid, con)?;
        }
        Ok(())
    }

    /// DROP CONSTRAINT (or DROP NOT NULL) on a parent, for the children's copies.
    fn drop_constraint_children(
        &mut self,
        parent: u32,
        name: &str,
        recurse: bool,
        cascade: bool,
    ) -> Result<()> {
        let kids: Vec<u32> = self.child_tables(parent).iter().map(|c| c.oid).collect();
        for oid in kids {
            // Fresh: an earlier child's recursion may have reached it (multiple inheritance).
            let Some(mut child) = self.cat.table(oid).map(|c| (**c).clone()) else {
                continue;
            };
            let Some(i) = child
                .constraints
                .iter()
                .position(|c| c.name == name && c.inhcount > 0)
            else {
                continue;
            };
            if recurse && child.constraints[i].inhcount == 1 && !child.constraints[i].islocal {
                // (dropconstraint_internal checks a child it drops from, when recursing.)
                let kind = if child.is_foreign() { "foreign table" } else { "table" };
                self.require_owner(child.owner, kind, &child.name)?;
                if let ConstraintKind::NotNull { column } = child.constraints[i].kind {
                    child.columns[column].not_null = false;
                }
                let child = self.drop_constraint(child, name, false, cascade)?;
                let oid = child.oid;
                self.cat.put_table(child);
                self.drop_constraint_children(oid, name, true, cascade)?;
                continue;
            }
            let c = &mut child.constraints[i];
            c.inhcount -= 1;
            if !recurse {
                c.islocal = true;
            }
            self.cat.put_table(child);
        }
        Ok(())
    }
}

impl Ddl<'_, '_> {
    /// ALTER TABLE ... INHERIT parent: the table must already have the parent's columns and
    /// inheritable constraints.
    pub(super) fn add_inherit(&mut self, mut t: Table, parent: &Table) -> Result<Table> {
        self.require_owner(parent.owner, "table", &parent.name)?;
        if t.of_type != 0 {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                "cannot change inheritance of typed table",
            ));
        }
        if parent.temporary && !t.temporary {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot inherit from temporary relation \"{}\"", parent.name),
            ));
        }
        if t.inherits.contains(&parent.oid) {
            return Err(Error::new(
                code::DUPLICATE_TABLE,
                format!(
                    "relation \"{}\" would be inherited from more than once",
                    parent.name
                ),
            ));
        }
        if self.cat.inheritors(t.oid).contains(&parent.oid) {
            return Err(
                Error::new(code::DUPLICATE_TABLE, "circular inheritance not allowed").detail(
                    format!("\"{}\" is already a child of \"{}\".", parent.name, t.name),
                ),
            );
        }
        if t.partbound.is_none()
            && let Some(x) = self.transition_row_trigger(t.oid)
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "trigger \"{x}\" prevents table \"{}\" from becoming an inheritance child",
                    t.name
                ),
            )
            .detail(
                "ROW triggers with transition tables are not supported in inheritance hierarchies.",
            ));
        }
        let mismatch = |msg: String| Error::new(code::DATATYPE_MISMATCH, msg);
        for (pi, pc) in parent.live_columns() {
            let Some(i) = t.column_index(&pc.name) else {
                return Err(mismatch(format!(
                    "child table is missing column \"{}\"",
                    pc.name
                )));
            };
            // Only an inheritable not-null must be matched.
            let inh_not_null = pc.not_null
                && !parent.constraints.iter().any(|k| {
                    k.no_inherit && matches!(k.kind, ConstraintKind::NotNull { column } if column == pi)
                });
            let c = &t.columns[i];
            if c.ty != pc.ty || c.domain != pc.domain {
                return Err(mismatch(format!(
                    "child table \"{}\" has different type for column \"{}\"",
                    t.name, pc.name
                )));
            }
            if collation_of(c) != collation_of(pc) {
                return Err(mismatch(format!(
                    "child table \"{}\" has different collation for column \"{}\"",
                    t.name, pc.name
                )));
            }
            if inh_not_null && !c.not_null {
                return Err(mismatch(format!(
                    "column \"{}\" in child table \"{}\" must be marked NOT NULL",
                    pc.name, t.name
                )));
            }
            if pc.generated.is_some() && c.generated.is_none() {
                return Err(mismatch(format!(
                    "column \"{}\" in child table must be a generated column",
                    pc.name
                )));
            }
            if pc.generated.is_none() && c.generated.is_some() {
                return Err(mismatch(format!(
                    "column \"{}\" in child table must not be a generated column",
                    pc.name
                )));
            }
            if pc.generated.is_some() && pc.virtual_gen != c.virtual_gen {
                return Err(mismatch(format!(
                    "column \"{}\" inherits from generated column of different kind",
                    pc.name
                ))
                .detail(kind_detail(pc, c)));
            }
        }
        // MergeConstraintsIntoExisting: each of the parent's inheritable check and not-null
        // constraints (in name order) must have a compatible counterpart, whose inheritance
        // count goes up (a partition's isn't its own any more).
        let mut pcons: Vec<&Constraint> = (parent.constraints.iter())
            .filter(|c| match &c.kind {
                ConstraintKind::Check { no_inherit, .. } => !no_inherit,
                ConstraintKind::NotNull { .. } => !c.no_inherit,
                _ => false,
            })
            .collect();
        pcons.sort_by(|a, b| a.name.cmp(&b.name));
        let conflict = |what: &str, name: &str, t: &Table| {
            Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!(
                    "constraint \"{name}\" conflicts with {what} constraint on child table \"{}\"",
                    t.name
                ),
            )
        };
        let mut matched: Vec<usize> = Vec::new();
        for pc in pcons {
            let found = match &pc.kind {
                ConstraintKind::Check { .. } => t.constraints.iter().position(|c| {
                    matches!(c.kind, ConstraintKind::Check { .. }) && c.name == pc.name
                }),
                ConstraintKind::NotNull { column } => {
                    let i = t.column_index(&parent.columns[*column].name);
                    t.constraints.iter().position(|c| {
                        matches!(c.kind, ConstraintKind::NotNull { column } if Some(column) == i)
                    })
                }
                _ => None,
            };
            let Some(k) = found else {
                return Err(match &pc.kind {
                    ConstraintKind::NotNull { column } => mismatch(format!(
                        "column \"{}\" in child table \"{}\" must be marked NOT NULL",
                        parent.columns[*column].name, t.name
                    )),
                    _ => mismatch(format!("child table is missing constraint \"{}\"", pc.name)),
                });
            };
            let c = &t.constraints[k];
            let c_no_inherit = match &c.kind {
                ConstraintKind::Check { no_inherit, .. } => *no_inherit,
                _ => c.no_inherit,
            };
            if let (ConstraintKind::Check { expr, .. }, ConstraintKind::Check { expr: e, .. }) =
                (&pc.kind, &c.kind)
                && e != expr
            {
                return Err(mismatch(format!(
                    "child table \"{}\" has different definition for check constraint \"{}\"",
                    t.name, pc.name
                )));
            }
            if c_no_inherit {
                return Err(conflict("non-inherited", &c.name, &t));
            }
            if pc.validated && c.enforced && !c.validated {
                return Err(conflict("NOT VALID", &c.name, &t));
            }
            if pc.enforced && !c.enforced {
                return Err(conflict("NOT ENFORCED", &c.name, &t));
            }
            matched.push(k);
        }
        // Everything matches: the columns and constraints now come from the parent too.
        for (_, pc) in parent.live_columns() {
            let i = t.column_index(&pc.name).expect("checked");
            t.columns[i].inhcount += 1;
            // (A partition's columns are never its own: MergeAttributesIntoExisting.)
            if parent.partkey.is_some() {
                t.columns[i].islocal = false;
            }
        }
        for k in matched {
            let c = &mut t.constraints[k];
            c.inhcount += 1;
            if parent.partkey.is_some() {
                c.islocal = false;
            }
        }
        t.inherits.push(parent.oid);
        Ok(t)
    }

    /// ALTER TABLE ... NO INHERIT parent: what came only from the parent becomes the table's own.
    pub(super) fn drop_inherit(&mut self, mut t: Table, parent: &Table) -> Result<Table> {
        if !t.inherits.contains(&parent.oid) {
            return Err(Error::new(
                code::UNDEFINED_TABLE,
                format!(
                    "relation \"{}\" is not a parent of relation \"{}\"",
                    parent.name, t.name
                ),
            ));
        }
        for (_, pc) in parent.live_columns() {
            if let Some(i) = t.column_index(&pc.name) {
                let c = &mut t.columns[i];
                c.inhcount = c.inhcount.saturating_sub(1);
                if c.inhcount == 0 {
                    c.islocal = true;
                }
            }
        }
        for pc in &parent.constraints {
            let found = match &pc.kind {
                ConstraintKind::Check {
                    no_inherit: false, ..
                } => t
                    .constraints
                    .iter_mut()
                    .find(|c| c.name == pc.name && c.inhcount > 0),
                ConstraintKind::NotNull { column } => {
                    match t.column_index(&parent.columns[*column].name) {
                        Some(i) => t.constraints.iter_mut().find(|c| {
                            matches!(c.kind, ConstraintKind::NotNull { column } if column == i)
                                && c.inhcount > 0
                        }),
                        None => None,
                    }
                }
                _ => None,
            };
            if let Some(c) = found {
                c.inhcount -= 1;
                if c.inhcount == 0 {
                    c.islocal = true;
                }
            }
        }
        t.inherits.retain(|p| *p != parent.oid);
        Ok(t)
    }
}
