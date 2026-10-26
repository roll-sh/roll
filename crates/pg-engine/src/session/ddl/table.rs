//! CREATE TABLE, CREATE TABLE AS, ALTER TABLE, TRUNCATE, LOCK, and the constraint and
//! table-rewrite machinery they share.

use super::{Ddl, def_elems};
use crate::catalog::system;
use crate::catalog::{
    Catalog, Column, Constraint, ConstraintKind, FkAction, Identity, Index, IndexColumn, IndexKey,
    Relation, Sequence, Table, ViewDef, quote_ident,
};
use crate::error::{Error, Result, code};
use crate::exec::dml::with_failing_row;
use crate::exec::scan::{data_key, encode_table_row, index_entry_key, index_value_prefix};
use crate::exec::{Row, collect};
use crate::sql::binder::ast::{self, inner, names, with_pos};
use crate::sql::binder::{Clause, bound_indexes, resolve_type_name, serial_type};
use crate::sql::expr::{Expr, eval};
use crate::sql::plan::{BoundIndex, OutCol};
use crate::types::text::format_value;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::collections::HashSet;
use std::sync::Arc;

/// The error for not-null declarations of a column that disagree about NO INHERIT
/// (transformColumnDefinition's when both are in the column definition,
/// AddRelationNotNullConstraints' when one is a table constraint).
fn no_inherit_conflict(table_level: bool, column: &str) -> Error {
    Error::new(
        code::SYNTAX_ERROR,
        if table_level {
            format!("conflicting NO INHERIT declaration for not-null constraint on column \"{column}\"")
        } else {
            format!("conflicting NO INHERIT declarations for not-null constraints on column \"{column}\"")
        },
    )
}

/// CREATE TABLE's not-null declarations, one per column: a name given once is the
/// constraint's, two different names or disagreeing NO INHERIT are errors.
fn merge_not_nulls(not_nulls: Vec<Pending>, t: &Table) -> Result<Vec<Pending>> {
    let mut merged: Vec<Pending> = Vec::new();
    for p in not_nulls {
        let Pending::NotNull {
            col,
            name,
            no_inherit,
            table_level,
            ..
        } = &p
        else {
            merged.push(p);
            continue;
        };
        let Some(Pending::NotNull {
            name: n0,
            no_inherit: ni0,
            table_level: tl0,
            ..
        }) = merged
            .iter_mut()
            .find(|m| matches!(m, Pending::NotNull { col: c, .. } if c == col))
        else {
            merged.push(p);
            continue;
        };
        if let (Some(a), Some(b)) = (n0.as_ref(), name.as_ref())
            && a != b
        {
            return Err(Error::new(
                code::INTERNAL_ERROR,
                format!("conflicting not-null constraint names \"{a}\" and \"{b}\""),
            ));
        }
        if *ni0 != *no_inherit {
            return Err(no_inherit_conflict(*table_level || *tl0, &t.columns[*col].name));
        }
        if n0.is_none() {
            *n0 = name.clone();
        }
    }
    Ok(merged)
}

/// Placeholder attrdef oid of a column default copied by LIKE: allocated once the TOAST
/// table exists.
const LIKE_DEF: u32 = u32::MAX;

/// A constraint waiting for its table's columns to exist.
pub(super) enum Pending {
    NotNull {
        col: usize,
        name: Option<String>,
        no_inherit: bool,
        /// ALTER TABLE ... ADD CONSTRAINT ... NOT NULL ... NOT VALID.
        not_valid: bool,
        /// Declared as a table constraint (not in a column definition).
        table_level: bool,
    },
    Check {
        name: Option<String>,
        expr: String,
        /// The expression as written in the statement (errors in CREATE TABLE point into
        /// it).
        raw: Option<Box<pb::Node>>,
        col: Option<usize>,
        not_valid: bool,
        no_inherit: bool,
        enforced: bool,
    },
    Index {
        primary: bool,
        name: Option<String>,
        cols: Vec<usize>,
        include: Vec<usize>,
        nulls_not_distinct: bool,
        loc: i32,
        deferrable: bool,
        initially_deferred: bool,
        options: Vec<String>,
        /// USING INDEX TABLESPACE.
        space: String,
    },
    Exclusion(Box<PendingExclusion>),
    Fk(Box<PendingFk>),
    /// LIKE src INCLUDING ...: what is copied once the table exists (expandTableLikeClause,
    /// after the TOAST table): the copied defaults' attrdefs (`defaults`, column indexes),
    /// the checks (by name), then indexes, statistics and comments.
    Like {
        src: Arc<Table>,
        defaults: Vec<usize>,
        checks: Vec<Pending>,
        indexes: bool,
        comments: bool,
        constraints: bool,
        statistics: bool,
    },
}

pub(super) struct PendingExclusion {
    name: Option<String>,
    method: String,
    /// IndexElem nodes, with their operators.
    elems: Vec<pb::Node>,
    ops: Vec<String>,
    including: Vec<pb::Node>,
    predicate: Option<pb::Node>,
    options: Vec<pb::Node>,
    deferrable: bool,
    initially_deferred: bool,
    /// PRIMARY KEY ('p') or UNIQUE ('u') ... WITHOUT OVERLAPS.
    period: Option<char>,
}

pub(super) struct PendingFk {
    name: Option<String>,
    pub(super) cols: Vec<usize>,
    ref_rv: pb::RangeVar,
    ref_cols: Vec<String>,
    on_delete: FkAction,
    on_update: FkAction,
    match_full: bool,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
    enforced: bool,
    /// FOREIGN KEY (..., PERIOD col) / REFERENCES t (..., PERIOD col).
    period: bool,
    pk_period: bool,
    /// ON DELETE SET NULL / DEFAULT (columns).
    del_set_cols: Vec<String>,
}

/// transformConstraintAttrs: a column's constraints with DEFERRABLE / INITIALLY ... /
/// [NOT] ENFORCED folded into the constraint they follow.
pub(super) fn transform_constraint_attrs(list: &[pb::Node]) -> Result<Vec<pb::Constraint>> {
    use pb::ConstrType as C;
    let mut out: Vec<pb::Constraint> = Vec::new();
    let (mut saw_deferrability, mut saw_initially, mut saw_enforced) = (false, false, false);
    let misplaced = |what: &str, loc: i32| {
        with_pos(
            Error::new(code::SYNTAX_ERROR, format!("misplaced {what} clause")),
            loc,
        )
    };
    let multiple = |what: &str, loc: i32| {
        with_pos(
            Error::new(
                code::SYNTAX_ERROR,
                format!("multiple {what} clauses not allowed"),
            ),
            loc,
        )
    };
    let must_defer = |loc: i32| {
        with_pos(
            Error::new(
                code::SYNTAX_ERROR,
                "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
            ),
            loc,
        )
    };
    for n in list {
        let Some(NodeEnum::Constraint(con)) = inner(n) else {
            continue;
        };
        let loc = con.location;
        let kind = C::try_from(con.contype).unwrap_or(C::Undefined);
        let last = out.last_mut();
        let supports = last.as_ref().is_some_and(|l| {
            matches!(
                C::try_from(l.contype),
                Ok(C::ConstrPrimary | C::ConstrUnique | C::ConstrExclusion | C::ConstrForeign)
            )
        });
        let enforceable = last.as_ref().is_some_and(|l| {
            matches!(
                C::try_from(l.contype),
                Ok(C::ConstrCheck | C::ConstrForeign)
            )
        });
        match kind {
            C::ConstrAttrDeferrable | C::ConstrAttrNotDeferrable => {
                let word = if kind == C::ConstrAttrDeferrable {
                    "DEFERRABLE"
                } else {
                    "NOT DEFERRABLE"
                };
                let Some(last) = last.filter(|_| supports) else {
                    return Err(misplaced(word, loc));
                };
                if saw_deferrability {
                    return Err(multiple("DEFERRABLE/NOT DEFERRABLE", loc));
                }
                saw_deferrability = true;
                last.deferrable = kind == C::ConstrAttrDeferrable;
                if !last.deferrable && saw_initially && last.initdeferred {
                    return Err(must_defer(loc));
                }
            }
            C::ConstrAttrDeferred | C::ConstrAttrImmediate => {
                let word = if kind == C::ConstrAttrDeferred {
                    "INITIALLY DEFERRED"
                } else {
                    "INITIALLY IMMEDIATE"
                };
                let Some(last) = last.filter(|_| supports) else {
                    return Err(misplaced(word, loc));
                };
                if saw_initially {
                    return Err(multiple("INITIALLY IMMEDIATE/DEFERRED", loc));
                }
                saw_initially = true;
                last.initdeferred = kind == C::ConstrAttrDeferred;
                if last.initdeferred {
                    if !saw_deferrability {
                        last.deferrable = true;
                    } else if !last.deferrable {
                        return Err(must_defer(loc));
                    }
                }
            }
            C::ConstrAttrEnforced | C::ConstrAttrNotEnforced => {
                let word = if kind == C::ConstrAttrEnforced {
                    "ENFORCED"
                } else {
                    "NOT ENFORCED"
                };
                let Some(last) = last.filter(|_| enforceable) else {
                    return Err(misplaced(word, loc));
                };
                if saw_enforced {
                    return Err(multiple("ENFORCED/NOT ENFORCED", loc));
                }
                saw_enforced = true;
                last.is_enforced = kind == C::ConstrAttrEnforced;
                if !last.is_enforced {
                    last.skip_validation = true;
                    last.initially_valid = false;
                }
            }
            _ => {
                out.push((**con).clone());
                (saw_deferrability, saw_initially, saw_enforced) = (false, false, false);
            }
        }
    }
    Ok(out)
}

fn fk_action(s: &str) -> FkAction {
    match s {
        "r" => FkAction::Restrict,
        "c" => FkAction::Cascade,
        "n" => FkAction::SetNull,
        "d" => FkAction::SetDefault,
        _ => FkAction::NoAction,
    }
}

impl Ddl<'_, '_> {
    // ── CREATE TABLE ──

    /// The stand-alone composite type of CREATE TABLE ... OF / ALTER TABLE ... OF.
    fn typed_table_type(&self, tn: &pb::TypeName) -> Result<Arc<crate::catalog::CompositeType>> {
        let (ty, _) = resolve_type_name(&self.cat, self.search_path(), tn)?;
        let comp = match &ty {
            DataType::Row(rt) => self.cat.composites.get(&rt.oid).cloned(),
            _ => None,
        };
        match comp {
            Some(c) => Ok(c),
            _ if matches!(ty, DataType::Row(_)) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("type {} is the row type of another table", ty.display()),
            )
            .detail(
                "A typed table must use a stand-alone composite type created with CREATE TYPE.",
            )),
            _ => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("type {} is not a composite type", ty.display()),
            )),
        }
    }

    /// ON COMMIT: only for temporary tables.
    fn check_on_commit(&self, action: i32, rv: &pb::RangeVar) -> Result<()> {
        use pb::OnCommitAction as A;
        let action = A::try_from(action).unwrap_or(A::OncommitNoop);
        if !matches!(action, A::OncommitNoop | A::Undefined) && rv.relpersistence != "t" {
            return Err(Error::new(
                code::INVALID_TABLE_DEFINITION,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        Ok(())
    }

    /// Remember what commits do to a new temporary table (register_on_commit_action).
    fn register_on_commit(&self, action: i32, oid: u32) {
        use pb::OnCommitAction as A;
        match A::try_from(action) {
            Ok(A::OncommitDeleteRows) => self.ctx.sess.on_commit.borrow_mut().push((oid, false)),
            Ok(A::OncommitDrop) => self.ctx.sess.on_commit.borrow_mut().push((oid, true)),
            _ => {}
        }
    }

    pub(super) fn create_table(&mut self, s: &pb::CreateStmt) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let of_type = match &s.of_typename {
            Some(tn) => Some(self.typed_table_type(tn)?),
            None => None,
        };
        let (schema, name) = self
            .create_target(rv)
            // (A schema lookup's error points at the name; heap_create's system-catalog
            // check doesn't.)
            .map_err(|e| {
                if e.message.starts_with("permission denied to create") {
                    e
                } else {
                    crate::sql::binder::ast::with_pos(e, rv.location)
                }
            })?;
        if s.partspec.is_some()
            && !s.inh_relations.is_empty()
            && s.partbound.is_none()
            && !(s.if_not_exists && self.cat.name_taken(schema, &name))
        {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "cannot create partitioned table as inheritance child",
            ));
        }
        self.check_on_commit(s.oncommit, rv)?;
        if s.partspec.is_some() && rv.relpersistence == "u" {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "partitioned tables cannot be unlogged",
            ));
        }
        // DefineRelation's tablespace: named, a partition's parent's, or the default (a
        // foreign table has none).
        let spc = {
            let inherited = if s.partbound.is_some() {
                s.inh_relations
                    .first()
                    .and_then(|n| match inner(n) {
                        Some(NodeEnum::RangeVar(p)) => self.lookup(p).ok().flatten(),
                        _ => None,
                    })
                    .map(|r| self.cat.rel_tablespace(r.oid()))
                    .filter(|o| *o != 0)
            } else {
                None
            };
            let spc = self.choose_tablespace(
                &s.tablespacename,
                s.partspec.is_some(),
                rv.relpersistence == "t",
                inherited,
            )?;
            if self.creating_foreign.is_some() { 0 } else { spc }
        };
        if self.cat.name_taken(schema, &name) {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists, skipping"),
                );
                return Ok("CREATE TABLE".into());
            }
            return Err(Error::new(
                code::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }
        // As DefineRelation allocates them: the serial and identity columns' sequences, the
        // table and its row type, then (below) defaults, checks, not-nulls and indexes.
        let own_seqs = s
            .table_elts
            .iter()
            .filter(|e| match inner(e) {
                Some(NodeEnum::ColumnDef(cd)) => {
                    cd.type_name.as_ref().and_then(serial_type).is_some()
                        || cd.constraints.iter().any(|c| matches!(inner(c), Some(NodeEnum::Constraint(c)) if c.contype == pb::ConstrType::ConstrIdentity as i32))
                }
                _ => false,
            })
            .count();
        self.seq_oids = (0..own_seqs).map(|_| self.cat.alloc_oid()).collect();
        let oid = self.cat.alloc_oid();
        self.register_on_commit(s.oncommit, oid);
        let (rowtype, rowarray) = self.cat.alloc_rowtype();
        let mut t = Table {
            oid,
            schema,
            name: name.clone(),
            owner: self.me(),
            columns: Vec::new(),
            pkey: None,
            pkey_index: None,
            pkey_unkeyed: false,
            indexes: Vec::new(),
            constraints: Vec::new(),
            next_index_id: 1,
            rowid_seq: None,
            // (RangeVarAdjustRelationPersistence: made in a temporary schema, it is
            // temporary.)
            temporary: rv.relpersistence == "t"
                || self
                    .cat
                    .schemas
                    .get(&schema)
                    .is_some_and(|s| s.name.starts_with("pg_temp")),
            unlogged: rv.relpersistence == "u",
            comment: None,
            rowtype,
            rowarray,
            toast: 0,
            toast_relname: None,
            toast_indname: None,
            rls: false,
            rls_force: false,
            of_type: 0,
            inherits: Vec::new(),
            partkey: None,
            partbound: None,
            options: Vec::new(),
            replident: None,
            foreign: self.creating_foreign.take(),
            detach_pending: false,
        };
        t.options = heap_options(&s.options, &[], false)?;
        if s.partspec.is_some() && partitioned_refuses(&t.options) {
            return Err(partitioned_options());
        }
        let mut pending = Vec::new();
        let mut seqs = Vec::new();
        // PARTITION OF: the parent's columns, and a bound against its key.
        let mut bound = None;
        let parents = match &s.partbound {
            Some(spec) => {
                let Some(NodeEnum::RangeVar(prv)) = s.inh_relations.first().and_then(inner) else {
                    return Err(crate::error::internal("partition without parent"));
                };
                let parent = self.partitioned_parent(prv)?;
                if t.temporary && !parent.temporary {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "cannot create a temporary relation as partition of permanent relation \"{}\"",
                            parent.name
                        ),
                    ));
                }
                if !t.temporary && parent.temporary {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "cannot create a permanent relation as partition of temporary relation \"{}\"",
                            parent.name
                        ),
                    ));
                }
                self.require_owner(parent.owner, "table", &parent.name)?;
                self.not_in_use(parent.oid, "CREATE TABLE .. PARTITION OF", &parent.name)?;
                bound = Some((self.partition_bound(&parent, spec)?, spec, parent.clone()));
                vec![parent]
            }
            None => self.inherit_parents(&s.inh_relations, t.temporary)?,
        };
        // A partitioned table's `roll_pk_clustered` passes to its new partitions (unless
        // the partition's own WITH names it).
        if bound.is_some()
            && !t.is_foreign()
            && parents[0].pk_clustered()
            && !names_pk_clustered(&t.options)
        {
            t.options.push(format!("{}=true", crate::catalog::PK_CLUSTERED));
        }
        let mut inh = if parents.is_empty() {
            None
        } else {
            Some(self.inherit_from(&mut t, &parents)?)
        };
        if bound.is_some() {
            // A partition's identity columns share their partitioned table's sequence.
            share_identity(&parents[0], &mut t);
        }
        let mut local_pos = 0;
        if let Some(comp) = &of_type {
            // The type's attributes are the columns; `name WITH OPTIONS ...` adds to one.
            t.of_type = comp.oid;
            let mut seen = std::collections::HashSet::new();
            for e in &s.table_elts {
                if let Some(NodeEnum::ColumnDef(cd)) = inner(e) {
                    if !comp.fields.iter().any(|(n, _)| *n == cd.colname) {
                        return Err(Error::new(
                            code::UNDEFINED_COLUMN,
                            format!("column \"{}\" does not exist", cd.colname),
                        ));
                    }
                    // (MergeAttributes: one set of options merges into the type's column.)
                    if !seen.insert(cd.colname.as_str()) {
                        return Err(Error::new(
                            code::DUPLICATE_COLUMN,
                            format!("column \"{}\" specified more than once", cd.colname),
                        ));
                    }
                }
            }
            for (fname, _) in &comp.fields {
                let opts = s.table_elts.iter().find_map(|e| match inner(e) {
                    Some(NodeEnum::ColumnDef(cd)) if cd.colname == *fname => Some((**cd).clone()),
                    _ => None,
                });
                let cd = opts.unwrap_or_else(|| pb::ColumnDef {
                    colname: fname.clone(),
                    is_local: true,
                    ..Default::default()
                });
                self.column_def(&mut t, &cd, &mut pending, &mut seqs)?;
            }
        }
        let mut late_not_nulls: Vec<pb::Constraint> = Vec::new();
        // Generated columns are checked after the loop, once every column exists (cookDefault
        // runs after the columns are made).
        // MergeAttributes: a partition's column list names each column once.
        if bound.is_some() {
            let mut seen = std::collections::HashSet::new();
            for e in &s.table_elts {
                if let Some(NodeEnum::ColumnDef(cd)) = inner(e)
                    && !seen.insert(cd.colname.as_str())
                {
                    return Err(Error::new(
                        code::DUPLICATE_COLUMN,
                        format!("column \"{}\" specified more than once", cd.colname),
                    ));
                }
            }
        }
        let outer_deferred = self.defer_generated.replace(Vec::new());
        let outer_defaults = self.defer_defaults.replace(Vec::new());
        for e in &s.table_elts {
            match inner(e) {
                Some(NodeEnum::ColumnDef(_)) if of_type.is_some() => {}
                Some(NodeEnum::ColumnDef(cd)) if bound.is_some() => {
                    self.partition_column_def(&mut t, cd, &mut pending)?
                }
                Some(NodeEnum::ColumnDef(cd)) => match &mut inh {
                    Some(inh) => {
                        local_pos += 1;
                        self.child_column_def(&mut t, inh, cd, local_pos, &mut pending, &mut seqs)?
                    }
                    None => self.column_def(&mut t, cd, &mut pending, &mut seqs)?,
                },
                // A table NOT NULL constraint may name a column defined after it.
                Some(NodeEnum::Constraint(c))
                    if c.contype == pb::ConstrType::ConstrNotnull as i32 =>
                {
                    late_not_nulls.push((**c).clone());
                }
                Some(NodeEnum::Constraint(c)) => {
                    if let Some(p) = self.table_constraint(&t, c, None)? {
                        pending.push(p);
                    }
                }
                Some(NodeEnum::TableLikeClause(l)) => {
                    self.like_clause(&mut t, l, &mut pending, &mut seqs)?
                }
                _ => return Err(crate::error::unsupported("this table element")),
            }
        }
        let deferred =
            std::mem::replace(&mut self.defer_generated, outer_deferred).unwrap_or_default();
        let defaults =
            std::mem::replace(&mut self.defer_defaults, outer_defaults).unwrap_or_default();
        // The expressions are cooked once the relation exists (its name may be in them),
        // in column order, defaults and generation expressions alike.
        if (!deferred.is_empty() || !defaults.is_empty()) && !self.cat.tables.contains_key(&t.oid)
        {
            self.cat.put_table(t.clone());
        }
        // (So are the serial columns' sequences, which transformColumnDefinition creates
        // before the table: a default may name one, nextval('t_id_seq').)
        if !defaults.is_empty() {
            for sq in &seqs {
                if !self.cat.sequences.contains_key(&sq.oid) {
                    self.cat.put_sequence(sq.clone());
                }
            }
        }
        let mut cook: Vec<(usize, Option<(pb::Node, DataType)>, Option<pb::Node>, String)> =
            Vec::new();
        for (name, raw) in deferred {
            // (Merged into an inherited column, it may have moved.)
            if let Some(idx) = t.column_index(&name) {
                cook.push((idx, None, raw, name));
            }
        }
        for (name, e, ty) in defaults {
            if let Some(idx) = t.column_index(&name) {
                cook.push((idx, Some((e, ty)), None, name));
            }
        }
        cook.sort_by_key(|c| c.0);
        for (idx, default, raw, name) in cook {
            match default {
                Some((e, ty)) => self.check_default(&e, &ty, &name)?,
                None => {
                    if let Some(g) = t.columns[idx].generated.clone() {
                        self.check_generated(&t, idx, &g, raw.as_ref())?;
                    }
                }
            }
        }
        for c in &late_not_nulls {
            if let Some(p) = self.table_constraint(&t, c, None)? {
                pending.push(p);
            }
        }
        if let Some(inh) = &inh {
            self.check_inherited(&t, inh)?;
        }
        if t.columns.len() > 1600 {
            return Err(Error::new(
                code::TOO_MANY_COLUMNS,
                "tables can have at most 1600 columns",
            ));
        }
        if let Some((b, spec, parent)) = bound {
            self.check_new_bound(&parent, &t.name, &b, spec, None)?;
            self.check_default_rows(&parent, &b, None)?;
            t.partbound = Some(b);
        }
        if let Some(spec) = &s.partspec {
            // The key may use the table's own row type, which exists by then.
            if t.rowtype == 0 {
                (t.rowtype, t.rowarray) = self.cat.alloc_rowtype();
            }
            self.cat.put_table(t.clone());
            t.partkey = Some(self.partition_spec(&t, spec)?);
        }
        self.seq_oids.clear();
        self.cat.put_table(t.clone());
        self.cat.set_rel_tablespace(t.oid, spc);
        for sq in seqs {
            self.cat.put_sequence(sq);
        }
        // Checks, then not-nulls (a primary key's columns last), then the rest as written.
        let (mut checks, mut not_nulls, mut rest) = (Vec::new(), Vec::new(), Vec::new());
        for p in pending {
            match p {
                Pending::Check { .. } => checks.push(p),
                Pending::NotNull { .. } => not_nulls.push(p),
                _ => rest.push(p),
            }
        }
        // (A partitioned table's CHECK NO INHERIT, a LIKE's too, is refused before its NOT
        // NULLs are looked at.)
        let no_inherit_check = |p: &Pending| matches!(p, Pending::Check { no_inherit: true, .. });
        if t.partkey.is_some()
            && (checks.iter().any(no_inherit_check)
                || rest.iter().any(|p| {
                    matches!(p, Pending::Like { checks, constraints: true, .. }
                        if checks.iter().any(no_inherit_check))
                }))
        {
            return Err(Error::new(
                code::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot add NO INHERIT constraint to partitioned table \"{}\"",
                    t.name
                ),
            ));
        }
        // transformIndexConstraints: the primary key's index comes first of the indexes the
        // constraints make (its oid is the lowest).
        if let Some(pk) = rest
            .iter()
            .position(|p| matches!(p, Pending::Index { primary: true, .. }))
            && let Some(first) = rest
                .iter()
                .position(|p| matches!(p, Pending::Index { .. } | Pending::Exclusion(_)))
            && first < pk
        {
            let p = rest.remove(pk);
            rest.insert(first, p);
        }
        let not_nulls = merge_not_nulls(not_nulls, &t)?;
        let mut not_nulls = not_nulls;
        for p in &rest {
            if let Pending::Index {
                primary: true,
                cols,
                ..
            } = p
            {
                for &c in cols {
                    // A primary key's column can't have a NO INHERIT not-null.
                    if let Some(Pending::NotNull { table_level, .. }) = not_nulls.iter().find(
                        |n| matches!(n, Pending::NotNull { col, no_inherit: true, .. } if *col == c),
                    ) {
                        return Err(no_inherit_conflict(*table_level, &t.columns[c].name));
                    }
                    let declared = not_nulls
                        .iter()
                        .any(|n| matches!(n, Pending::NotNull { col, .. } if *col == c));
                    // (An inherited one becomes local too.)
                    if !declared {
                        not_nulls.push(Pending::NotNull {
                            col: c,
                            name: None,
                            no_inherit: false,
                            not_valid: false,
                            table_level: false,
                        });
                    }
                }
            }
        }
        // In DefineRelation's order: a partition's copies of its parent's indexes, triggers
        // and foreign keys, the checks, then the not-nulls (the table's own, then inherited
        // ones not merged into those).
        if t.partbound.is_some()
            && let Some(parent) = t
                .inherits
                .first()
                .and_then(|p| self.cat.tables.get(p))
                .cloned()
        {
            t = self.clone_indexes(&parent, t, false)?;
            self.clone_triggers(&parent, &t)?;
            self.partition_fks(&parent, &mut t, false)?;
        }
        let t = self.apply_pending(t, checks, false)?;
        let mut t = self.apply_pending(t, not_nulls, false)?;
        // Inherited not-nulls not merged into the table's own, in order: one whose name
        // is taken by then (the table's own, or another parent's) gets a name of its own.
        for i in 0..t.constraints.len() {
            if t.constraints[i].oid != 0 {
                continue;
            }
            let taken = t.constraints.iter().enumerate().any(|(j, k)| {
                j != i && k.oid != 0 && k.name == t.constraints[i].name
            });
            if taken && let ConstraintKind::NotNull { column } = t.constraints[i].kind {
                let col = t.columns[column].name.clone();
                t.constraints[i].name = self.constraint_name(&t, &[col.as_str()], "not_null");
            }
            t.constraints[i].oid = self.cat.alloc_oid();
        }
        // The TOAST table comes before the indexes (ProcessUtilitySlow makes it right after
        // DefineRelation), then the LIKE clauses' copies, then the foreign keys.
        self.cat.add_toast(&mut t);
        rest.sort_by_key(|p| match p {
            Pending::Like { .. } => 1,
            Pending::Fk(_) => 2,
            _ => 0,
        });
        let mut t = self.apply_pending(t, rest, false)?;
        for c in t.columns.iter_mut().filter(|c| c.def_oid == LIKE_DEF) {
            c.def_oid = self.cat.alloc_oid();
        }
        self.cat.put_table(t);
        self.save()?;
        Ok("CREATE TABLE".into())
    }

    /// Have the ALTER TABLE rewrite table `oid` at its end, for `reason` (AT_REWRITE_*:
    /// 1 persistence, 2 a default's values, 4 a column's values, 8 access method).
    pub(super) fn rewrite_later(&mut self, oid: u32, reason: i32) {
        if !self.rewrites.contains(&oid) {
            self.rewrites.push(oid);
        }
        match self.rewrite_why.iter_mut().find(|w| w.0 == oid) {
            Some(w) => w.1 |= reason,
            None => self.rewrite_why.push((oid, reason)),
        }
    }

    /// table_rewrite event triggers, before table `oid` is rewritten.
    pub(super) fn fire_table_rewrite(&mut self, oid: u32) -> Result<()> {
        let reason = self
            .rewrite_why
            .iter()
            .find(|w| w.0 == oid)
            .map_or(0, |w| w.1);
        self.rewrite_why.retain(|w| w.0 != oid);
        if !self.cat.event_triggers.values().any(|e| e.event == "table_rewrite") {
            return Ok(());
        }
        // The triggers see the catalog as the command left it so far.
        self.save()?;
        super::event_trigger::table_rewrite(self.ctx, oid, reason)
    }

    /// Add a column from its definition (CREATE TABLE / ALTER TABLE ADD COLUMN).
    pub(super) fn column_def(
        &mut self,
        t: &mut Table,
        cd: &pb::ColumnDef,
        pending: &mut Vec<Pending>,
        seqs: &mut Vec<Sequence>,
    ) -> Result<()> {
        let colname = crate::catalog::truncate_ident(&cd.colname);
        system_name(&colname)?;
        if t.column_index(&colname).is_some() {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                format!("column \"{colname}\" specified more than once"),
            ));
        }
        let serial = cd.type_name.as_ref().and_then(serial_type);
        let (ty, domain) = match (&serial, cd.type_name.as_ref()) {
            (Some(ty), _) => (ty.clone(), None),
            (None, Some(tn)) => resolve_type_name(&self.cat, self.search_path(), tn)?,
            // A typed table's column: the type's attribute.
            (None, None) => {
                let field = self.cat.composites.get(&t.of_type).and_then(|c| {
                    c.fields
                        .iter()
                        .find(|(n, _)| *n == colname)
                        .map(|f| f.1.clone())
                });
                (
                    field.ok_or_else(|| crate::error::internal("column without type"))?,
                    None,
                )
            }
        };
        if matches!(ty, DataType::Unknown | DataType::Record | DataType::Void)
            || ty.is_polymorphic()
        {
            return Err(Error::new(
                code::INVALID_TABLE_DEFINITION,
                format!("column \"{colname}\" has pseudo-type {}", ty.display()),
            ));
        }
        check_not_self_member(t, &ty)?;
        let idx = t.columns.len();
        let mut col = Column::new(colname.clone(), ty.clone());
        col.domain = domain;
        col.ndims = Some(if serial.is_some() {
            0
        } else {
            cd.type_name
                .as_ref()
                .map_or(0, |tn| tn.array_bounds.len() as i16)
        });
        if !cd.storage_name.is_empty() {
            col.storage = storage_kind(&cd.storage_name, &ty)?;
        }
        if !cd.compression.is_empty() {
            if ty.typlen() != -1 {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "column data type {} does not support compression",
                        ty.sql_name()
                    ),
                ));
            }
            col.compression = compression_method(&cd.compression)?;
        }
        if let Some(cc) = &cd.coll_clause {
            let cn = names(&cc.collname);
            if crate::types::collation::resolve(&self.cat, self.search_path(), &cn).is_none() {
                return Err(with_pos(
                    Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "collation \"{}\" for encoding \"UTF8\" does not exist",
                            cn.join(".")
                        ),
                    ),
                    cc.location,
                ));
            }
            if !ty.collatable() {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("collations are not supported by type {}", ty.display()),
                    ),
                    cc.location,
                ));
            }
            col.collation = cn.last().cloned();
        } else if let Some(dc) = col
            .domain
            .and_then(|d| self.cat.domains.get(&d))
            .and_then(|d| d.collation.clone())
        {
            // A domain column takes the domain's collation.
            col.collation = Some(dc);
        }
        if serial.is_some() {
            let seq = self.owned_sequence(t, &colname, idx, &ty, &[])?;
            col.default = Some(format!(
                "nextval('{}'::regclass)",
                self.qualified(seq.schema, &seq.name).replace('\'', "''")
            ));
            col.seq = Some(seq.oid);
            col.not_null = true;
            pending.push(Pending::NotNull {
                col: idx,
                name: None,
                no_inherit: false,
                not_valid: false,
                table_level: false,
            });
            seqs.push(seq);
        }
        let mut saw_null = false;
        let mut gen_raw = None;
        let constraints = transform_constraint_attrs(&cd.constraints)?;
        let (mut saw_default, mut saw_identity, mut last_loc) = (false, false, -1);
        for c in &constraints {
            last_loc = c.location;
            use pb::ConstrType as C;
            let conname = if c.conname.is_empty() {
                None
            } else {
                Some(c.conname.clone())
            };
            match C::try_from(c.contype).unwrap_or(C::Undefined) {
                C::ConstrNull => {
                    if col.not_null && serial.is_none() {
                        return Err(with_pos(conflicting_null(&colname, &t.name), c.location));
                    }
                    saw_null = true;
                }
                C::ConstrNotnull => {
                    if saw_null {
                        return Err(with_pos(conflicting_null(&colname, &t.name), c.location));
                    }
                    col.not_null = true;
                    pending.push(Pending::NotNull {
                        col: idx,
                        name: conname,
                        no_inherit: c.is_no_inherit,
                        not_valid: false,
                        table_level: false,
                    });
                }
                C::ConstrDefault => {
                    if col.default.is_some() && serial.is_some() {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            format!(
                                "multiple default values specified for column \"{colname}\" of table \"{}\"",
                                t.name
                            ),
                        ));
                    }
                    let e = c.raw_expr.as_deref().unwrap();
                    match &mut self.defer_defaults {
                        Some(v) => v.push((colname.clone(), e.clone(), ty.clone())),
                        None => self.check_default(e, &ty, &colname)?,
                    }
                    // (AddRelationNewConstraints: a NULL constant isn't stored, as the
                    // column's default is NULL anyway; over a domain it is a CoerceToDomain.)
                    if col.domain.is_none() && self.is_null_default(e, &ty) {
                        saw_default = true;
                        continue;
                    }
                    col.default = Some(ast::deparse_expr(e)?);
                    saw_default = true;
                }
                C::ConstrIdentity => {
                    if t.of_type != 0 {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "identity columns are not supported on typed tables",
                        ));
                    }
                    if saw_identity {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                format!(
                                    "multiple identity specifications for column \"{colname}\" of table \"{}\"",
                                    t.name
                                ),
                            ),
                            c.location,
                        ));
                    }
                    saw_identity = true;
                    // Identity columns are NOT NULL: an explicit NULL conflicts.
                    if saw_null {
                        return Err(with_pos(conflicting_null(&colname, &t.name), c.location));
                    }
                    if !matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8) {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "identity column type must be smallint, integer, or bigint",
                        ));
                    }
                    col.identity = Some(if c.generated_when == "a" {
                        Identity::Always
                    } else {
                        Identity::ByDefault
                    });
                    let seq = self.owned_sequence(t, &colname, idx, &ty, &c.options)?;
                    col.seq = Some(seq.oid);
                    col.not_null = true;
                    pending.push(Pending::NotNull {
                        col: idx,
                        name: None,
                        no_inherit: false,
                        not_valid: false,
                        table_level: false,
                    });
                    seqs.push(seq);
                }
                C::ConstrGenerated => {
                    if t.of_type != 0 {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "generated columns are not supported on typed tables",
                        ));
                    }
                    if col.generated.is_some() {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                format!(
                                    "multiple generation clauses specified for column \"{colname}\" of table \"{}\"",
                                    t.name
                                ),
                            ),
                            c.location,
                        ));
                    }
                    let e = c.raw_expr.as_deref().unwrap();
                    col.generated = Some(ast::deparse_expr(e)?);
                    col.virtual_gen = c.generated_kind != "s";
                    gen_raw = Some(e);
                }
                C::ConstrCheck => pending.push(Pending::Check {
                    name: conname,
                    expr: ast::deparse_expr(c.raw_expr.as_deref().unwrap())?,
                    raw: c.raw_expr.clone(),
                    col: Some(idx),
                    not_valid: c.skip_validation || !c.is_enforced,
                    no_inherit: c.is_no_inherit,
                    enforced: c.is_enforced,
                }),
                C::ConstrPrimary | C::ConstrUnique => {
                    let include = self.column_list(t, &c.including, Some((&colname, idx)))?;
                    pending.push(Pending::Index {
                        primary: C::try_from(c.contype) == Ok(C::ConstrPrimary),
                        name: if c.indexname.is_empty() {
                            conname
                        } else {
                            Some(c.indexname.clone())
                        },
                        cols: vec![idx],
                        include,
                        nulls_not_distinct: c.nulls_not_distinct,
                        loc: c.location,
                        deferrable: c.deferrable,
                        initially_deferred: c.initdeferred,
                        options: super::objects::index_options("btree", &c.options)?,
                        space: c.indexspace.clone(),
                    });
                }
                C::ConstrForeign => {
                    pending.push(Pending::Fk(Box::new(self.fk_pending(c, vec![idx])?)))
                }
                C::ConstrExclusion => {
                    return Err(crate::error::unsupported("exclusion constraints"));
                }
                _ => {}
            }
        }
        // transformColumnDefinition: a serial's default comes after the column's own
        // constraints (with no position).
        if saw_identity && (saw_default || serial.is_some()) {
            let loc = if serial.is_some() { -1 } else { last_loc };
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "both default and identity specified for column \"{colname}\" of table \"{}\"",
                        t.name
                    ),
                ),
                loc,
            ));
        }
        if col.generated.is_some() && (saw_default || serial.is_some()) {
            let loc = if serial.is_some() { -1 } else { last_loc };
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "both default and generation expression specified for column \"{colname}\" of table \"{}\"",
                        t.name
                    ),
                ),
                loc,
            ));
        }
        if col.generated.is_some() && saw_identity {
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "both identity and generation expression specified for column \"{colname}\" of table \"{}\"",
                        t.name
                    ),
                ),
                last_loc,
            ));
        }
        if col.virtual_gen {
            // (A type with a domain inside, a range over one say, counts.)
            let in_domain = |ty: &DataType| {
                fn go(t: &DataType, cat: &crate::catalog::Catalog, depth: usize) -> bool {
                    depth < 16
                        && match t {
                            DataType::Array(e) => go(e, cat, depth + 1),
                            DataType::Range(rt) | DataType::Multirange(rt) => {
                                rt.subtype_domain.is_some() || go(&rt.subtype, cat, depth + 1)
                            }
                            DataType::Row(rt) if rt.oid != 0 => {
                                cat.composite_domains.get(&rt.oid).is_some_and(|m| !m.is_empty())
                                    || rt.fields.iter().any(|(_, f)| go(f, cat, depth + 1))
                            }
                            _ => false,
                        }
                }
                go(ty, &self.cat, 0)
            };
            if col.domain.is_some() || in_domain(&ty) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("virtual generated column \"{colname}\" cannot have a domain type"),
                ));
            }
            if ty.oid() >= crate::catalog::FIRST_USER_OID {
                return Err(Error::new(code::FEATURE_NOT_SUPPORTED, format!("virtual generated column \"{colname}\" cannot have a user-defined type"))
                    .detail("Virtual generated columns that make use of user-defined types are not yet supported."));
            }
        }
        t.columns.push(col);
        // Generated expressions may reference any column (but not other generated ones):
        // CREATE TABLE checks them once all are there.
        if let Some(g) = t.columns[idx].generated.clone() {
            match &mut self.defer_generated {
                Some(v) => v.push((t.columns[idx].name.clone(), gen_raw.cloned())),
                // (ALTER TABLE ADD COLUMN cooks it without the statement's text.)
                None => self
                    .check_generated(t, idx, &g, gen_raw)
                    .map_err(|e| Error { position: None, ..e })?,
            }
        }
        Ok(())
    }

    /// Whether default `e` of a column of type `ty` cooks to a bare NULL constant (`NULL`,
    /// `NULL::<the column's type>`), which Postgres doesn't store (AddRelationNewConstraints).
    pub(super) fn is_null_default(&self, e: &pb::Node, ty: &DataType) -> bool {
        match inner(e) {
            Some(NodeEnum::AConst(c)) => c.isnull,
            Some(NodeEnum::TypeCast(tc)) => {
                tc.arg.as_deref().is_some_and(|a| matches!(inner(a), Some(NodeEnum::AConst(c)) if c.isnull))
                    && tc.type_name.as_ref().is_some_and(|tn| {
                        matches!(
                            crate::sql::binder::resolve_type_name(&self.cat, self.search_path(), tn),
                            Ok((t, None)) if t.same_base(ty)
                        )
                    })
            }
            _ => false,
        }
    }

    pub(super) fn check_default(&self, e: &pb::Node, ty: &DataType, colname: &str) -> Result<()> {
        // Over the catalog as the statement has it so far: names (regclass input) of a
        // relation being created resolve.
        let cat = Arc::new(self.cat.clone());
        let nctx = self.ctx.child_over(cat.clone());
        let mut b = crate::sql::binder::Binder::new(&nctx, self.params.clone());
        b.cat = cat;
        // cookDefault doesn't simplify the expression: what fails folding fails when used.
        b.unplanned = true;
        let ex = b.bind_scalar(e, Clause::Default).map_err(|err| {
            // (A relation name's input — 'x'::regclass — fails as itself.)
            if err.code == code::UNDEFINED_COLUMN
                || err.code == code::UNDEFINED_TABLE && !err.message.starts_with("relation \"")
            {
                Error {
                    position: err.position,
                    ..Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot use column reference in DEFAULT expression",
                    )
                }
            } else {
                err
            }
        })?;
        b.coerce_assign(ex, ty, colname)?;
        Ok(())
    }

    /// Check a generation expression: `raw` is the expression as written in the statement
    /// (errors point into it); without it errors carry no position, as for ALTER COLUMN SET
    /// EXPRESSION in Postgres.
    fn check_generated(
        &self,
        t: &Table,
        idx: usize,
        sql: &str,
        raw: Option<&pb::Node>,
    ) -> Result<()> {
        let parsed;
        let n = match raw {
            Some(n) => n,
            None => {
                parsed = ast::parse_expr(sql)?;
                &parsed
            }
        };
        // System columns (but tableoid) and whole-row references can't be used
        // (cookDefault's checks on the transformed expression).
        if let Some(ne) = n.node.as_ref() {
            for (node, _, _, _) in ne.nodes() {
                let pg_query::NodeRef::ColumnRef(c) = node else {
                    continue;
                };
                let last = c.fields.last().and_then(inner);
                let name = match last {
                    Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                };
                let star = matches!(last, Some(NodeEnum::AStar(_)));
                let at = if raw.is_some() { c.location } else { -1 };
                let whole = star || (c.fields.len() == 1 && name.is_some_and(|n| n == t.name && t.column_index(n).is_none()));
                if whole {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_OBJECT_DEFINITION,
                            "cannot use whole-row variable in column generation expression",
                        )
                        .detail("This would cause the generated column to depend on its own value."),
                        at,
                    ));
                }
                if let Some(n) = name
                    && matches!(n, "ctid" | "xmin" | "cmin" | "xmax" | "cmax")
                    && t.column_index(n).is_none()
                {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_COLUMN_REFERENCE,
                            format!("cannot use system column \"{n}\" in column generation expression"),
                        ),
                        at,
                    ));
                }
            }
        }
        // Cooked once the relation exists: names (regclass input) resolve through it.
        let cat = Arc::new(self.cat.clone());
        let nctx = self.ctx.child_over(cat.clone());
        let mut b = crate::sql::binder::Binder::new(&nctx, self.params.clone());
        b.cat = cat;
        let ta = Arc::new(t.clone());
        let e = b.bind_over_table(&ta, n, Clause::Generated).map_err(|e| {
            if raw.is_none() {
                Error {
                    position: None,
                    ..e
                }
            } else {
                e
            }
        })?;
        for c in e.columns() {
            if t.columns[c].generated.is_some() {
                return Err(Error::new(
                    code::INVALID_OBJECT_DEFINITION,
                    "cannot use generated column \"".to_string()
                        + &t.columns[c].name
                        + "\" in column generation expression",
                )
                .detail("A generated column cannot reference another generated column."));
            }
        }
        if !e.is_immutable() {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                "generation expression is not immutable",
            ));
        }
        let col = &t.columns[idx];
        // Virtual columns are computed with the reader's privileges: no user-defined code.
        if col.virtual_gen
            && let Some(loc) = b.user_fn_loc
        {
            let err = Error::new(code::FEATURE_NOT_SUPPORTED, "generation expression uses user-defined function")
                .detail("Virtual generated columns that make use of user-defined functions are not yet supported.");
            return Err(if raw.is_some() {
                ast::with_pos(err, loc)
            } else {
                err
            });
        }
        // (A column of a domain type reads as the domain: a user-defined type.)
        let domain_col = e.columns().iter().any(|&i| t.columns.get(i).is_some_and(|c| c.domain.is_some()));
        if col.virtual_gen && (uses_user_type(&e) || domain_col) {
            let err = Error::new(code::FEATURE_NOT_SUPPORTED, "generation expression uses user-defined type")
                .detail("Virtual generated columns that make use of user-defined types are not yet supported.");
            return Err(if raw.is_some() {
                ast::with_pos(err, ast::location(n))
            } else {
                err
            });
        }
        b.coerce_assign(e, &col.ty, &col.name)?;
        Ok(())
    }

    /// ChooseRelationName for a constraint's index: a name no relation and no constraint in
    /// the schema has (the table's own constraints included).
    fn choose_constraint_index_name(&self, t: &Table, base: &crate::catalog::ObjectName) -> String {
        let used = |n: &str| {
            self.cat.name_taken(t.schema, n)
                || t.constraints.iter().any(|c| c.name == n)
                // (The tables map's index of the schema's constraint names: `relindex`.)
                || self.cat.tables.ix().con_names.contains_key(&(t.schema, n.to_string()))
        };
        debug_assert_eq!(
            base.choose(used),
            base.choose(|n: &str| {
                self.cat.name_taken(t.schema, n)
                    || t.constraints.iter().any(|c| c.name == n)
                    || self
                        .cat
                        .tables
                        .values()
                        .any(|x| x.schema == t.schema && x.constraints.iter().any(|c| c.name == n))
            }),
            "constraint name index"
        );
        base.choose(used)
    }

    /// The sequence behind a serial or identity column.
    fn owned_sequence(
        &mut self,
        t: &Table,
        col: &str,
        idx: usize,
        ty: &DataType,
        opts: &[pb::Node],
    ) -> Result<Sequence> {
        let base = Catalog::object_name(&t.name, &[col], "seq");
        let mut name = self.cat.choose_name(t.schema, &base);
        let oid = match self.seq_oids.pop_front() {
            Some(o) => o,
            None => self.cat.alloc_oid(),
        };
        let mut sq = Sequence {
            oid,
            schema: t.schema,
            name: String::new(),
            owner: t.owner,
            ty: ty.clone(),
            start: 1,
            increment: 1,
            min: 1,
            max: type_max(ty),
            cache: 1,
            cycle: false,
            owned_by: Some((t.oid, idx)),
            unlogged: None,
        };
        let mut logged: Option<&pb::DefElem> = None;
        for d in def_elems(opts) {
            if d.defname == "sequence_name"
                && let Some(NodeEnum::List(l)) = d.arg.as_deref().and_then(inner)
            {
                name = names(&l.items).last().cloned().unwrap_or(name);
            }
            // generateSerialExtraStmts: LOGGED / UNLOGGED (pg_dump's, for a sequence whose
            // persistence isn't its table's) is the sequence's own persistence.
            if d.defname == "logged" || d.defname == "unlogged" {
                if logged.is_some() {
                    return Err(crate::sql::binder::ast::with_pos(
                        Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                        d.location,
                    ));
                }
                logged = Some(d);
            }
        }
        if let Some(d) = logged {
            let unlogged = d.defname == "unlogged";
            sq.unlogged = (unlogged != t.unlogged).then_some(unlogged);
        }
        let opts: Vec<pb::Node> = opts
            .iter()
            .filter(|o| {
                !matches!(inner(o), Some(NodeEnum::DefElem(d)) if d.defname == "logged" || d.defname == "unlogged")
            })
            .cloned()
            .collect();
        self.apply_seq_options(&mut sq, &opts, true)?;
        // generateSerialExtraStmts's DEBUG1 (CREATE TABLE's, while the table isn't in the
        // catalog yet; else ALTER TABLE's).
        let stmt = if self.cat.tables.contains_key(&t.oid) {
            "ALTER TABLE"
        } else {
            "CREATE TABLE"
        };
        let mut e = crate::session::notice(format!(
            "{stmt} will create implicit sequence \"{name}\" for serial column \"{}.{col}\"",
            t.name
        ));
        e.severity = crate::error::Severity::Debug;
        self.ctx.notice(e);
        sq.name = name;
        Ok(sq)
    }

    /// Column names → positions.
    pub(super) fn column_list(
        &self,
        t: &Table,
        list: &[pb::Node],
        extra: Option<(&str, usize)>,
    ) -> Result<Vec<usize>> {
        let mut out = Vec::new();
        for n in names(list) {
            let i = match (t.column_index(&n), extra) {
                (Some(i), _) => i,
                (None, Some((e, i))) if e == n => i,
                _ => {
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{n}\" named in key does not exist"),
                    ));
                }
            };
            if out.contains(&i) {
                return Err(Error::new(
                    code::DUPLICATE_COLUMN,
                    format!("column \"{n}\" appears twice in primary key constraint"),
                ));
            }
            out.push(i);
        }
        Ok(out)
    }

    fn fk_pending(&self, c: &pb::Constraint, cols: Vec<usize>) -> Result<PendingFk> {
        Ok(PendingFk {
            name: if c.conname.is_empty() {
                None
            } else {
                Some(c.conname.clone())
            },
            cols,
            ref_rv: c
                .pktable
                .clone()
                .ok_or_else(|| crate::error::internal("FK without table"))?,
            ref_cols: names(&c.pk_attrs),
            on_delete: fk_action(&c.fk_del_action),
            on_update: fk_action(&c.fk_upd_action),
            match_full: c.fk_matchtype == "f",
            deferrable: c.deferrable,
            initially_deferred: c.initdeferred,
            not_valid: c.skip_validation || !c.is_enforced,
            enforced: c.is_enforced,
            period: c.fk_with_period,
            pk_period: c.pk_with_period,
            del_set_cols: names(&c.fk_del_set_cols),
        })
    }

    /// PRIMARY KEY / UNIQUE (cols, period WITHOUT OVERLAPS) (transformIndexConstraint): a
    /// GiST exclusion constraint, `=` on each column and `&&` on the range or multirange
    /// last one.
    fn without_overlaps_pending(
        &self,
        t: &Table,
        c: &pb::Constraint,
        name: Option<String>,
    ) -> Result<PendingExclusion> {
        let primary = c.contype == pb::ConstrType::ConstrPrimary as i32;
        let keys = names(&c.keys);
        let mut cols: Vec<usize> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            let Some(ci) = t.column_index(k) else {
                return Err(with_pos(
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{k}\" named in key does not exist"),
                    ),
                    c.location,
                ));
            };
            if cols.contains(&ci) {
                let what = if primary { "primary key" } else { "unique" };
                return Err(with_pos(
                    Error::new(
                        code::DUPLICATE_COLUMN,
                        format!("column \"{k}\" appears twice in {what} constraint"),
                    ),
                    c.location,
                ));
            }
            if i + 1 == keys.len()
                && !matches!(t.columns[ci].ty, DataType::Range(_) | DataType::Multirange(_))
            {
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "column \"{k}\" in WITHOUT OVERLAPS is not a range or multirange type"
                        ),
                    ),
                    c.location,
                ));
            }
            cols.push(ci);
        }
        if keys.len() < 2 {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "constraint using WITHOUT OVERLAPS needs at least two columns",
            ));
        }
        let elems = keys
            .iter()
            .map(|k| pb::Node {
                node: Some(NodeEnum::IndexElem(Box::new(pb::IndexElem {
                    name: k.clone(),
                    ..Default::default()
                }))),
            })
            .collect();
        let mut ops = vec!["=".to_string(); keys.len() - 1];
        ops.push("&&".into());
        Ok(PendingExclusion {
            name,
            method: "gist".into(),
            elems,
            ops,
            including: c.including.clone(),
            predicate: None,
            options: c.options.clone(),
            deferrable: c.deferrable,
            initially_deferred: c.initdeferred,
            period: Some(if primary { 'p' } else { 'u' }),
        })
    }

    /// A table-level constraint (or one from ALTER TABLE ADD CONSTRAINT).
    pub(super) fn table_constraint(
        &self,
        t: &Table,
        c: &pb::Constraint,
        _col: Option<usize>,
    ) -> Result<Option<Pending>> {
        use pb::ConstrType as C;
        let conname = if c.conname.is_empty() {
            None
        } else {
            Some(c.conname.clone())
        };
        Ok(Some(match C::try_from(c.contype).unwrap_or(C::Undefined) {
            C::ConstrCheck => Pending::Check {
                name: conname,
                expr: ast::deparse_expr(c.raw_expr.as_deref().unwrap())?,
                raw: c.raw_expr.clone(),
                col: None,
                not_valid: c.skip_validation || !c.is_enforced,
                no_inherit: c.is_no_inherit,
                enforced: c.is_enforced,
            },
            C::ConstrPrimary | C::ConstrUnique if c.without_overlaps => {
                Pending::Exclusion(Box::new(self.without_overlaps_pending(t, c, conname)?))
            }
            C::ConstrPrimary | C::ConstrUnique => {
                if !c.indexname.is_empty() && c.keys.is_empty() {
                    return Err(with_pos(
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "cannot use an existing index in CREATE TABLE",
                        ),
                        c.location,
                    ));
                }
                Pending::Index {
                    primary: C::try_from(c.contype) == Ok(C::ConstrPrimary),
                    name: conname,
                    cols: self.column_list(t, &c.keys, None)?,
                    include: self.column_list(t, &c.including, None)?,
                    nulls_not_distinct: c.nulls_not_distinct,
                    loc: c.location,
                    deferrable: c.deferrable,
                    initially_deferred: c.initdeferred,
                    options: super::objects::index_options("btree", &c.options)?,
                    space: c.indexspace.clone(),
                }
            }
            C::ConstrExclusion => {
                let mut elems = Vec::new();
                let mut ops = Vec::new();
                for x in &c.exclusions {
                    let Some(NodeEnum::List(pair)) = inner(x) else {
                        continue;
                    };
                    let (Some(elem), Some(op)) = (pair.items.first(), pair.items.get(1)) else {
                        continue;
                    };
                    let Some(NodeEnum::List(op)) = inner(op) else {
                        continue;
                    };
                    elems.push(elem.clone());
                    ops.push(names(&op.items).last().cloned().unwrap_or_default());
                }
                Pending::Exclusion(Box::new(PendingExclusion {
                    name: conname,
                    method: if c.access_method.is_empty() {
                        "btree".into()
                    } else {
                        c.access_method.clone()
                    },
                    elems,
                    ops,
                    including: c.including.clone(),
                    predicate: c.where_clause.as_deref().cloned(),
                    options: c.options.clone(),
                    deferrable: c.deferrable,
                    initially_deferred: c.initdeferred,
                    period: None,
                }))
            }
            C::ConstrForeign => {
                let cols = names(&c.fk_attrs)
                    .iter()
                    .map(|n| {
                        t.column_index(n).ok_or_else(|| fk_column_missing(n))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Pending::Fk(Box::new(self.fk_pending(c, cols)?))
            }
            C::ConstrNotnull => {
                let n = names(&c.keys);
                let col = n.first().and_then(|n| t.column_index(n)).ok_or_else(|| {
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        "column of NOT NULL constraint does not exist",
                    )
                })?;
                Pending::NotNull {
                    col,
                    name: conname,
                    no_inherit: c.is_no_inherit,
                    not_valid: c.skip_validation,
                    table_level: true,
                }
            }
            _ => return Ok(None),
        }))
    }

    /// LIKE other_table [INCLUDING ...]
    fn like_clause(
        &mut self,
        t: &mut Table,
        l: &pb::TableLikeClause,
        pending: &mut Vec<Pending>,
        seqs: &mut Vec<Sequence>,
    ) -> Result<()> {
        let rv = l.relation.as_ref().unwrap();
        // (Errors opening the relation point at it.)
        let pos = |e: Error| crate::sql::binder::ast::with_pos(e, rv.location);
        let src = match self.lookup(rv).map_err(pos)? {
            Some(Relation::Table(s)) => {
                // transformTableLikeClause: SELECT on the relation.
                self.require_rel_priv(s.oid, crate::catalog::acl::SELECT, false)?;
                s
            }
            Some(Relation::View(v)) => {
                self.require_rel_priv(v.oid, crate::catalog::acl::SELECT, false)?;
                for (n, ty) in &v.columns {
                    t.columns.push(Column::new(n.clone(), ty.clone()));
                }
                return Ok(());
            }
            Some(r @ (Relation::Sequence(_) | Relation::Index(_))) => {
                let what = if matches!(r, Relation::Sequence(_)) {
                    "sequences"
                } else {
                    "indexes"
                };
                return Err(pos(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("relation \"{}\" is invalid in LIKE clause", rv.relname),
                )
                .detail(format!("This operation is not supported for {what}."))));
            }
            _ => {
                // A composite type's attributes.
                let schema = Self::rv_schema(rv);
                let comp = self.cat.composites.values().find(|c| {
                    c.name == rv.relname
                        && schema.is_none_or(|s| self.cat.schema_name(c.schema) == s)
                        && (schema.is_some()
                            || self.cat.effective_path(self.search_path()).contains(&c.schema))
                });
                if let Some(c) = comp.cloned() {
                    for (n, ty) in &c.fields {
                        t.columns.push(Column::new(n.clone(), ty.clone()));
                    }
                    return Ok(());
                }
                return Err(pos(crate::catalog::no_such_relation(schema, &rv.relname)));
            }
        };
        const DEFAULTS: u32 = 1 << 3;
        const GENERATED: u32 = 1 << 4;
        const IDENTITY: u32 = 1 << 5;
        const INDEXES: u32 = 1 << 6;
        const STATISTICS: u32 = 1 << 7;
        const CONSTRAINTS: u32 = 1 << 2;
        const COMPRESSION: u32 = 1 << 1;
        const COMMENTS: u32 = 1 << 0;
        const STORAGE: u32 = 1 << 8;
        const ALL: u32 = 0x7FFF_FFFF;
        let o = if l.options == ALL {
            u32::MAX
        } else {
            l.options
        };
        // A foreign table copies no indexes, identity, compression or storage.
        let o = if self.creating_foreign.is_some() || t.is_foreign() {
            o & !(INDEXES | IDENTITY | COMPRESSION | STORAGE)
        } else {
            o
        };
        let mut defaults = Vec::new();
        for (i, c) in src.live_columns() {
            if let Some(ei) = t.column_index(&c.name) {
                // A column inherited only merges with LIKE's, as with a local definition.
                let ex = &t.columns[ei];
                if ex.islocal || ex.inhcount == 0 {
                    return Err(Error::new(
                        code::DUPLICATE_COLUMN,
                        format!("column \"{}\" specified more than once", c.name),
                    ));
                }
                if ex.ty != c.ty {
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        format!("column \"{}\" has a type conflict", c.name),
                    )
                    .detail(format!("{} versus {}", ex.ty.display(), c.ty.display())));
                }
                self.notice(format!(
                    "merging column \"{}\" with inherited definition",
                    c.name
                ));
                let ex = &mut t.columns[ei];
                ex.islocal = true;
                if o & STORAGE != 0 {
                    let storage = |x: &Column| {
                        x.storage
                            .unwrap_or_else(|| crate::catalog::system::typstorage(&x.ty))
                    };
                    if storage(ex) != storage(c) {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            format!("column \"{}\" has a storage parameter conflict", c.name),
                        )
                        .detail(format!(
                            "{} versus {}",
                            storage_name(storage(ex)),
                            storage_name(storage(c))
                        )));
                    }
                    ex.storage = c.storage;
                }
                if o & COMPRESSION != 0 {
                    ex.compression = c.compression;
                }
                if c.not_null {
                    let src_nn = src.constraints.iter().find(
                        |k| matches!(k.kind, ConstraintKind::NotNull { column } if column == i),
                    );
                    pending.push(Pending::NotNull {
                        col: ei,
                        name: src_nn.map(|k| k.name.clone()),
                        no_inherit: src_nn.is_some_and(|k| k.no_inherit),
                        not_valid: false,
                        table_level: false,
                    });
                }
                continue;
            }
            let mut nc = Column::new(c.name.clone(), c.ty.clone());
            nc.not_null = c.not_null;
            nc.domain = c.domain;
            nc.collation = c.collation.clone();
            if o & DEFAULTS != 0 && c.seq.is_none() {
                nc.default = c.default.clone();
            }
            if o & GENERATED != 0 {
                nc.generated = c.generated.clone();
                nc.virtual_gen = c.virtual_gen;
            }
            if o & IDENTITY != 0 && c.identity.is_some() {
                nc.identity = c.identity.clone();
                // A sequence of its own, with the original's parameters.
                let mut sq = self.owned_sequence(t, &c.name, t.columns.len(), &c.ty, &[])?;
                if let Some(old) = c.seq.and_then(|o| self.cat.sequences.get(&o)) {
                    sq.start = old.start;
                    sq.increment = old.increment;
                    sq.min = old.min;
                    sq.max = old.max;
                    sq.cache = old.cache;
                    sq.cycle = old.cycle;
                }
                nc.seq = Some(sq.oid);
                seqs.push(sq);
            }
            if o & COMPRESSION != 0 {
                nc.compression = c.compression;
            }
            if o & STORAGE != 0 {
                nc.storage = c.storage;
            }

            let idx = t.columns.len();
            if nc.default.is_some() || nc.generated.is_some() {
                // Its attrdef comes after the TOAST table.
                nc.def_oid = LIKE_DEF;
                defaults.push(idx);
            }
            if nc.not_null {
                // The NOT NULL constraint keeps its name.
                let src_nn = src
                    .constraints
                    .iter()
                    .find(|k| matches!(k.kind, ConstraintKind::NotNull { column } if column == i));
                let (name, no_inherit) = (
                    src_nn.map(|k| k.name.clone()),
                    src_nn.is_some_and(|k| k.no_inherit),
                );
                pending.push(Pending::NotNull {
                    col: idx,
                    name,
                    no_inherit,
                    not_valid: false,
                    table_level: false,
                });
            }
            t.columns.push(nc);
        }
        let mut checks = Vec::new();
        if o & CONSTRAINTS != 0 {
            // The relcache's order: by name.
            let mut src_checks: Vec<_> = src
                .constraints
                .iter()
                .filter(|c| matches!(c.kind, ConstraintKind::Check { .. }))
                .collect();
            src_checks.sort_by(|a, b| a.name.cmp(&b.name));
            for c in src_checks {
                if let ConstraintKind::Check { expr, no_inherit } = &c.kind {
                    // (expandTableLikeClause makes them valid ones.)
                    let (not_valid, no_inherit, enforced) = (false, *no_inherit, c.enforced);
                    checks.push(Pending::Check {
                        name: Some(c.name.clone()),
                        expr: expr.clone(),
                        raw: None,
                        col: None,
                        not_valid,
                        no_inherit,
                        enforced,
                    });
                }
            }
        }
        if o & (INDEXES | COMMENTS | STATISTICS) != 0 || !defaults.is_empty() || !checks.is_empty()
        {
            let statistics = o & STATISTICS != 0;
            pending.push(Pending::Like {
                src: src.clone(),
                defaults,
                checks,
                indexes: o & INDEXES != 0,
                comments: o & COMMENTS != 0,
                constraints: o & CONSTRAINTS != 0,
                statistics,
            });
        }
        Ok(())
    }

    /// Turn pending constraints into catalog entries. `existing`: the table may already have
    /// rows (ALTER TABLE), so new constraints are checked against them.
    pub(super) fn apply_pending(
        &mut self,
        mut t: Table,
        pending: Vec<Pending>,
        existing: bool,
    ) -> Result<Table> {
        let mut rewrite_for_pk = false;
        for p in pending {
            match p {
                Pending::NotNull {
                    col,
                    name,
                    no_inherit,
                    not_valid,
                    ..
                } => {
                    if no_inherit && t.partkey.is_some() {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            "not-null constraints on partitioned tables cannot be NO INHERIT",
                        ));
                    }
                    t.columns[col].not_null = true;
                    let tname = t.name.clone();
                    let colname = t.columns[col].name.clone();
                    if let Some(c) = t.constraints.iter_mut().find(
                        |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == col),
                    ) {
                        // AdjustNotNullInheritance: an existing one is kept, if it agrees.
                        if existing && c.oid != 0 {
                            if c.no_inherit != no_inherit {
                                return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \"{tname}\"", c.name))
                                    .hint("You might need to make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT."));
                            }
                            // SET NOT NULL validates an invalid one.
                            if !not_valid && !c.validated && name.is_none() {
                                c.validated = true;
                                c.islocal = true;
                                self.check_not_null(&t, col)?;
                                continue;
                            }
                            if !not_valid && !c.validated {
                                return Err(Error::new(
                                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                    format!("incompatible NOT VALID constraint \"{}\" on relation \"{tname}\"", c.name),
                                )
                                .hint("You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT."));
                            }
                            if let Some(n) = &name
                                && *n != c.name
                            {
                                return Err(Error::new(
                                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                    format!("cannot create not-null constraint \"{n}\" on column \"{colname}\" of table \"{tname}\""),
                                )
                                .detail(format!(
                                    "A not-null constraint named \"{}\" already exists for this column.",
                                    c.name
                                )));
                            }
                            c.islocal = true;
                            continue;
                        }
                        if c.no_inherit && !no_inherit {
                            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \"{tname}\"", c.name))
                                .hint("You might need to make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT."));
                        }
                        // CREATE TABLE: an inherited one merged into the table's own is made
                        // with those, under the local name (AddRelationNotNullConstraints).
                        if c.oid == 0 {
                            if no_inherit {
                                return Err(Error::new(
                                    code::DATATYPE_MISMATCH,
                                    format!(
                                        "cannot define not-null constraint with NO INHERIT on column \"{}\"",
                                        t.columns[col].name
                                    ),
                                )
                                .detail("The column has an inherited not-null constraint."));
                            }
                            c.oid = self.cat.alloc_oid();
                            if !c.islocal {
                                c.name = String::new();
                            }
                        }
                        c.islocal = true;
                        if c.name.is_empty() {
                            let n = match name {
                                Some(n) => n,
                                None => {
                                    let colname = t.columns[col].name.clone();
                                    self.constraint_name(&t, &[colname.as_str()], "not_null")
                                }
                            };
                            if let Some(c) = t.constraints.iter_mut().find(|c| {
                                matches!(c.kind, ConstraintKind::NotNull { column } if column == col)
                            }) {
                                c.name = n;
                            }
                        }
                    } else {
                        if let Some(n) = &name
                            && t.constraints.iter().any(|c| &c.name == n)
                        {
                            return Err(Error::new(
                                code::DUPLICATE_OBJECT,
                                format!("constraint \"{n}\" for relation \"{tname}\" already exists"),
                            ));
                        }
                        let name = match name {
                            Some(n) => n,
                            None => self.constraint_name(
                                &t,
                                &[t.columns[col].name.as_str()],
                                "not_null",
                            ),
                        };
                        let oid = self.cat.alloc_oid();
                        let mut c = Constraint::new(
                            oid,
                            name,
                            ConstraintKind::NotNull { column: col },
                            true,
                        );
                        c.no_inherit = no_inherit;
                        // NOT VALID: existing rows aren't checked (new ones are).
                        c.validated = !(existing && not_valid);
                        t.constraints.push(c);
                        if existing && not_valid {
                            continue;
                        }
                    }
                    if existing {
                        self.check_not_null(&t, col)?;
                    }
                }
                Pending::Check {
                    name,
                    expr,
                    raw,
                    col: _,
                    not_valid,
                    no_inherit,
                    enforced,
                } => {
                    if no_inherit && t.partkey.is_some() {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            format!(
                                "cannot add NO INHERIT constraint to partitioned table \"{}\"",
                                t.name
                            ),
                        ));
                    }
                    // A local copy of an inherited check constraint merges into it
                    // (MergeWithExistingConstraint); CREATE TABLE ignores NOT VALID.
                    let initially_valid = enforced && (!not_valid || !existing);
                    let partition = t.partbound.is_some();
                    let tname = t.name.clone();
                    let tc = Arc::new(t.clone());
                    let pos = t.constraints.iter().position(|c| {
                        !c.islocal && name.as_deref() == Some(c.name.as_str()) && matches!(&c.kind, ConstraintKind::Check { expr: e, .. } if self.same_check(&tc, e, &expr))
                    });
                    if let Some(c) = pos.map(|i| &mut t.constraints[i]) {
                        let conflict = |what: &str| {
                            Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!(
                                    "constraint \"{}\" conflicts with {what} constraint on relation \"{tname}\"",
                                    c.name
                                ),
                            )
                        };
                        if matches!(c.kind, ConstraintKind::Check { no_inherit: true, .. }) {
                            return Err(conflict("non-inherited"));
                        }
                        if c.inhcount > 0 && no_inherit {
                            return Err(conflict("inherited"));
                        }
                        if initially_valid && c.enforced && !c.validated {
                            return Err(conflict("NOT VALID"));
                        }
                        if !enforced && c.enforced {
                            return Err(conflict("NOT ENFORCED"));
                        }
                        self.notice(format!("merging constraint \"{}\" with inherited definition", c.name));
                        // A partition's constraint is inherited once and never local.
                        c.islocal = !partition;
                        if no_inherit
                            && let ConstraintKind::Check { no_inherit: n, .. } = &mut c.kind
                        {
                            *n = true;
                        }
                        if enforced && !c.enforced {
                            c.enforced = true;
                            c.validated = true;
                        }
                        continue;
                    }
                    let name = match name {
                        Some(n) => {
                            if t.constraints.iter().any(|c| c.name == n) {
                                return Err(Error::new(
                                    code::DUPLICATE_OBJECT,
                                    format!(
                                        "constraint \"{n}\" for relation \"{}\" already exists",
                                        t.name
                                    ),
                                ));
                            }
                            n
                        }
                        None => {
                            let cols: Vec<String> = sole_column_ref(&t, &expr)
                                .into_iter()
                                .map(str::to_string)
                                .collect();
                            let refs: Vec<&str> = cols.iter().map(String::as_str).collect();
                            self.constraint_name(&t, &refs, "check")
                        }
                    };
                    // Only CREATE TABLE's errors point into the statement (ALTER TABLE's
                    // carry no position).
                    let raw = raw.filter(|_| !existing);
                    let bound = self.bind_check_raw(&t, &expr, raw.as_deref())?;
                    if existing && !not_valid && enforced {
                        self.validate_check(&t, &name, &bound)?;
                    }
                    let oid = self.cat.alloc_oid();
                    let mut c = Constraint::new(
                        oid,
                        name,
                        ConstraintKind::Check { expr, no_inherit },
                        initially_valid,
                    );
                    c.enforced = enforced;
                    c.row_types = crate::sql::expr::named_row_types(&bound);
                    t.constraints.push(c);
                }
                Pending::Exclusion(x) => self.in_index_build(|d| d.exclusion_constraint(&mut t, *x, existing))?,
                Pending::Index {
                    primary,
                    name,
                    cols,
                    include,
                    nulls_not_distinct,
                    loc,
                    deferrable,
                    initially_deferred,
                    options,
                    space,
                } => {
                    if primary
                        && (t.pkey.is_some()
                            || t.constraints.iter().any(|c| c.period == Some('p')))
                    {
                        // (CREATE TABLE points at the clause; ALTER TABLE doesn't, nor
                        // a key a partition got from its parent: index_check_primary_key.)
                        let cloned = t.constraints.iter().any(|c| {
                            matches!(c.kind, ConstraintKind::PrimaryKey { .. }) && c.parent != 0
                        });
                        let existing = existing || cloned;
                        return Err(with_pos(
                            Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "multiple primary keys for table \"{}\" are not allowed",
                                    t.name
                                ),
                            ),
                            if existing { -1 } else { loc },
                        ));
                    }
                    if cols
                        .iter()
                        .chain(&include)
                        .any(|&c| t.columns[c].virtual_gen)
                    {
                        let what = if primary {
                            "primary keys"
                        } else {
                            "unique constraints"
                        };
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("{what} on virtual generated columns are not supported"),
                        ));
                    }
                    self.check_unique_partkey(
                        &t,
                        &cols,
                        if primary { "PRIMARY KEY" } else { "UNIQUE" },
                    )?;
                    // ALTER TABLE ONLY: the partitions' columns must be NOT NULL already
                    // (nothing is added to them).
                    if primary && existing && self.only && t.partkey.is_some() {
                        for p in self.cat.inheritors(t.oid).into_iter().skip(1) {
                            let Some(part) = self.cat.tables.get(&p).cloned() else {
                                continue;
                            };
                            for &c in &cols {
                                let name = &t.columns[c].name;
                                if part
                                    .column_index(name)
                                    .is_some_and(|rc| !part.columns[rc].not_null)
                                {
                                    return Err(Error::new(
                                        code::INTERNAL_ERROR,
                                        format!(
                                            "column \"{name}\" of table \"{}\" is not marked NOT NULL",
                                            part.name
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                    // A primary key's columns need inheritable, valid not-null constraints.
                    if primary && existing {
                        // (A partitioned table's partitions too: they get the not-nulls.)
                        let mut rels: Vec<Arc<Table>> = vec![Arc::new(t.clone())];
                        if t.partkey.is_some() {
                            rels.extend(
                                self.cat
                                    .inheritors(t.oid)
                                    .into_iter()
                                    .skip(1)
                                    .filter_map(|o| self.cat.tables.get(&o).cloned()),
                            );
                        }
                        for (&c, rel) in cols
                            .iter()
                            .flat_map(|c| rels.iter().map(move |r| (c, r)))
                        {
                            let Some(rc) = rel.column_index(&t.columns[c].name) else {
                                continue;
                            };
                            let Some(k) = rel.constraints.iter().find(|k| {
                                matches!(k.kind, ConstraintKind::NotNull { column } if column == rc)
                            }) else {
                                continue;
                            };
                            let (what, hint) = if k.no_inherit {
                                ("NO INHERIT", "You might need to make the existing constraint inheritable using ALTER TABLE ... ALTER CONSTRAINT ... INHERIT.")
                            } else if !k.validated {
                                ("NOT VALID", "You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT.")
                            } else {
                                continue;
                            };
                            return Err(Error::new(
                                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                format!("cannot create primary key on column \"{}\"", t.columns[c].name),
                            )
                            .detail(format!(
                                "The constraint \"{}\" on column \"{}\" of table \"{}\", marked {what}, is incompatible with a primary key.",
                                k.name, t.columns[c].name, rel.name
                            ))
                            .hint(hint));
                        }
                    }
                    // (ChooseIndexColumnNames names INCLUDE columns too.)
                    let colnames: Vec<&str> = cols
                        .iter()
                        .chain(&include)
                        .map(|c| t.columns[*c].name.as_str())
                        .collect();
                    let name = match name {
                        Some(n) => {
                            if self.cat.name_taken(t.schema, &n) {
                                return Err(Error::new(
                                    code::DUPLICATE_TABLE,
                                    format!("relation \"{n}\" already exists"),
                                ));
                            }
                            n
                        }
                        None if primary => self.choose_constraint_index_name(
                            &t,
                            &Catalog::object_name(&t.name, &[], "pkey"),
                        ),
                        None => self.choose_constraint_index_name(
                            &t,
                            &Catalog::object_name(&t.name, &colnames, "key"),
                        ),
                    };
                    // A primary key's columns get their not-nulls first.
                    if primary {
                        for c in &cols {
                            if existing {
                                self.check_not_null(&t, *c)?;
                            }
                            t.columns[*c].not_null = true;
                            if !t.constraints.iter().any(|k| matches!(k.kind, ConstraintKind::NotNull { column } if column == *c)) {
                                let nn = self.constraint_name(&t, &[t.columns[*c].name.as_str()], "not_null");
                                let o = self.cat.alloc_oid();
                                t.constraints.push(Constraint::new(o, nn, ConstraintKind::NotNull { column: *c }, true));
                            }
                        }
                    }
                    // Same columns as an existing unique constraint: Postgres still makes a new one.
                    let spc =
                        self.choose_tablespace(&space, t.partkey.is_some(), t.temporary, None)?;
                    let ix_oid = self.cat.alloc_oid();
                    self.cat.set_rel_tablespace(ix_oid, spc);
                    // A primary key is an index of its own: rows are stored under a
                    // hidden row id, in heap order (a deferrable key's values may clash
                    // until it is checked, too). Only `roll_pk_clustered` (or the legacy
                    // layout) keys rows by it.
                    let keyed = primary && pk_keys_rows(&t, deferrable)?;
                    let id = if keyed { 0 } else { t.new_index_id(ix_oid) };
                    let index = Index {
                        oid: ix_oid,
                        name: name.clone(),
                        table: t.oid,
                        id,
                        columns: cols
                            .iter()
                            .map(|c| IndexColumn {
                                key: IndexKey::Column(*c),
                                desc: false,
                                nulls_first: false,
                                opclass: 0,
                                collation: None,
                                stattarget: None,
                                opclass_options: Vec::new(),
                            })
                            .collect(),
                        include,
                        unique: true,
                        primary,
                        nulls_not_distinct,
                        predicate: None,
                        method: "btree".into(),
                        // ALTER TABLE ONLY on a partitioned table with partitions: invalid
                        // until each partition's index is attached.
                        valid: !(self.only
                            && t.partkey.is_some()
                            && !self.cat.partitions(t.oid).is_empty()),
                        options,
                        parent: 0,
                        clustered: false,
                        replident: false,
                    };
                    for c in &cols {
                        if matches!(t.columns[*c].ty, DataType::Json | DataType::Xid) {
                            return Err(Error::new(
                                code::UNDEFINED_OBJECT,
                                format!("data type {} has no default operator class for access method \"btree\"", t.columns[*c].ty.display()),
                            )
                            .hint("You must specify an operator class for the index or define a default operator class for the data type."));
                        }
                    }
                    let con_oid = self.cat.alloc_con_oid(deferrable);
                    if primary {
                        t.pkey = Some(cols.clone());
                        t.pkey_index = Some(ix_oid);
                        t.pkey_unkeyed = !keyed;
                        let mut con = Constraint::new(
                            con_oid,
                            name,
                            ConstraintKind::PrimaryKey { index: ix_oid },
                            true,
                        );
                        (con.deferrable, con.initially_deferred) = (deferrable, initially_deferred);
                        t.constraints.push(con);
                        rewrite_for_pk = existing && keyed;
                    } else {
                        t.indexes.push(ix_oid);
                        let mut con = Constraint::new(
                            con_oid,
                            name,
                            ConstraintKind::Unique { index: ix_oid },
                            true,
                        );
                        (con.deferrable, con.initially_deferred) = (deferrable, initially_deferred);
                        t.constraints.push(con);
                    }
                    self.cat.put_table(t.clone());
                    self.cat.put_index(index.clone());
                    if existing && !keyed && t.partkey.is_none() {
                        self.build_index(&t, &index)?;
                    }
                    // Partitions get theirs (not under ONLY).
                    for p in self.cat.partitions(t.oid) {
                        if self.only {
                            break;
                        }
                        let Some(part) = self.cat.tables.get(&p).cloned() else {
                            continue;
                        };
                        let part = self.clone_index(&t, &index, (*part).clone(), true)?;
                        self.cat.put_table(part);
                    }
                }
                Pending::Like {
                    src,
                    defaults,
                    checks,
                    indexes,
                    comments,
                    constraints,
                    statistics,
                } => {
                    for i in defaults {
                        if let Some(c) = t.columns.get_mut(i)
                            && c.def_oid == LIKE_DEF
                        {
                            c.def_oid = self.cat.alloc_oid();
                        }
                    }
                    t = self.apply_pending(t, checks, existing)?;
                    if indexes {
                        t = self.like_indexes(&src, t, comments)?;
                    }
                    if statistics {
                        self.like_statistics(&src, &t, comments);
                    }
                    if comments {
                        let mut copied = Vec::new();
                        for (o, sub, text) in self.cat.comments.iter() {
                            if *o == src.oid && *sub > 0 {
                                let Some(c) = src.columns.get(*sub as usize - 1) else {
                                    continue;
                                };
                                if let Some(i) = t.column_index(&c.name) {
                                    copied.push((t.oid, i as i32 + 1, text.clone()));
                                }
                            } else if constraints
                                && *sub == 0
                                && let Some(sc) = src.constraints.iter().find(|c| {
                                    c.oid == *o && matches!(c.kind, ConstraintKind::Check { .. })
                                })
                                && let Some(tc) = t.constraints.iter().find(|c| c.name == sc.name)
                            {
                                copied.push((tc.oid, 0, text.clone()));
                            } else if *sub == 0
                                && let Some(ConstraintKind::NotNull { column }) = src
                                    .constraints
                                    .iter()
                                    .find(|c| c.oid == *o)
                                    .map(|c| &c.kind)
                                && let Some(i) = src
                                    .columns
                                    .get(*column)
                                    .and_then(|c| t.column_index(&c.name))
                                && let Some(tc) = t.constraints.iter().find(|c| {
                                    matches!(c.kind, ConstraintKind::NotNull { column } if column == i)
                                })
                            {
                                // (Not-null constraints' comments come along with them.)
                                copied.push((tc.oid, 0, text.clone()));
                            }
                        }
                        self.cat.comments_mut().extend(copied);
                    }
                }
                Pending::Fk(f) => {
                    // ATAddForeignKeyConstraint: actions that would write a generated column.
                    for &c in &f.cols {
                        let Some(col) = t.columns.get(c) else { continue };
                        if col.generated.is_none() {
                            continue;
                        }
                        if matches!(f.on_update, FkAction::SetNull | FkAction::SetDefault | FkAction::Cascade) {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "invalid ON UPDATE action for foreign key constraint containing generated column",
                            ));
                        }
                        if matches!(f.on_delete, FkAction::SetNull | FkAction::SetDefault) {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "invalid ON DELETE action for foreign key constraint containing generated column",
                            ));
                        }
                    }
                    let mut con = self.fk_constraint(&t, *f, existing)?;
                    t.constraints.push(con.clone());
                    // Partitions of the referenced table get a derived constraint each, then
                    // this table gets its check triggers; its partitions, a copy.
                    self.fk_ref_parts(&mut t, &con)?;
                    self.fk_check_trigger_oids(&mut t, &mut con);
                    for p in self.cat.partitions(t.oid) {
                        let Some(part) = self.cat.tables.get(&p).cloned() else {
                            continue;
                        };
                        let mut part = (*part).clone();
                        // (addFkRecurseReferencing takes over an equal key a partition has.)
                        self.clone_fk(&t, &con, &mut part, true)?;
                        self.cat.put_table(part);
                    }
                }
            }
        }
        if rewrite_for_pk {
            // Rows were keyed by a hidden rowid: re-key them by the primary key.
            self.relayout(&t, &t)?;
        }
        Ok(t)
    }

    /// EXCLUDE [USING method] (key WITH op, ...): an index plus the constraint.
    fn exclusion_constraint(
        &mut self,
        t: &mut Table,
        x: PendingExclusion,
        existing: bool,
    ) -> Result<()> {
        let method = x.method.as_str();
        if !matches!(
            method,
            "btree" | "hash" | "gin" | "gist" | "brin" | "spgist"
        ) {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("access method \"{method}\" does not exist"),
            ));
        }
        if matches!(method, "gin" | "brin") {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("access method \"{method}\" does not support exclusion constraints"),
            ));
        }
        let parts = self.index_parts(
            t,
            method,
            &x.elems,
            &x.including,
            x.predicate.as_ref(),
            true,
        )?;
        let am = system::access_method_oid(method);
        let primary = x.period == Some('p');
        if primary
            && (t.pkey.is_some() || t.constraints.iter().any(|c| c.period == Some('p')))
        {
            return Err(Error::new(
                code::INVALID_TABLE_DEFINITION,
                format!("multiple primary keys for table \"{}\" are not allowed", t.name),
            ));
        }
        // (Its values don't key rows: they may be equal where the periods don't overlap.)
        if primary && t.pk_clustered() {
            return Err(pk_clustered_unusable(t, "would have a primary key WITHOUT OVERLAPS"));
        }
        // WITHOUT OVERLAPS: the operators come from the opclasses (always `=` and `&&`).
        let checked_ops = if x.period.is_some() { &[][..] } else { &x.ops[..] };
        for ((op, ty), col) in checked_ops.iter().zip(&parts.key_types).zip(&parts.columns) {
            let res = crate::sql::func::resolve_binop(op, ty, ty)?;
            let shown = format!(
                "{op}({},{})",
                res.arg_types[0].display(),
                res.arg_types[1].display()
            );
            if !exclusion_commutative(op) {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("operator {shown} is not commutative"),
                )
                .detail("Only commutative operators can be used in exclusion constraints."));
            }
            let opclass = if col.opclass != 0 {
                col.opclass
            } else {
                system::default_opclass(am, ty)
            };
            let Some(&(_, _, opcname, family, ..)) =
                system::OPCLASSES.iter().find(|c| c.0 == opclass)
            else {
                continue;
            };
            if !exclusion_op_in_family(method, opcname, op) {
                let fname = system::OPFAMILIES
                    .iter()
                    .find(|f| f.0 == family)
                    .map_or(opcname, |f| f.2);
                return Err(Error::new(code::WRONG_OBJECT_TYPE, format!("operator {shown} is not a member of operator family \"{fname}\""))
                    .detail("The exclusion operator must be related to the index operator class for the constraint."));
            }
        }
        if x.period.is_none() {
            self.check_exclusion_partkey(t, &parts.columns, &x.ops)?;
        }
        let name = match x.name {
            Some(n) => {
                if t.constraints.iter().any(|c| c.name == n) {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{n}\" for relation \"{}\" already exists",
                            t.name
                        ),
                    ));
                }
                if self.cat.name_taken(t.schema, &n) {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{n}\" already exists"),
                    ));
                }
                n
            }
            None if primary => {
                self.choose_constraint_index_name(t, &Catalog::object_name(&t.name, &[], "pkey"))
            }
            None if x.period.is_some() => {
                let refs: Vec<&str> = parts.name_parts.iter().map(String::as_str).collect();
                self.choose_constraint_index_name(t, &Catalog::object_name(&t.name, &refs, "key"))
            }
            None => {
                let refs: Vec<&str> = parts.name_parts.iter().map(String::as_str).collect();
                self.cat
                    .choose_name(t.schema, &Catalog::object_name(&t.name, &refs, "excl"))
            }
        };
        // A primary key's columns get their not-nulls first.
        if primary {
            let cols: Vec<usize> = parts
                .columns
                .iter()
                .filter_map(|c| match c.key {
                    IndexKey::Column(i) => Some(i),
                    _ => None,
                })
                .collect();
            for c in cols {
                if existing {
                    self.check_not_null(t, c)?;
                }
                t.columns[c].not_null = true;
                if !t.constraints.iter().any(
                    |k| matches!(k.kind, ConstraintKind::NotNull { column } if column == c),
                ) {
                    let nn = self.constraint_name(t, &[t.columns[c].name.as_str()], "not_null");
                    let o = self.cat.alloc_oid();
                    t.constraints.push(Constraint::new(
                        o,
                        nn,
                        ConstraintKind::NotNull { column: c },
                        true,
                    ));
                }
            }
        }
        let options = super::objects::index_options(method, &x.options)?;
        let ix_oid = self.cat.alloc_oid();
        let index = Index {
            oid: ix_oid,
            name: name.clone(),
            table: t.oid,
            id: t.new_index_id(ix_oid),
            columns: parts.columns,
            include: parts.include,
            unique: false,
            primary: false,
            nulls_not_distinct: false,
            predicate: parts.predicate,
            method: method.into(),
            valid: true,
            options,
            parent: 0,
            clustered: false,
            replident: false,
        };
        t.indexes.push(ix_oid);
        let con_oid = self.cat.alloc_con_oid(x.deferrable);
        let mut con = Constraint::new(
            con_oid,
            name.clone(),
            ConstraintKind::Exclusion {
                index: ix_oid,
                ops: x.ops,
            },
            true,
        );
        (con.deferrable, con.initially_deferred) = (x.deferrable, x.initially_deferred);
        con.period = x.period;
        t.constraints.push(con);
        self.cat.put_table(t.clone());
        self.cat.put_index(index.clone());
        if existing {
            self.build_index(t, &index)?;
            self.validate_exclusion(t, ix_oid, &name)?;
        } else {
            crate::exec::sizing::index_created(ix_oid, crate::exec::sizing::models_build(&index.method), self.ctx);
        }
        Ok(())
    }

    /// Existing rows must not conflict with each other under a new exclusion constraint.
    fn validate_exclusion(&self, t: &Table, ix_oid: u32, name: &str) -> Result<()> {
        let ta = Arc::new(t.clone());
        let mut b = self.binder();
        b.cat = Arc::new(self.cat.clone());
        let Some(bix) = b
            .bound_indexes(&ta)?
            .into_iter()
            .find(|b| b.index.oid == ix_oid)
        else {
            return Ok(());
        };
        for (dkey, row) in self.table_rows(t)? {
            let Some(vals) = crate::exec::dml::index_values(&bix, &row, self.ctx)? else {
                continue;
            };
            crate::exec::dml::check_without_overlaps(t, &bix, &vals)?;
            if let Some(other) =
                crate::exec::dml::exclusion_conflict(t, &bix, &vals, Some(&dkey), self.ctx)?
            {
                let detail = crate::exec::dml::key_detail(t, &bix, &vals, self.ctx, "");
                let other = crate::exec::dml::key_detail(t, &bix, &other, self.ctx, "");
                return Err(Error::new(
                    code::EXCLUSION_VIOLATION,
                    format!("could not create exclusion constraint \"{name}\""),
                )
                .detail(format!(
                    "{} conflicts with key {}.",
                    detail.trim_end(),
                    other.trim_end().trim_start_matches("Key ")
                ))
                .table(self.cat.schema_name(t.schema), &t.name)
                .constraint(name));
            }
        }
        Ok(())
    }

    /// `{table}_{cols}_{suffix}`, unique among the table's constraints (and relation names,
    /// for index-backed ones).
    pub(super) fn constraint_name(&self, t: &Table, cols: &[&str], suffix: &str) -> String {
        Catalog::object_name(&t.name, cols, suffix)
            .choose(|n| t.constraints.iter().any(|c| c.name == n))
    }

    pub(super) fn bind_check(&self, t: &Table, sql: &str) -> Result<Expr> {
        self.bind_check_raw(t, sql, None)
    }

    /// `bind_check`, binding `raw` (the expression as written) when there is one; errors in
    /// the stored text carry no position.
    pub(super) fn bind_check_raw(&self, t: &Table, sql: &str, raw: Option<&pb::Node>) -> Result<Expr> {
        let parsed;
        let n = match raw {
            Some(n) => n,
            None => {
                parsed = ast::parse_expr(sql)?;
                &parsed
            }
        };
        let mut b = self.binder();
        let ta = Arc::new(t.clone());
        let e = b
            .bind_over_table(&ta, n, Clause::Check)
            .map_err(|e| if raw.is_none() { Error { position: None, ..e } } else { e })?;
        let mut sub = false;
        e.walk(&mut |x| {
            // A user function's body isn't a subquery of the constraint.
            if matches!(x, Expr::Subquery(q) if q.func.is_none()) {
                sub = true
            }
        });
        if sub {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot use subquery in check constraint",
            ));
        }
        b.coerce_bool(e, "CHECK")
    }

    pub(super) fn validate_check(&self, t: &Table, name: &str, e: &Expr) -> Result<()> {
        for r in self.scan_rows(t) {
            self.ctx.check()?;
            let (_, row) = r?;
            if eval(e, &row, self.ctx)? == Value::Bool(false) {
                return Err(Error::new(
                    code::CHECK_VIOLATION,
                    format!(
                        "check constraint \"{name}\" of relation \"{}\" is violated by some row",
                        t.name
                    ),
                )
                .table(self.cat.schema_name(t.schema), &t.name)
                .constraint(name));
            }
        }
        Ok(())
    }

    pub(super) fn check_not_null(&self, t: &Table, col: usize) -> Result<()> {
        for r in self.scan_rows(t) {
            self.ctx.check()?;
            let (_, row) = r?;
            if row[col].is_null() {
                return Err(Error::new(
                    code::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        t.columns[col].name, t.name
                    ),
                )
                .table(self.cat.schema_name(t.schema), &t.name)
                .column(&t.columns[col].name));
            }
        }
        Ok(())
    }

    fn fk_constraint(&mut self, t: &Table, f: PendingFk, existing: bool) -> Result<Constraint> {
        // The referenced table may be the one being defined.
        let parent: Table = if f.ref_rv.relname == t.name
            && (f.ref_rv.schemaname.is_empty()
                || self.cat.schema_oid(&f.ref_rv.schemaname).ok() == Some(t.schema))
        {
            t.clone()
        } else {
            (*self.table(&f.ref_rv)?).clone()
        };
        if self.only && t.partkey.is_some() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "cannot use ONLY for foreign key on partitioned table \"{}\" referencing relation \"{}\"",
                    t.name, parent.name
                ),
            ));
        }
        if parent.temporary && !t.temporary {
            return Err(Error::new(
                code::INVALID_TABLE_DEFINITION,
                "constraints on permanent tables may reference only permanent tables",
            ));
        }
        // validateFkOnDeleteSetColumns: the SET NULL / DEFAULT columns must be the key's.
        let mut del_set_cols: Vec<usize> = Vec::new();
        for n in &f.del_set_cols {
            let i = t.column_index(n).ok_or_else(|| fk_column_missing(n))?;
            if !f.cols.contains(&i) {
                return Err(Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    format!("column \"{n}\" referenced in ON DELETE SET action must be part of foreign key"),
                ));
            }
            if !del_set_cols.contains(&i) {
                del_set_cols.push(i);
            }
        }
        let with_period = f.period || f.pk_period;
        let period_err = |msg: &str| Error::new(code::INVALID_FOREIGN_KEY, msg.to_string());
        if with_period && !f.period {
            return Err(period_err(
                "foreign key uses PERIOD on the referenced table but not the referencing table",
            ));
        }
        // The referenced table's exclusion constraints (WITHOUT OVERLAPS ones among them):
        // (index, its plain columns, constraint).
        let exclusions: Vec<(u32, Option<Vec<usize>>, &Constraint)> = parent
            .constraints
            .iter()
            .filter(|c| matches!(c.kind, ConstraintKind::Exclusion { .. }))
            .filter_map(|c| {
                let ix = self.cat.indexes.get(&c.kind.index()?)?;
                let cols: Option<Vec<usize>> = ix
                    .columns
                    .iter()
                    .map(|k| match k.key {
                        IndexKey::Column(i) => Some(i),
                        _ => None,
                    })
                    .collect();
                Some((ix.oid, cols.filter(|_| ix.predicate.is_none()), c))
            })
            .collect();
        let temporal_pk = exclusions.iter().find(|(_, _, c)| c.period == Some('p'));
        let ref_cols: Vec<usize> = if f.ref_cols.is_empty() {
            if let Some((_, cols, c)) = temporal_pk {
                if c.deferrable {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "cannot use a deferrable primary key for referenced table \"{}\"",
                            parent.name
                        ),
                    ));
                }
                if !f.period {
                    return Err(period_err(
                        "foreign key uses PERIOD on the referenced table but not the referencing table",
                    ));
                }
                cols.clone().unwrap_or_default()
            } else {
                parent.pkey.clone().ok_or_else(|| {
                    Error::new(
                        code::INVALID_FOREIGN_KEY,
                        format!(
                            "there is no primary key for referenced table \"{}\"",
                            parent.name
                        ),
                    )
                })?
            }
        } else {
            let cols: Vec<usize> = f
                .ref_cols
                .iter()
                .map(|n| parent.column_index(n).ok_or_else(|| fk_column_missing(n)))
                .collect::<Result<_>>()?;
            if with_period && !f.pk_period {
                return Err(period_err(
                    "foreign key uses PERIOD on the referencing table but not the referenced table",
                ));
            }
            if (1..cols.len()).any(|i| cols[..i].contains(&cols[i])) {
                return Err(period_err(
                    "foreign key referenced-columns list must not contain duplicates",
                ));
            }
            cols
        };
        if ref_cols.len() != f.cols.len() {
            return Err(Error::new(
                code::INVALID_FOREIGN_KEY,
                "number of referencing and referenced columns for foreign key disagree",
            ));
        }
        // A unique constraint on exactly the referenced columns, not a deferrable one
        // (transformFkeyGetPrimaryKey, transformFkeyCheckAttrs); with PERIOD, an exclusion
        // constraint ending in the period column.
        let deferrable_index = |ix: u32| {
            parent
                .constraints
                .iter()
                .any(|c| c.kind.index() == Some(ix) && c.deferrable)
        };
        if f.ref_cols.is_empty() && parent.pkey_index.is_some_and(deferrable_index) {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot use a deferrable primary key for referenced table \"{}\"",
                    parent.name
                ),
            ));
        }
        let mut set: Vec<usize> = ref_cols.clone();
        set.sort();
        let same_set = |cols: &[usize]| {
            let mut c = cols.to_vec();
            c.sort();
            c == set
        };
        // (index oid, deferrable, WITHOUT OVERLAPS), in index oid order.
        let mut matches: Vec<(u32, bool, bool)> = Vec::new();
        if with_period {
            for (oid, cols, c) in &exclusions {
                if let Some(cols) = cols
                    && same_set(cols)
                    && cols.last() == ref_cols.last()
                {
                    matches.push((*oid, c.deferrable, true));
                }
            }
        } else {
            if let (Some(p), Some(pix)) = (&parent.pkey, parent.pkey_index)
                && same_set(p)
            {
                matches.push((pix, deferrable_index(pix), false));
            }
            for ix in self.cat.table_indexes(&parent) {
                let cols: Vec<usize> = ix
                    .columns
                    .iter()
                    .filter_map(|c| {
                        if let IndexKey::Column(i) = c.key {
                            Some(i)
                        } else {
                            None
                        }
                    })
                    .collect();
                if ix.unique
                    && ix.predicate.is_none()
                    && cols.len() == ix.columns.len()
                    && same_set(&cols)
                {
                    matches.push((ix.oid, deferrable_index(ix.oid), false));
                }
            }
            // A WITHOUT OVERLAPS key is unique too (indisunique), but needs PERIOD.
            for (oid, cols, c) in &exclusions {
                if c.without_overlaps()
                    && let Some(cols) = cols
                    && same_set(cols)
                {
                    matches.push((*oid, c.deferrable, true));
                }
            }
            matches.sort_by_key(|m| m.0);
        }
        let Some(&(_, _, pk_without_overlaps)) = matches.iter().find(|m| !m.1) else {
            if !matches.is_empty() {
                return Err(Error::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "cannot use a deferrable unique constraint for referenced table \"{}\"",
                        parent.name
                    ),
                ));
            }
            return Err(Error::new(
                code::INVALID_FOREIGN_KEY,
                format!(
                    "there is no unique constraint matching given keys for referenced table \"{}\"",
                    parent.name
                ),
            ));
        };
        if pk_without_overlaps && !with_period {
            return Err(period_err(
                "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS",
            ));
        }
        if parent.oid != t.oid && !self.is_superuser() {
            // checkFkeyPermissions: REFERENCES on the table or on every referenced column.
            let (me, dbo) = (self.me(), self.db_owner());
            let ok = self
                .cat
                .rel_mask(parent.oid, me, dbo)
                .is_none_or(|(p, _)| p & crate::catalog::acl::REFERENCES != 0)
                || ref_cols.iter().all(|c| {
                    self.cat.col_mask(parent.oid, *c as i32 + 1, me, dbo).0
                        & crate::catalog::acl::REFERENCES
                        != 0
                });
            if !ok {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for table {}", parent.name),
                ));
            }
        }
        if f.cols.iter().any(|&c| t.columns[c].virtual_gen) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "foreign key constraints on virtual generated columns are not supported",
            ));
        }
        // Only NO ACTION is supported with PERIOD.
        if f.period {
            for (what, a) in [("ON UPDATE", f.on_update), ("ON DELETE", f.on_delete)] {
                if a != FkAction::NoAction {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("unsupported {what} action for foreign key constraint using PERIOD"),
                    ));
                }
            }
        }
        let colnames: Vec<&str> = f.cols.iter().map(|c| t.columns[*c].name.as_str()).collect();
        let name = match f.name {
            Some(n) => n,
            None => self.constraint_name(t, &colnames, "fkey"),
        };
        for (c, r) in f.cols.iter().zip(&ref_cols) {
            let (a, b) = (&t.columns[*c].ty, &parent.columns[*r].ty);
            let compatible = fk_types_compatible(a, b);
            if !compatible {
                return Err(Error::new(code::DATATYPE_MISMATCH, format!("foreign key constraint \"{name}\" cannot be implemented")).detail(format!(
                    "Key columns \"{}\" of the referencing table and \"{}\" of the referenced table are of incompatible types: {} and {}.",
                    t.columns[*c].name,
                    parent.columns[*r].name,
                    // (format_type_be: no typmod.)
                    a.without_typmod().display(),
                    b.without_typmod().display()
                )));
            }
            // Different collations only when both are deterministic: the keys need one
            // notion of equality.
            if a.collatable() && b.collatable() {
                let coll = |col: &crate::catalog::Column| {
                    col.collation.clone().unwrap_or_else(|| {
                        col.ty.default_collation().unwrap_or("default").to_string()
                    })
                };
                let (fc, pc) = (coll(&t.columns[*c]), coll(&parent.columns[*r]));
                let det = |n: &str| {
                    crate::types::collation::by_name(&self.cat, n).is_none_or(|k| k.deterministic)
                };
                if fc != pc && (!det(&fc) || !det(&pc)) {
                    return Err(Error::new(code::COLLATION_MISMATCH, format!("foreign key constraint \"{name}\" cannot be implemented")).detail(format!(
                        "Key columns \"{}\" of the referencing table and \"{}\" of the referenced table have incompatible collations: \"{fc}\" and \"{pc}\".  If either collation is nondeterministic, then both collations have to be the same.",
                        t.columns[*c].name,
                        parent.columns[*r].name,
                    )));
                }
            }
        }
        if matches!(f.on_delete, FkAction::SetNull) || matches!(f.on_update, FkAction::SetNull) {
            for c in &f.cols {
                if t.columns[*c].generated.is_some() {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "invalid ON DELETE action for foreign key constraint containing generated column",
                    ));
                }
            }
        }
        if existing && !f.not_valid && f.enforced {
            self.validate_fk(t, &parent, &f.cols, &ref_cols, &name, f.match_full, f.period)?;
        }
        Ok(Constraint {
            deferrable: false,
            initially_deferred: false,
            inhcount: 0,
            islocal: true,
            parent: 0,
            // Referencing a partitioned table, the check triggers come after the constraints
            // derived for its partitions (`fk_ref_parts`).
            oid: self.alloc_fk_oid(if parent.partkey.is_some() { 2 } else { 4 }),
            name,
            kind: ConstraintKind::ForeignKey {
                columns: f.cols,
                ref_table: parent.oid,
                ref_columns: ref_cols,
                on_delete: f.on_delete,
                on_update: f.on_update,
                match_full: f.match_full,
                deferrable: f.deferrable,
                initially_deferred: f.initially_deferred,
            },
            validated: !f.not_valid && f.enforced,
            enforced: f.enforced,
            no_inherit: false,
            ri_enabled: Vec::new(),
            ri_check: 0,
            period: f.period.then_some('f'),
            del_set_cols,
            row_types: Vec::new(),
            from_parent: false,
        })
    }

    /// ALTER TABLE ... ADD [CONSTRAINT name] PRIMARY KEY | UNIQUE USING INDEX index
    /// (transformIndexConstraint's checks, then ATExecAddIndexConstraint): an existing unique
    /// index becomes the constraint's, renamed after it; a primary key's columns get their
    /// not-null constraints first.
    fn add_index_constraint(&mut self, mut t: Table, c: &pb::Constraint) -> Result<Table> {
        let primary = c.contype == pb::ConstrType::ConstrPrimary as i32;
        if t.partkey.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / ADD CONSTRAINT USING INDEX is not supported on partitioned tables",
            ));
        }
        let at = |e: Error| with_pos(e, c.location);
        let ix = match self.cat.relation_in(t.schema, &c.indexname) {
            Some(Relation::Index(ix)) => ix,
            Some(_) => {
                return Err(at(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not an index", c.indexname),
                )));
            }
            None => {
                return Err(at(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("index \"{}\" does not exist", c.indexname),
                )));
            }
        };
        let prereq = |msg: String| at(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, msg));
        if ix.table != t.oid {
            return Err(prereq(format!(
                "index \"{}\" does not belong to table \"{}\"",
                ix.name, t.name
            )));
        }
        if t.constraints.iter().any(|k| k.kind.index() == Some(ix.oid)) {
            return Err(prereq(format!(
                "index \"{}\" is already associated with a constraint",
                ix.name
            )));
        }
        if !ix.valid {
            return Err(prereq(format!("index \"{}\" is not valid", ix.name)));
        }
        let unusable = |msg: String| {
            at(Error::new(code::WRONG_OBJECT_TYPE, msg)
                .detail("Cannot create a primary key or unique constraint using such an index."))
        };
        if !ix.unique {
            return Err(unusable(format!("\"{}\" is not a unique index", ix.name)));
        }
        if ix.columns.iter().any(|k| matches!(k.key, IndexKey::Expr(_))) {
            return Err(unusable(format!("index \"{}\" contains expressions", ix.name)));
        }
        if ix.predicate.is_some() {
            return Err(unusable(format!("\"{}\" is a partial index", ix.name)));
        }
        if ix.method != "btree" {
            return Err(unusable(format!("index \"{}\" is not a btree", ix.name)));
        }
        let mut cols = Vec::new();
        for (n, k) in ix.columns.iter().enumerate() {
            let IndexKey::Column(i) = k.key else {
                continue;
            };
            let col = &t.columns[i];
            let default_op = crate::catalog::system::default_opclass(403, &col.ty);
            let coll_ok = k.collation.as_deref().is_none_or(|x| {
                x == col.collation.as_deref().unwrap_or("default")
            });
            if k.desc || k.nulls_first || (k.opclass != 0 && k.opclass != default_op) || !coll_ok
            {
                return Err(unusable(format!(
                    "index \"{}\" column number {} does not have default sorting behavior",
                    ix.name,
                    n + 1
                )));
            }
            cols.push(i);
        }
        // A primary key's columns are made NOT NULL first (an earlier pass).
        if primary {
            for col in &cols {
                self.check_not_null(&t, *col)?;
                t.columns[*col].not_null = true;
                if !t.constraints.iter().any(
                    |k| matches!(k.kind, ConstraintKind::NotNull { column } if column == *col),
                ) {
                    let nn =
                        self.constraint_name(&t, &[t.columns[*col].name.as_str()], "not_null");
                    let o = self.cat.alloc_oid();
                    t.constraints.push(Constraint::new(
                        o,
                        nn,
                        ConstraintKind::NotNull { column: *col },
                        true,
                    ));
                }
            }
            if t.pkey.is_some() {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("multiple primary keys for table \"{}\" are not allowed", t.name),
                ));
            }
            // (index_check_primary_key)
            if ix.nulls_not_distinct {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    "primary keys cannot use NULLS NOT DISTINCT indexes",
                ));
            }
        }
        let name = if c.conname.is_empty() {
            ix.name.clone()
        } else {
            c.conname.clone()
        };
        let mut nix = (*ix).clone();
        if name != ix.name {
            self.notice(format!(
                "ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index \"{}\" to \"{name}\"",
                ix.name
            ));
            if self.cat.name_taken(t.schema, &name) {
                return Err(Error::new(
                    code::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists"),
                ));
            }
            self.cat.remove_name(t.schema, &ix.name);
            nix.name = name.clone();
        }
        let con_oid = self.cat.alloc_con_oid(c.deferrable);
        let kind = if primary {
            ConstraintKind::PrimaryKey { index: ix.oid }
        } else {
            ConstraintKind::Unique { index: ix.oid }
        };
        let mut con = Constraint::new(con_oid, name, kind, true);
        (con.deferrable, con.initially_deferred) = (c.deferrable, c.initdeferred);
        t.constraints.push(con);
        if primary {
            // The index keeps its own entries (only `roll_pk_clustered` or the legacy layout
            // keys the rows by it).
            nix.primary = true;
            let keyed = pk_keys_rows(&t, c.deferrable)?;
            if keyed {
                nix.id = 0;
            }
            t.indexes.retain(|i| *i != ix.oid);
            t.pkey = Some(cols);
            t.pkey_index = Some(ix.oid);
            t.pkey_unkeyed = !keyed;
            self.cat.put_index(nix);
            self.cat.put_table(t.clone());
            if keyed {
                self.relayout(&t, &t)?;
            }
        } else {
            self.cat.put_index(nix);
        }
        Ok(t)
    }

    /// A foreign key referencing a partitioned table takes its check triggers' oids after
    /// its derived constraints' (addFkRecurseReferenced, then addFkRecurseReferencing).
    pub(super) fn fk_check_trigger_oids(&mut self, t: &mut Table, con: &mut Constraint) {
        let ConstraintKind::ForeignKey { ref_table, .. } = &con.kind else {
            return;
        };
        let partitioned = if *ref_table == t.oid {
            t.partkey.is_some()
        } else {
            self.cat.tables.get(ref_table).is_some_and(|r| r.partkey.is_some())
        };
        if !partitioned {
            return;
        }
        con.ri_check = self.cat.alloc_oids(2) - 1;
        if let Some(c) = t.constraints.iter_mut().find(|c| c.oid == con.oid) {
            c.ri_check = con.ri_check;
        }
    }

    /// A foreign key's oid, then `n` more for its internal triggers (`trigger::ri_triggers`).
    pub(super) fn alloc_fk_oid(&mut self, n: u32) -> u32 {
        let oid = self.cat.alloc_oid();
        for _ in 0..n {
            self.cat.alloc_oid();
        }
        oid
    }

    /// Check a foreign key's existing rows, as adding it does (the referenced table locked
    /// SHARE ROW EXCLUSIVE: addFkConstraint).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn validate_fk(
        &self,
        t: &Table,
        parent: &Table,
        cols: &[usize],
        ref_cols: &[usize],
        name: &str,
        match_full: bool,
        period: bool,
    ) -> Result<()> {
        self.validate_fk_locking(t, parent, cols, ref_cols, name, match_full, period, lockmode::SHARE_ROW_EXCLUSIVE)
    }

    /// `validate_fk`, locking the referenced table in `ref_mode` (VALIDATE CONSTRAINT:
    /// ROW SHARE).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn validate_fk_locking(
        &self,
        t: &Table,
        parent: &Table,
        cols: &[usize],
        ref_cols: &[usize],
        name: &str,
        match_full: bool,
        period: bool,
        ref_mode: i32,
    ) -> Result<()> {
        if t.partkey.is_some() {
            // Each partition checks its own rows.
            for leaf in self.cat.leaves(t.oid) {
                let lcols: Vec<usize> = cols
                    .iter()
                    .filter_map(|c| leaf.column_index(&t.columns[*c].name))
                    .collect();
                // (Each partition's copy of the key is checked under its own name.)
                let lname = leaf
                    .constraints
                    .iter()
                    .find(|c| {
                        c.parent != 0
                            && matches!(&c.kind, ConstraintKind::ForeignKey { columns, ref_table, .. }
                                if *columns == lcols && *ref_table == parent.oid)
                    })
                    .map_or(name.to_string(), |c| c.name.clone());
                self.validate_fk_locking(&leaf, parent, &lcols, ref_cols, &lname, match_full, period, ref_mode)?;
            }
            return Ok(());
        }
        if period {
            // Row by row, as the check trigger does (RI_Initial_Check isn't used).
            let parent = Arc::new(parent.clone());
            for r in self.scan_rows(t) {
                self.ctx.check()?;
                let (_, row) = r?;
                crate::exec::dml::check_fk_period(t, &parent, cols, ref_cols, name, match_full, &row, self.ctx)?;
            }
            return Ok(());
        }
        // addFkConstraint: SHARE ROW EXCLUSIVE on the referenced table and (partitioned) its
        // partitions, so its rows' writers are out; RI_Initial_Check then reads over the
        // latest snapshot (what they committed while this waited).
        self.note_lock(parent.oid, ref_mode)?;
        if parent.partkey.is_some() {
            for p in self.cat.inheritors(parent.oid) {
                self.note_lock(p, ref_mode)?;
            }
        }
        // Keys compare under the referenced columns' collations: a nondeterministic one by
        // its sort keys.
        let colls: Vec<_> = ref_cols
            .iter()
            .map(|c| {
                let name = parent.columns.get(*c)?.collation.clone()?;
                crate::types::collation::by_name(&self.cat, &name).filter(|k| !k.deterministic)
            })
            .collect();
        let key_of = |vals: &[Value]| -> Vec<u8> {
            let vals: Vec<Value> = vals
                .iter()
                .zip(&colls)
                .map(|(v, c)| match c {
                    Some(c) => crate::types::collation::key_value(c, v),
                    None => v.clone(),
                })
                .collect();
            key_bytes(&vals)
        };
        let parents: HashSet<Vec<u8>> = self
            .tree_rows_at(parent, true)?
            .into_iter()
            .map(|r| key_of(&ref_cols.iter().map(|c| r[*c].clone()).collect::<Vec<_>>()))
            .collect();
        for r in self.scan_rows(t) {
            self.ctx.check()?;
            let (_, row) = r?;
            let vals: Vec<Value> = cols.iter().map(|c| row[*c].clone()).collect();
            let nulls = vals.iter().filter(|v| v.is_null()).count();
            // RI_Initial_Check: MATCH FULL takes all or none of the key NULL.
            if match_full && nulls > 0 && nulls < vals.len() {
                return Err(Error::new(code::FOREIGN_KEY_VIOLATION, format!("insert or update on table \"{}\" violates foreign key constraint \"{name}\"", t.name))
                    .detail("MATCH FULL does not allow mixing of null and nonnull key values.")
                    .table(self.cat.schema_name(t.schema), &t.name)
                    .constraint(name));
            }
            if nulls > 0 {
                continue;
            }
            // (Compared as the referenced columns' type: an int key can reference numeric.)
            let as_ref: Vec<Value> = vals
                .iter()
                .zip(ref_cols)
                .map(|(v, r)| crate::exec::dml::coerce_key(v, &parent.columns[*r].ty, self.ctx))
                .collect::<Result<_>>()?;
            if !parents.contains(&key_of(&as_ref)) {
                let fmt = self.ctx.fmt();
                let cn: Vec<&str> = cols.iter().map(|c| t.columns[*c].name.as_str()).collect();
                let shown: Vec<String> = vals
                    .iter()
                    .zip(cols)
                    .map(|(v, c)| format_value(v, &t.columns[*c].ty, fmt))
                    .collect();
                return Err(Error::new(code::FOREIGN_KEY_VIOLATION, format!("insert or update on table \"{}\" violates foreign key constraint \"{name}\"", t.name))
                    .detail(format!("Key ({})=({}) is not present in table \"{}\".", cn.join(", "), shown.join(", "), parent.name))
                    .table(self.cat.schema_name(t.schema), &t.name)
                    .constraint(name));
            }
        }
        Ok(())
    }

    /// The error building index `ix` would stop at (computing a key, or a unique index's
    /// duplicate; None: none), found without building it.
    pub(super) fn index_duplicate(&mut self, t: &Table, ix: &Index) -> Result<Option<Error>> {
        let ta = Arc::new(t.clone());
        let mut b = self.binder();
        b.cat = Arc::new(self.cat.clone());
        let Some(bix) = b
            .bound_indexes_all(&ta)?
            .into_iter()
            .find(|b| b.index.oid == ix.oid)
        else {
            return Ok(None);
        };
        // (Unique keys with the data key of the row that had each first: `build_index`.)
        let mut seen = UniqueKeys::default();
        // (Duplicates are keys equal under their collations, a nondeterministic one's too:
        // the same entry prefix.)
        let colls = dup_colls(&self.cat, t, &bix);
        let unique_key = |vals: &[Value]| index_value_prefix(t, ix, vals, &colls);
        let stored_values = |d: &[u8]| -> Result<Option<Vec<Value>>> {
            let bytes = self.ctx.txn.borrow().get(d)?;
            match bytes {
                Some(b) => index_values(&bix, &crate::exec::scan::decode_table_row(t, &b)?, self),
                None => Ok(None),
            }
        };
        for r in self.scan_rows(t) {
            self.ctx.check()?;
            let (dkey, row) = r?;
            let vals = match index_values(&bix, &row, self) {
                Ok(Some(v)) => v,
                Ok(None) => continue,
                Err(e) => return Ok(Some(e)),
            };
            if ix.unique && (ix.nulls_not_distinct || !vals.iter().any(Value::is_null)) {
                let first = seen.insert(unique_key(&vals), &dkey, |d| {
                    Ok(stored_values(d)?.map(|v| unique_key(&v)))
                })?;
                if let Some(first) = first {
                    let first = stored_values(&first)?;
                    return Ok(Some(self.duplicate_key(t, &bix, first.as_deref().unwrap_or(&vals), &ix.name)));
                }
            }
        }
        Ok(None)
    }

    /// Remove an index's entries (before it is built again).
    pub(super) fn clear_index_entries(&mut self, t: &Table, ix: &Index) -> Result<()> {
        if ix.id == 0 {
            return Ok(());
        }
        let start = crate::storage::keys::index_prefix(t.oid, ix.id);
        let end = crate::storage::keys::prefix_end(&start);
        let keys: Vec<Vec<u8>> = self
            .ctx
            .txn
            .borrow()
            .scan(&start, &end)
            .map(|e| e.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        let mut txn = self.ctx.txn.borrow_mut();
        for k in keys {
            txn.delete(k)?;
        }
        Ok(())
    }

    /// Fill a new index from the table's rows, checking uniqueness.
    pub(super) fn build_index(&mut self, t: &Table, ix: &Index) -> Result<()> {
        let ta = Arc::new(t.clone());
        let bix = bound_indexes(&ta, self.ctx)?
            .into_iter()
            .find(|b| b.index.oid == ix.oid);
        let bix = match bix {
            Some(b) => b,
            None => {
                let mut b = self.binder();
                b.cat = Arc::new(self.cat.clone());
                b.bound_indexes_all(&ta)?
                    .into_iter()
                    .find(|b| b.index.oid == ix.oid)
                    .ok_or_else(|| crate::error::internal("index not found"))?
            }
        };
        // Unique keys seen so far, with the data key of the row that had each first (its
        // values are read again for the error).
        let mut seen = UniqueKeys::default();
        // (Duplicates are keys equal under their collations, a nondeterministic one's too:
        // the same entry prefix.)
        let colls = dup_colls(&self.cat, t, &bix);
        let unique_key = |vals: &[Value]| index_value_prefix(t, ix, vals, &colls);
        let stored_values = |d: &[u8]| -> Result<Option<Vec<Value>>> {
            let bytes = self.ctx.txn.borrow().get(d)?;
            match bytes {
                Some(b) => index_values(&bix, &crate::exec::scan::decode_table_row(t, &b)?, self),
                None => Ok(None),
            }
        };
        // (A row at a time: nothing of the table is held but the unique keys, and a batch of
        // entries: a big build's go in as sorted runs, `Txn::put_new_index_entries`.)
        let range_start = crate::storage::keys::index_prefix(t.oid, ix.id);
        let range_end = crate::storage::keys::prefix_end(&range_start);
        let mut batch: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut batch_bytes = 0usize;
        // (How the build's batches go in, once the first has: as sorted runs or not.)
        let mut direct: Option<bool> = None;
        // (Half the spill threshold, at most 32 MB: a build too big to buffer.)
        let run_bytes = (self.ctx.txn.borrow().db.store.txn_spill_bytes() / 2).min(32 << 20);
        for r in self.scan_rows(t) {
            self.ctx.check()?;
            let (dkey, row) = r?;
            let Some(vals) = index_values(&bix, &row, self)? else {
                continue;
            };
            if ix.unique && (ix.nulls_not_distinct || !vals.iter().any(Value::is_null)) {
                let first = seen.insert(unique_key(&vals), &dkey, |d| {
                    Ok(stored_values(d)?.map(|v| unique_key(&v)))
                })?;
                if let Some(first) = first {
                    let first = stored_values(&first)?;
                    return Err(self.duplicate_key(t, &bix, first.as_deref().unwrap_or(&vals), &ix.name));
                }
            }
            if ix.id != 0 {
                let k = index_entry_key(t, ix, &vals, &dkey, &bix.colls);
                batch_bytes += k.len() + dkey.len() + 64;
                batch.push((k, dkey));
                if batch_bytes >= run_bytes {
                    let b = std::mem::take(&mut batch);
                    let d = self
                        .ctx
                        .txn
                        .borrow_mut()
                        .put_new_index_entries(b, &range_start, &range_end, direct)?;
                    direct = Some(d);
                    batch_bytes = 0;
                }
            }
        }
        if direct.is_some() {
            self.ctx
                .txn
                .borrow_mut()
                .put_new_index_entries(batch, &range_start, &range_end, direct)?;
        } else {
            let mut txn = self.ctx.txn.borrow_mut();
            for (k, dkey) in batch {
                txn.put_new_index_entry(k, dkey)?;
            }
        }
        drop(seen);
        self.index_built(t, ix)
    }

    fn duplicate_key(&self, t: &Table, bix: &BoundIndex, vals: &[Value], name: &str) -> Error {
        let fmt = self.ctx.fmt();
        let cols: Vec<String> = bix
            .index
            .columns
            .iter()
            .map(|c| match &c.key {
                IndexKey::Column(i) => t.columns[*i].name.clone(),
                IndexKey::Expr(e) => e.clone(),
            })
            .collect();
        let shown: Vec<String> = vals
            .iter()
            .zip(&bix.keys)
            .map(|(v, k)| {
                if v.is_null() {
                    "null".into()
                } else {
                    format_value(v, &k.ty(), fmt)
                }
            })
            .collect();
        Error::new(
            code::UNIQUE_VIOLATION,
            format!("could not create unique index \"{name}\""),
        )
        .detail(format!(
            "Key ({})=({}) is duplicated.",
            cols.join(", "),
            shown.join(", ")
        ))
        .table(self.cat.schema_name(t.schema), &t.name)
        .constraint(name)
    }

    /// Rewrite every row of `old` into the layout of `new` (re-keying and rebuilding all
    /// indexes). `f` maps an old row to a new one.
    pub(super) fn rewrite(
        &mut self,
        old: &Table,
        new: &Table,
        f: impl Fn(Row) -> Result<Row>,
    ) -> Result<()> {
        let rows = self.table_rows(old)?;
        // The rows keep their places in the heap (a rewrite Postgres does too happens at the
        // end of the ALTER TABLE).
        let saved = crate::exec::sizing::detach(old, &self.ctx.sess.db);
        crate::exec::dml::truncate(&Arc::new(old.clone()), self.ctx)?;
        let mut cat2 = self.cat.clone();
        // A scratch copy: oids it would give out aren't taken from the cluster's counter.
        cat2.oids = None;
        cat2.put_table(new.clone());
        let mut b = self.binder();
        b.cat = Arc::new(cat2);
        let nt = Arc::new(new.clone());
        let bixs = b.bound_indexes(&nt)?;
        let mut seen: Vec<HashSet<Vec<u8>>> = vec![HashSet::new(); bixs.len()];
        let rowid_seq = rowid_sequence(new);
        let mut written = Vec::with_capacity(rows.len());
        // A NOT VALID not-null isn't checked by a rewrite.
        let invalid_nn: Vec<usize> = new
            .constraints
            .iter()
            .filter(|k| !k.validated)
            .filter_map(|k| match k.kind {
                ConstraintKind::NotNull { column } => Some(column),
                _ => None,
            })
            .collect();
        for (okey, row) in rows {
            let mut nr = f(row)?;
            nr.resize(new.columns.len(), Value::Null);
            for (i, c) in new.columns.iter().enumerate() {
                if c.not_null && !c.dropped && !invalid_nn.contains(&i) && nr[i].is_null() {
                    return Err(Error::new(
                        code::NOT_NULL_VIOLATION,
                        format!(
                            "column \"{}\" of relation \"{}\" contains null values",
                            c.name, new.name
                        ),
                    )
                    .table(self.cat.schema_name(new.schema), &new.name)
                    .column(&c.name));
                }
            }
            let rowid = if new.has_rowid() {
                Some(self.ctx.sess.db.nextval(&rowid_seq)?)
            } else {
                None
            };
            let dkey = data_key(new, &nr, rowid);
            for (n, bix) in bixs.iter().enumerate() {
                let Some(vals) = index_values(bix, &nr, self)? else {
                    continue;
                };
                if bix.index.unique
                    && (bix.index.nulls_not_distinct || !vals.iter().any(Value::is_null))
                    && !seen[n].insert(index_value_prefix(new, &bix.index, &vals, &dup_colls(&self.cat, new, bix)))
                {
                    return Err(self.duplicate_key(new, bix, &vals, &bix.index.name));
                }
                if bix.index.id != 0 {
                    self.ctx
                        .txn
                        .borrow_mut()
                        .put(index_entry_key(new, &bix.index, &vals, &dkey, &bix.colls), dkey.clone())?;
                }
            }
            let xid = self.ctx.txn.borrow_mut().cur_xid();
            self.ctx
                .txn
                .borrow_mut()
                .put(dkey.clone(), encode_table_row(&nr, xid))?;
            written.push((okey, dkey));
        }
        crate::exec::sizing::reattach(new, saved, &written, &self.ctx.sess.db);
        Ok(())
    }

    /// Store `new`'s rows under the keys its layout gives them: by its primary key's
    /// values (`storage_pkey`), or by new row ids in their heap order; its indexes'
    /// entries are made again (a primary key keying the rows has none). Unlike `rewrite`
    /// the stored rows are kept as they are (xmin, heap TID), and so are their places in
    /// the modelled heap: only the keys change. Two rows with the same key values are a
    /// unique violation of the primary key. (`old`: the table as its models know it.)
    pub(super) fn relayout(&mut self, old: &Table, new: &Table) -> Result<()> {
        use crate::storage::keys::{index_prefix, prefix_end};
        let start = index_prefix(new.oid, 0);
        let mut rows: Vec<(Vec<u8>, bytes::Bytes)> = self
            .ctx
            .txn
            .borrow()
            .scan(&start, &prefix_end(&start))
            .collect::<Result<_>>()?;
        crate::exec::sizing::in_heap_order(new, &mut rows, self.ctx);
        // (The models stay as they are, the rows' keys aside: nothing moves in the heap.)
        let saved = crate::exec::sizing::detach(old, &self.ctx.sess.db);
        let keys: Vec<Vec<u8>> = self
            .ctx
            .txn
            .borrow()
            .scan(&start, &index_prefix(new.oid, u32::MAX))
            .map(|e| e.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        for k in keys {
            self.ctx.txn.borrow_mut().delete(k)?;
        }
        let mut cat2 = self.cat.clone();
        // A scratch copy: oids it would give out aren't taken from the cluster's counter.
        cat2.oids = None;
        cat2.put_table(new.clone());
        let mut b = self.binder();
        b.cat = Arc::new(cat2);
        let nt = Arc::new(new.clone());
        let bixs = b.bound_indexes(&nt)?;
        let rowid_seq = rowid_sequence(new);
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut written = Vec::with_capacity(rows.len());
        for (okey, bytes) in rows {
            self.ctx.check()?;
            let row = crate::exec::scan::decode_table_row(new, &bytes)?;
            let dkey = if new.has_rowid() {
                let id = self.ctx.sess.db.nextval_by_one(new.oid, || rowid_seq.clone())?;
                data_key(new, &row, Some(id))
            } else {
                let cols = new.storage_pkey().cloned().unwrap_or_default();
                if let Some(&c) = cols.iter().find(|&&c| row[c].is_null()) {
                    let name = &new.columns[c].name;
                    return Err(Error::new(
                        code::NOT_NULL_VIOLATION,
                        format!("column \"{name}\" of relation \"{}\" contains null values", new.name),
                    )
                    .table(self.cat.schema_name(new.schema), &new.name)
                    .column(name));
                }
                let dkey = data_key(new, &row, None);
                // (Duplicates are keys equal under their collations, a nondeterministic one's
                // too: the key's entry prefix, not the data key's bytes.)
                let vals: Vec<Value> = cols.iter().map(|&c| row[c].clone()).collect();
                let pk = bixs.iter().find(|b| Some(b.index.oid) == new.pkey_index);
                let dup = match pk {
                    Some(pk) => index_value_prefix(new, &pk.index, &vals, &dup_colls(&self.cat, new, pk)),
                    None => dkey.clone(),
                };
                if !seen.insert(dup) {
                    if let Some(pk) = pk {
                        return Err(self.duplicate_key(new, pk, &vals, &pk.index.name));
                    }
                    return Err(crate::error::internal("primary key index not found"));
                }
                dkey
            };
            for bix in bixs.iter().filter(|b| b.index.id != 0) {
                if let Some(vals) = index_values(bix, &row, self)? {
                    self.ctx
                        .txn
                        .borrow_mut()
                        .put(index_entry_key(new, &bix.index, &vals, &dkey, &bix.colls), dkey.clone())?;
                }
            }
            self.ctx.txn.borrow_mut().put(dkey.clone(), bytes.to_vec())?;
            written.push((okey, dkey));
        }
        crate::exec::sizing::reattach(new, saved, &written, &self.ctx.sess.db);
        crate::exec::sizing::gin_rekeyed(new, &written, &self.ctx.sess.db);
        Ok(())
    }

    /// ALTER TABLE SET / RESET gives `t` the options `opts`. Where that turns
    /// `roll_pk_clustered` on or off, its rows are laid out again with its primary key
    /// keying them or an index of its own. A partitioned table's partitions follow when the
    /// command names the option (`named`; not under ONLY). The table as it is then.
    pub(super) fn pk_clustered_change(
        &mut self,
        mut t: Table,
        opts: Vec<String>,
        named: bool,
    ) -> Result<Table> {
        let on = crate::catalog::pk_clustered_option(&opts);
        let was = t.pk_clustered();
        t.options = opts;
        let partitions = if named && t.partkey.is_some() && !self.only {
            self.cat.partitions(t.oid)
        } else {
            Vec::new()
        };
        if on == was && partitions.is_empty() {
            return Ok(t);
        }
        let pk = t.pkey_index.and_then(|i| self.cat.indexes.get(&i).cloned());
        if on && !was {
            if t.constraints.iter().any(|c| c.period == Some('p')) {
                return Err(pk_clustered_unusable(&t, "has a primary key WITHOUT OVERLAPS"));
            }
            let Some(pk) = &pk else {
                return Err(pk_clustered_unusable(&t, "has no primary key"));
            };
            if t.constraints.iter().any(|c| c.kind.index() == Some(pk.oid) && c.deferrable) {
                return Err(pk_clustered_unusable(
                    &t,
                    &format!("has a deferrable primary key \"{}\"", pk.name),
                ));
            }
        }
        // The key keys the rows, or has entries of its own. (A partitioned table stores no
        // rows: its key just follows. A legacy table keyed by its key is already keyed.)
        if let Some(pk) = pk
            && t.pkey_unkeyed == on
        {
            let mut ix = (*pk).clone();
            if on {
                ix.id = 0;
            } else {
                ix.id = t.new_index_id(ix.oid);
            }
            t.pkey_unkeyed = !on;
            self.cat.put_index(ix);
            self.cat.put_table(t.clone());
            if t.partkey.is_none() {
                self.relayout(&t, &t)?;
                // Its cached insertion target was of the old keys.
                crate::exec::sizing::invalidated(t.oid, &self.ctx.sess.db);
            }
        }
        self.cat.put_table(t.clone());
        let set = format!("{}={on}", crate::catalog::PK_CLUSTERED);
        for p in partitions {
            let Some(part) = self.cat.tables.get(&p).map(|p| (**p).clone()) else {
                continue;
            };
            let mut popts: Vec<String> = part
                .options
                .iter()
                .filter(|o| !names_pk_clustered(std::slice::from_ref(o)))
                .cloned()
                .collect();
            if on {
                popts.push(set.clone());
            }
            if part.is_foreign() {
                continue;
            }
            let part = self.pk_clustered_change(part, popts, true)?;
            self.cat.put_table(part);
        }
        Ok(t)
    }

    /// The primary key of `t` went: the rows it keyed under `roll_pk_clustered` are going
    /// back to the default layout (the caller re-keys them), and the option with it.
    pub(super) fn pk_clustered_dropped(&self, t: &mut Table) {
        if t.pk_clustered() {
            t.options.retain(|o| !names_pk_clustered(std::slice::from_ref(o)));
        }
    }

    // ── CREATE TABLE AS / SELECT INTO / CREATE MATERIALIZED VIEW ──

    pub(super) fn create_table_as(&mut self, s: &pb::CreateTableAsStmt) -> Result<String> {
        let into = s.into.as_ref().unwrap();
        let rv = into.rel.as_ref().unwrap();
        let matview = pb::ObjectType::try_from(s.objtype) == Ok(pb::ObjectType::ObjectMatview);
        self.check_on_commit(into.on_commit, rv)?;
        let (schema, name) = self.create_target(rv)?;
        if self.cat.name_taken(schema, &name) {
            if s.if_not_exists {
                self.notice_code(
                    code::DUPLICATE_TABLE,
                    format!("relation \"{name}\" already exists, skipping"),
                );
                return Ok(if matview {
                    "CREATE MATERIALIZED VIEW".into()
                } else {
                    "CREATE TABLE AS".into()
                });
            }
            return Err(Error::new(
                code::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists"),
            ));
        }
        let q = s.query.as_deref().unwrap();
        let mut b = self.binder();
        // WITH NO DATA isn't planned. A materialized view is created WITH NO DATA and then
        // refreshed, which plans its query: what fails then has taken the view's oids.
        b.unplanned = into.skip_data || matview;
        b.persistent = matview;
        let mut bq = b.bind_statement(q)?;
        if !into.skip_data {
            bq.check_perms(self.ctx)?;
        }
        let colnames = names(&into.col_names);
        if colnames.len() > bq.cols.len() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "too many column names were specified",
            ));
        }
        let mut cols: Vec<OutCol> = bq.cols.clone();
        for (c, n) in cols.iter_mut().zip(&colnames) {
            c.name = n.clone();
        }
        let mut collations = Vec::with_capacity(cols.len());
        for c in &cols {
            let ty = if matches!(c.ty, DataType::Unknown) {
                DataType::Text
            } else {
                c.ty.clone()
            };
            collations.push(b.stored_collation(c, &ty, true)?);
        }
        super::super::super::sql::binder::check_unique_names(
            cols.iter().map(|c| c.name.as_str()),
            |n| {
                Error::new(
                    code::DUPLICATE_COLUMN,
                    format!("column \"{n}\" specified more than once"),
                )
            },
        )?;
        for c in &cols {
            system_name(&c.name)?;
        }
        for c in &cols {
            super::objects::check_column_type(&c.name, &c.ty)?;
        }
        let spc = self.choose_tablespace(
            &into.table_space_name,
            false,
            rv.relpersistence == "t",
            None,
        )?;
        let oid = self.cat.alloc_oid();
        self.cat.set_rel_tablespace(oid, spc);
        self.register_on_commit(into.on_commit, oid);
        let mut t = Table {
            oid,
            schema,
            name: name.clone(),
            owner: self.me(),
            columns: cols
                .iter()
                .map(|c| {
                    let mut col = Column::new(
                        c.name.clone(),
                        if matches!(c.ty, DataType::Unknown) {
                            DataType::Text
                        } else {
                            c.ty.clone()
                        },
                    );
                    col.domain = c.domain;
                    col
                })
                .zip(collations)
                .map(|(mut col, coll)| {
                    col.collation = coll;
                    col
                })
                .collect(),
            pkey: None,
            pkey_index: None,
            pkey_unkeyed: false,
            indexes: Vec::new(),
            constraints: Vec::new(),
            next_index_id: 1,
            rowid_seq: None,
            // (RangeVarAdjustRelationPersistence: made in a temporary schema, it is
            // temporary.)
            temporary: rv.relpersistence == "t"
                || self
                    .cat
                    .schemas
                    .get(&schema)
                    .is_some_and(|s| s.name.starts_with("pg_temp")),
            unlogged: rv.relpersistence == "u",
            comment: None,
            rowtype: 0,
            rowarray: 0,
            toast: 0,
            toast_relname: None,
            toast_indname: None,
            rls: false,
            rls_force: false,
            of_type: 0,
            inherits: Vec::new(),
            partkey: None,
            partbound: None,
            options: heap_options(&into.options, &[], false)?,
            replident: None,
            foreign: None,
            detach_pending: false,
        };
        if matview && names_pk_clustered(&t.options) {
            return Err(pk_clustered_unusable(&t, "is a materialized view"));
        }
        // The relation exists (with its row type, TOAST table and, for a materialized view,
        // its rule) before the query runs (intorel_startup), so a query failing then has
        // taken their oids.
        let (rowtype, rowarray) = self.cat.alloc_rowtype();
        self.cat.add_toast(&mut t);
        let rule = if matview { self.cat.alloc_oid() } else { 0 };
        // A large rule action goes to pg_rewrite's TOAST table (an oid for the value).
        if matview {
            let names: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            let cat = Arc::new(self.cat.clone());
            if crate::sql::nodetree::view_rule_tree(self.ctx, &cat, &ast::deparse_stmt(q)?, &names)
                .is_some_and(|t| crate::sql::nodetree::rule_action_toasted(&t))
            {
                self.cat.alloc_oid();
            }
        }
        if !matview {
            (t.rowtype, t.rowarray) = (rowtype, rowarray);
        } else if !into.skip_data {
            // The refresh's new heap, which the query fills; then the query is planned.
            self.cat.rewrite_heap(&mut t, false);
            bq = self.binder().bind_statement(q)?;
        }
        if self.ctx.instr.borrow().is_some() {
            *self.ctx.explained.borrow_mut() = Some((bq.plan.clone(), bq.hidden_rtes));
        }
        let rows = if into.skip_data {
            Vec::new()
        } else if matview {
            // (A security-restricted operation: RestrictSearchPath.)
            let level = self.ctx.sess.settings.borrow_mut().restrict_search_path();
            let r = collect(&bq.plan, self.ctx, None);
            self.ctx.sess.settings.borrow_mut().pop_restricted(level);
            r?
        } else {
            collect(&bq.plan, self.ctx, None)?
        };
        let n = rows.len();
        let tag = if into.skip_data && !s.is_select_into {
            if matview {
                "CREATE MATERIALIZED VIEW".to_string()
            } else {
                "CREATE TABLE AS".to_string()
            }
        } else {
            format!("SELECT {n}")
        };
        if matview {
            let names: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            let query = match self.normalized_view_query(q, &bq.cols, &names) {
                Some(text) => text,
                None => ast::deparse_stmt(q)?,
            };
            let view = ViewDef {
                oid,
                schema,
                name: name.clone(),
                owner: t.owner,
                query,
                columns: t
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.ty.clone()))
                    .collect(),
                domains: t.columns.iter().map(|c| c.domain).collect(),
                collations: t.columns.iter().map(|c| c.collation.clone()).collect(),
                defaults: Vec::new(),
                default_oids: Vec::new(),
                materialized: true,
                check_option: None,
                storage: Some(Arc::new(t.clone())),
                deps: self.view_deps(q),
                rowtype,
                rowarray,
                rule,
                options: Vec::new(),
                populated: !into.skip_data,
                rowtype_cols: Vec::new(),
            };
            self.cat.put_view(view);
        } else {
            t.name = name;
            self.cat.put_table(t.clone());
        }
        self.save()?;
        self.write_new_rows(&t, rows)?;
        Ok(tag)
    }

    /// ATRewriteTable: the rows copied into a new heap in the order a sequential scan of
    /// the old one reads them, each recording its new place.
    fn rewrite_heap_rows(&self, t: &Table) -> Result<()> {
        let mut rows = self.table_rows(t)?;
        crate::exec::sizing::in_heap_order(t, &mut rows, self.ctx);
        let tids = crate::exec::sizing::loaded(t, &rows, false, self.ctx);
        let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
        crate::exec::sizing::stamp_tids(&keys, &tids, self.ctx)
    }

    /// Insert rows into a table with no indexes (fresh CTAS / matview storage).
    pub(super) fn write_new_rows(&self, t: &Table, rows: Vec<Row>) -> Result<()> {
        let mut written = Vec::with_capacity(rows.len());
        for r in rows {
            let id = self.ctx.sess.db.nextval_by_one(t.oid, || rowid_sequence(t))?;
            written.push((data_key(t, &r, Some(id)), r));
        }
        crate::exec::pgstat::count_insert(self.ctx, t.oid, written.len() as i64);
        crate::exec::pgstat::io_insert(self.ctx, t.oid, written.len() as i64, "bulkwrite");
        // A bulk insert into the new heap; a materialized view's rows are frozen. Each row
        // records the heap TID it got (its ctid).
        let frozen = self.cat.views.get(&t.oid).is_some_and(|v| v.materialized);
        let tids = crate::exec::sizing::loaded(t, &written, frozen, self.ctx);
        let xid = self.ctx.txn.borrow_mut().cur_xid();
        for (i, (k, r)) in written.into_iter().enumerate() {
            let bytes = crate::exec::scan::encode_table_row_at(&r, xid, tids.get(i).copied());
            self.ctx.txn.borrow_mut().put(k, bytes)?;
        }
        Ok(())
    }

    // ── ALTER TABLE ──

    pub(super) fn alter_table(&mut self, s: &pb::AlterTableStmt) -> Result<String> {
        let rv = s.relation.as_ref().unwrap();
        let kind = pb::ObjectType::try_from(s.objtype).unwrap_or(pb::ObjectType::ObjectTable);
        match kind {
            pb::ObjectType::ObjectIndex => return self.alter_index(s),
            pb::ObjectType::ObjectSequence => return self.alter_sequence_cmds(s),
            pb::ObjectType::ObjectView | pb::ObjectType::ObjectMatview => {
                return self.alter_view(s, kind);
            }
            pb::ObjectType::ObjectType => return self.alter_composite(s),
            _ => {}
        }
        let t = match self.lookup(rv)? {
            Some(Relation::Table(t)) => t,
            None if s.missing_ok => {
                self.notice(format!(
                    "relation \"{}\" does not exist, skipping",
                    rv.relname
                ));
                return Ok(if kind == pb::ObjectType::ObjectForeignTable {
                    "ALTER FOREIGN TABLE".into()
                } else {
                    "ALTER TABLE".into()
                });
            }
            Some(_) if kind == pb::ObjectType::ObjectForeignTable => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a foreign table", rv.relname),
                ));
            }
            // ALTER TABLE takes a view too, for what applies to one (ATSimplePermissions).
            Some(Relation::View(v)) if kind == pb::ObjectType::ObjectTable => {
                use pb::AlterTableType as A;
                let word = if v.materialized { "materialized view" } else { "view" };
                self.require_owner(v.owner, word, &v.name)?;
                for c in &s.cmds {
                    let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                        continue;
                    };
                    let sub = A::try_from(cmd.subtype).unwrap_or(A::Undefined);
                    let ok = if v.materialized {
                        matches!(
                            sub,
                            A::AtSetRelOptions
                                | A::AtResetRelOptions
                                | A::AtReplaceRelOptions
                                | A::AtChangeOwner
                                | A::AtSetTableSpace
                                | A::AtClusterOn
                                | A::AtDropCluster
                                | A::AtSetStatistics
                                | A::AtSetOptions
                                | A::AtResetOptions
                                | A::AtSetStorage
                                | A::AtSetCompression
                                | A::AtSetAccessMethod
                        )
                    } else {
                        matches!(
                            sub,
                            A::AtColumnDefault
                                | A::AtSetRelOptions
                                | A::AtResetRelOptions
                                | A::AtReplaceRelOptions
                                | A::AtChangeOwner
                        )
                    };
                    if !ok && let Some(action) = super::foreign::alter_action(sub) {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!(
                                "ALTER action {action} cannot be performed on relation \"{}\"",
                                v.name
                            ),
                        )
                        .detail(format!("This operation is not supported for {word}s.")));
                    }
                }
                self.alter_view(
                    s,
                    if v.materialized {
                        pb::ObjectType::ObjectMatview
                    } else {
                        pb::ObjectType::ObjectView
                    },
                )?;
                return Ok("ALTER TABLE".into());
            }
            // ALTER TABLE on an index: what applies to a table only fails as Postgres's
            // checks do (transformPartitionCmd, ATSimplePermissions).
            Some(Relation::Index(ix)) if kind == pb::ObjectType::ObjectTable => {
                use pb::AlterTableType as A;
                let partitioned = self
                    .cat
                    .index_table(ix.table)
                    .is_some_and(|t| t.partkey.is_some());
                for c in &s.cmds {
                    let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                        continue;
                    };
                    match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
                        A::AtAttachPartition if partitioned => {
                            return Err(Error::new(
                                code::INVALID_OBJECT_DEFINITION,
                                format!("\"{}\" is not a partitioned table", rv.relname),
                            ));
                        }
                        sub @ (A::AtDetachPartition | A::AtDetachPartitionFinalize) => {
                            let action = super::foreign::alter_action(sub).unwrap_or("DETACH PARTITION");
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!(
                                    "ALTER action {action} cannot be performed on relation \"{}\"",
                                    ix.name
                                ),
                            )
                            .detail(if partitioned {
                                "This operation is not supported for partitioned indexes."
                            } else {
                                "This operation is not supported for indexes."
                            }));
                        }
                        _ => {}
                    }
                }
                self.table(rv)?
            }
            // (RangeVarCallbackForAlterRelation: the owner, and then not a system catalog.)
            None if system::SysTable::lookup(Self::rv_schema(rv), &rv.relname, self.search_path())
                .is_some() =>
            {
                if !self.is_superuser() {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("must be owner of table {}", rv.relname),
                    ));
                }
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied: \"{}\" is a system catalog", rv.relname),
                ));
            }
            _ => self.table(rv)?,
        };
        self.require_owner(
            t.owner,
            if t.is_foreign() { "foreign table" } else { "table" },
            &t.name,
        )?;
        if kind == pb::ObjectType::ObjectForeignTable && !t.is_foreign() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a foreign table", rv.relname),
            ));
        }
        self.not_in_use(t.oid, "ALTER TABLE", &t.name)?;
        self.note_alter_locks(&t, &s.cmds)?;
        let mut t = (*t).clone();
        let orig = t.clone();
        // Several ALTER COLUMN TYPEs: their USINGs all read the row as it was, converted
        // in one rewrite.
        {
            use pb::AlterTableType as A;
            let kinds: Vec<A> = s
                .cmds
                .iter()
                .filter_map(|c| match inner(c) {
                    Some(NodeEnum::AlterTableCmd(cmd)) => {
                        Some(A::try_from(cmd.subtype).unwrap_or(A::Undefined))
                    }
                    _ => None,
                })
                .collect();
            if kinds.iter().filter(|k| **k == A::AtAlterColumnType).count() > 1
                && kinds
                    .iter()
                    .all(|k| matches!(k, A::AtAlterColumnType | A::AtDropColumn))
                && t.partkey.is_none()
                && t.inherits.is_empty()
                && self.cat.inheritors(t.oid).len() <= 1
            {
                self.combined_types = Some((Arc::new(t.clone()), Vec::new()));
            }
        }
        for c in &s.cmds {
            let Some(NodeEnum::AlterTableCmd(cmd)) = inner(c) else {
                continue;
            };
            // ATPrepCmd: a partition pending detach takes nothing but FINALIZE.
            if t.detach_pending
                && t.partbound.is_some()
                && cmd.subtype != pb::AlterTableType::AtDetachPartitionFinalize as i32
            {
                return Err(super::partition::pending_detach_error(
                    format!("cannot alter partition \"{}\" with an incomplete detach", t.name),
                    None,
                ));
            }
            self.foreign_alter_check(&t, cmd)?;
            self.inh_precheck(&t, cmd, rv.inh)?;
            // What ALTER COLUMN TYPE re-creates comes back before the later passes' work.
            if !self.type_rebuilds.is_empty() && !before_old_index_pass(cmd) {
                self.flush_type_rebuilds(Some(&mut t))?;
            }
            let before = t.clone();
            self.only = !rv.inh;
            let r = self.alter_cmd(t, cmd);
            self.only = false;
            t = r?;
            self.inh_recurse(&before, &t, cmd, rv.inh)?;
        }
        if let Some((from, convs)) = self.combined_types.take()
            && !convs.is_empty()
        {
            self.combined_rewrite(&from, &t, &convs)?;
        }
        self.flush_type_rebuilds(Some(&mut t))?;
        self.flush_stat_rebuilds();
        for n in std::mem::take(&mut self.merge_notices) {
            self.notice(n);
        }
        // The end of ATRewriteCatalogs: a TOAST table if the table now needs one; then the
        // rewrites (ATRewriteTables), the table's and those of inheritors the commands reached.
        self.cat.add_toast(&mut t);
        if !self.rewrites.is_empty()
            && self.cat.event_triggers.values().any(|e| e.event == "table_rewrite")
        {
            self.cat.put_table(t.clone());
        }
        for oid in std::mem::take(&mut self.rewrites) {
            self.fire_table_rewrite(oid)?;
            self.ctx.txn.borrow_mut().mark_new_storage(oid);
            let xmin = self.recent_xmin();
            if oid == t.oid {
                // The rewrite writes every value out: no column keeps a "missing" value
                // (atthasmissing goes off).
                if t.columns.iter().any(|c| c.missing.is_some()) {
                    if self.has_rows(&t)? {
                        let cur = t.clone();
                        self.rewrite(&cur, &cur, Ok)?;
                    }
                    for c in &mut t.columns {
                        c.missing = None;
                    }
                }
                self.cat.rewrite_heap(&mut t, false);
                self.set_frozen_xid(&t.clone(), xmin);
                self.rewrite_heap_rows(&t)?;
            } else if let Some(c) = self.cat.tables.get(&oid).cloned() {
                let mut c = (*c).clone();
                self.cat.add_toast(&mut c);
                self.cat.rewrite_heap(&mut c, false);
                self.set_frozen_xid(&c, xmin);
                self.rewrite_heap_rows(&c)?;
                self.cat.put_table(c);
            }
        }
        self.cat.put_table(t);
        // Values of its row type stored elsewhere take its new columns.
        self.retype_relation_rows(&orig)?;
        self.save()?;
        Ok(if kind == pb::ObjectType::ObjectForeignTable {
            "ALTER FOREIGN TABLE".into()
        } else {
            "ALTER TABLE".into()
        })
    }

    pub(super) fn alter_table_cmd(&mut self, t: Table, cmd: &pb::AlterTableCmd) -> Result<Table> {
        self.alter_cmd(t, cmd)
    }

    pub(super) fn alter_cmd(&mut self, mut t: Table, cmd: &pb::AlterTableCmd) -> Result<Table> {
        use pb::AlterTableType as A;
        self.foreign_alter_check(&t, cmd)?;
        // A foreign table takes no primary key, unique, exclusion or foreign key constraint.
        if t.is_foreign() {
            match cmd.def.as_deref().and_then(inner) {
                Some(NodeEnum::Constraint(c)) => super::foreign::foreign_constraint_check(c)?,
                Some(NodeEnum::ColumnDef(cd)) if cmd.subtype == A::AtAddColumn as i32 => {
                    for c in &cd.constraints {
                        if let Some(NodeEnum::Constraint(c)) = inner(c) {
                            super::foreign::foreign_constraint_check(c)?;
                        }
                    }
                }
                _ => {}
            }
        }
        let col_idx = |t: &Table, name: &str| -> Result<usize> {
            t.column_index(name).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_COLUMN,
                    format!(
                        "column \"{name}\" of relation \"{}\" does not exist",
                        t.name
                    ),
                )
            })
        };
        // A typed table's columns follow its type (ALTER TYPE ... CASCADE changes them).
        if t.of_type != 0 && !self.typed_cascade {
            let typed = |msg: &str| Error::new(code::WRONG_OBJECT_TYPE, msg.to_string());
            match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
                A::AtAddColumn => return Err(typed("cannot add column to typed table")),
                A::AtDropColumn => return Err(typed("cannot drop column from typed table")),
                A::AtAlterColumnType => {
                    let loc = match cmd.def.as_deref().and_then(inner) {
                        Some(NodeEnum::ColumnDef(cd)) => cd.location,
                        _ => -1,
                    };
                    return Err(with_pos(
                        typed("cannot alter column type of typed table"),
                        loc,
                    ));
                }
                _ => {}
            }
        }
        match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
            A::AtAddOf => {
                let Some(NodeEnum::TypeName(tn)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                let comp = self.typed_table_type(tn)?;
                if !t.inherits.is_empty() {
                    return Err(Error::new(code::WRONG_OBJECT_TYPE, "typed tables cannot inherit"));
                }
                let mismatch = |msg: String| Error::new(code::DATATYPE_MISMATCH, msg);
                let live: Vec<&crate::catalog::Column> = t.live_columns().map(|(_, c)| c).collect();
                for (k, (fname, fty)) in comp.fields.iter().enumerate() {
                    let Some(c) = live.get(k) else {
                        return Err(mismatch(format!("table is missing column \"{fname}\"")));
                    };
                    if c.name != *fname {
                        return Err(mismatch(format!(
                            "table has column \"{}\" where type requires \"{fname}\"",
                            c.name
                        )));
                    }
                    if c.ty != *fty {
                        return Err(mismatch(format!(
                            "table \"{}\" has different type for column \"{fname}\"",
                            t.name
                        )));
                    }
                }
                if let Some(c) = live.get(comp.fields.len()) {
                    return Err(mismatch(format!("table has extra column \"{}\"", c.name)));
                }
                t.of_type = comp.oid;
            }
            A::AtDropOf => {
                if t.of_type == 0 {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a typed table", t.name),
                    ));
                }
                t.of_type = 0;
            }
            A::AtAddColumn => {
                let Some(NodeEnum::ColumnDef(cd)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                if t.column_index(&cd.colname).is_some() {
                    if cmd.missing_ok {
                        self.notice_code(
                            code::DUPLICATE_COLUMN,
                            format!(
                                "column \"{}\" of relation \"{}\" already exists, skipping",
                                cd.colname, t.name
                            ),
                        );
                        return Ok(t);
                    }
                    return Err(Error::new(
                        code::DUPLICATE_COLUMN,
                        format!(
                            "column \"{}\" of relation \"{}\" already exists",
                            cd.colname, t.name
                        ),
                    ));
                }
                let mut pending = Vec::new();
                let mut seqs = Vec::new();
                let old = t.clone();
                self.column_def(&mut t, cd, &mut pending, &mut seqs)?;
                let idx = t.columns.len() - 1;
                // Composite values of the row type stored elsewhere can't get a non-null
                // value for the new column (find_composite_type_dependencies).
                let c = &t.columns[idx];
                let valued = c.identity.is_some()
                    || c.generated.as_ref().is_some_and(|_| !c.virtual_gen)
                    || c.default.as_deref().is_some_and(|d| !d.eq_ignore_ascii_case("null"));
                if valued
                    && t.partbound.is_none()
                    && let Some((tn, cn)) = self.column_using(t.rowtype)
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot alter table \"{}\" because column \"{tn}.{cn}\" uses its row type",
                            t.name
                        ),
                    ));
                }
                // A partition's identity column is its partitioned table's: same sequence.
                let shared = t.partbound.is_some() && t.columns[idx].identity.is_some();
                if shared
                    && let Some(parent) = t.inherits.first().and_then(|p| self.cat.tables.get(p).cloned())
                    && let Some(pc) = parent.column_index(&t.columns[idx].name).map(|j| parent.columns[j].clone())
                    && pc.identity.is_some()
                {
                    let own = t.columns[idx].seq;
                    seqs.retain(|s| Some(s.oid) != own);
                    t.columns[idx].identity = pc.identity.clone();
                    t.columns[idx].seq = pc.seq;
                }
                for sq in seqs {
                    self.cat.put_sequence(sq);
                }
                if t.is_foreign() && !cd.fdwoptions.is_empty() {
                    self.foreign_column_options(&mut t, &cd.colname, &cd.fdwoptions)?;
                }
                let before = Arc::new(self.cat.clone());
                self.cat.put_table(t.clone());
                let added = super::stored::Change::AddColumn(t.oid, t.columns[idx].name.clone());
                self.follow_renames(&before, &[added])?;
                self.fill_new_column(&old, &mut t, idx)?;
                // The rows' new values are of the column's domain: checked
                // (ATRewriteTable's domain constraint checks).
                if t.columns[idx].domain.is_some() {
                    let nt = Arc::new(t.clone());
                    let rules = crate::sql::binder::table_rules(&nt, self.ctx)?;
                    if let Some(Some(d)) = rules.domain_checks.get(idx) {
                        for (_, row) in self.table_rows(&t)? {
                            d.check(row.get(idx).unwrap_or(&Value::Null), self.ctx)?;
                        }
                    }
                }
                t = self.apply_pending(t, pending, true)?;
            }
            A::AtDropColumn => {
                let Some(idx) = t.column_index(&cmd.name) else {
                    if matches!(cmd.name.as_str(), "tableoid" | "ctid" | "xmin" | "cmin" | "xmax" | "cmax") {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("cannot drop system column \"{}\"", cmd.name),
                        ));
                    }
                    if cmd.missing_ok {
                        self.notice(format!(
                            "column \"{}\" of relation \"{}\" does not exist, skipping",
                            cmd.name, t.name
                        ));
                        return Ok(t);
                    }
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            cmd.name, t.name
                        ),
                    ));
                };
                let cascade = cmd.behavior == pb::DropBehavior::DropCascade as i32;
                let before = Arc::new(self.cat.clone());
                let dropped = super::stored::Change::DropColumn(t.oid, idx as i16 + 1);
                t = self.drop_column(t, idx, cascade)?;
                self.follow_renames(&before, &[dropped])?;
            }
            A::AtColumnDefault => {
                let idx = col_idx(&t, &cmd.name)?;
                if t.columns[idx].identity.is_some() {
                    let e = Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "column \"{}\" of relation \"{}\" is an identity column",
                            cmd.name, t.name
                        ),
                    );
                    return Err(if cmd.def.is_some() {
                        e
                    } else {
                        e.hint("Use ALTER TABLE ... ALTER COLUMN ... DROP IDENTITY instead.")
                    });
                }
                if t.columns[idx].generated.is_some() {
                    let e = Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "column \"{}\" of relation \"{}\" is a generated column",
                            cmd.name, t.name
                        ),
                    );
                    return Err(if cmd.def.is_some() {
                        e.hint("Use ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION instead.")
                    } else if !t.columns[idx].virtual_gen {
                        e.hint("Use ALTER TABLE ... ALTER COLUMN ... DROP EXPRESSION instead.")
                    } else {
                        e
                    });
                }
                match cmd.def.as_deref() {
                    Some(e) => {
                        let ty = t.columns[idx].ty.clone();
                        // (ATExecColumnDefault cooks it without the query text: no position.)
                        self.check_default(e, &ty, &cmd.name)
                            .map_err(|e| Error { position: None, ..e })?;
                        // (SET DEFAULT NULL removes the default: a NULL constant isn't stored.)
                        t.columns[idx].default = if t.columns[idx].domain.is_none()
                            && self.is_null_default(e, &ty)
                        {
                            None
                        } else {
                            Some(ast::deparse_expr(e)?)
                        };
                        // A new pg_attrdef row replaces the old.
                        t.columns[idx].def_oid = self.cat.alloc_oid();
                    }
                    None => {
                        // Dropping the pg_attrdef row is what sql_drop triggers see.
                        if t.columns[idx].default.is_some() {
                            use crate::catalog::depend::{ATTRDEF, flag};
                            super::event_trigger::note_dropped(
                                &self.cat,
                                &[((ATTRDEF, t.columns[idx].def_oid, 0), flag::ORIGINAL)],
                            );
                        }
                        t.columns[idx].default = None
                    }
                }
            }
            A::AtSetNotNull => {
                let idx = col_idx(&t, &cmd.name)?;
                // A NO INHERIT one can't become inherited by recursing.
                if !self.only
                    && let Some(k) = t.constraints.iter().find(|k| {
                        k.no_inherit && matches!(k.kind, ConstraintKind::NotNull { column } if column == idx)
                    })
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cannot change NO INHERIT status of NOT NULL constraint \"{}\" on relation \"{}\"",
                            k.name, t.name
                        ),
                    ));
                }
                // A NOT VALID one gets validated.
                let invalid = t.constraints.iter().any(|c| {
                    !c.validated && matches!(c.kind, ConstraintKind::NotNull { column } if column == idx)
                });
                if !t.columns[idx].not_null || invalid {
                    t = self.apply_pending(
                        t,
                        vec![Pending::NotNull {
                            col: idx,
                            name: None,
                            no_inherit: false,
                            not_valid: false,
                            table_level: false,
                        }],
                        true,
                    )?;
                }
            }
            A::AtDropNotNull => {
                let idx = col_idx(&t, &cmd.name)?;
                if t.columns[idx].identity.is_some() {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "column \"{}\" of relation \"{}\" is an identity column",
                            cmd.name, t.name
                        ),
                    ));
                }
                not_null_droppable(&self.cat, &t, idx)?;
                t.columns[idx].not_null = false;
                t.constraints.retain(
                    |c| !matches!(c.kind, ConstraintKind::NotNull { column } if column == idx),
                );
            }
            A::AtAlterColumnType => {
                // (ATPrepAlterColumnType: a system column can't change, at the type.)
                if t.column_index(&cmd.name).is_none()
                    && matches!(cmd.name.as_str(), "tableoid" | "ctid" | "xmin" | "cmin" | "xmax" | "cmax")
                {
                    let loc = match cmd.def.as_deref().and_then(inner) {
                        Some(NodeEnum::ColumnDef(cd)) => cd.location,
                        _ => -1,
                    };
                    return Err(with_pos(
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("cannot alter system column \"{}\"", cmd.name),
                        ),
                        loc,
                    ));
                }
                let idx = col_idx(&t, &cmd.name)?;
                let Some(NodeEnum::ColumnDef(cd)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                if t.is_foreign() && cd.raw_default.is_some() {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a table", t.name),
                    ));
                }
                if t.columns[idx].virtual_gen
                    && let Some(tn) = cd.type_name.as_ref()
                    && resolve_type_name(&self.cat, self.search_path(), tn)?.1.is_some()
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!(
                            "virtual generated column \"{}\" cannot have a domain type",
                            cmd.name
                        ),
                    ));
                }
                if t.columns[idx].generated.is_some() && cd.raw_default.is_some() {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            "cannot specify USING when altering type of generated column",
                        )
                        .detail(format!("Column \"{}\" is a generated column.", cmd.name)),
                        cd.location,
                    ));
                }
                t = self.alter_column_type(t, idx, cd)?;
                self.rebuild_column_statistics(&t, idx);
                // The column's statistics, and those of indexes rebuilt over it, go.
                let name = t.columns[idx].name.clone();
                let mut rels = vec![t.oid];
                for i in t.indexes.iter().chain(&t.pkey_index) {
                    if self.cat.indexes.get(i).is_some_and(|ix| {
                        ix.columns.iter().any(|c| match &c.key {
                            IndexKey::Column(k) => *k == idx,
                            IndexKey::Expr(e) => view_mentions(e, &name),
                        }) || ix.include.contains(&idx)
                            || ix
                                .predicate
                                .as_deref()
                                .is_some_and(|p| view_mentions(p, &name))
                    }) {
                        rels.push(*i);
                    }
                }
                for r in rels {
                    if let Some(rows) = self.cat.colstats.get_mut(&r) {
                        rows.retain(|s| r != t.oid || s.attnum != idx as i16 + 1);
                        if r != t.oid {
                            rows.clear();
                        }
                    }
                }
            }
            A::AtAddConstraint => {
                let Some(NodeEnum::Constraint(c)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                if !c.indexname.is_empty() {
                    return self.add_index_constraint(t, c);
                }
                // A check constraint may merge into an inherited one of the same name.
                // ... and a not-null one into the column's own (AdjustNotNullInheritance).
                let nn_col = (c.contype == pb::ConstrType::ConstrNotnull as i32)
                    .then(|| names(&c.keys).first().and_then(|n| t.column_index(n)))
                    .flatten();
                let merges = |x: &Constraint| {
                    (c.contype == pb::ConstrType::ConstrCheck as i32
                        && !x.islocal
                        && matches!(x.kind, ConstraintKind::Check { .. }))
                        || matches!(x.kind, ConstraintKind::NotNull { column } if Some(column) == nn_col)
                };
                // An index-backed one's name is its index's: a relation of the name is the
                // clash reported.
                if !c.conname.is_empty()
                    && matches!(
                        pb::ConstrType::try_from(c.contype),
                        Ok(pb::ConstrType::ConstrPrimary | pb::ConstrType::ConstrUnique | pb::ConstrType::ConstrExclusion)
                    )
                    && self.cat.name_taken(t.schema, &c.conname)
                {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{}\" already exists", c.conname),
                    ));
                }
                if !c.conname.is_empty()
                    && t.constraints.iter().any(|x| x.name == c.conname && !merges(x))
                {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{}\" for relation \"{}\" already exists",
                            c.conname, t.name
                        ),
                    ));
                }
                // A primary key's columns are made NOT NULL first (a missing one is
                // reported as ALTER COLUMN would).
                if c.contype == pb::ConstrType::ConstrPrimary as i32
                    && c.indexname.is_empty()
                    && let Some(k) = names(&c.keys).into_iter().find(|k| t.column_index(k).is_none())
                {
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{k}\" of relation \"{}\" does not exist", t.name),
                    ));
                }
                if let Some(p) = self.table_constraint(&t, c, None)? {
                    t = self.apply_pending(t, vec![p], true)?;
                }
            }
            A::AtDropConstraint => {
                let cascade = cmd.behavior == pb::DropBehavior::DropCascade as i32;
                t = self.drop_constraint(t, &cmd.name, cmd.missing_ok, cascade)?;
            }
            A::AtValidateConstraint => {
                let Some(i) = t.constraints.iter().position(|c| c.name == cmd.name) else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "constraint \"{}\" of relation \"{}\" does not exist",
                            cmd.name, t.name
                        ),
                    ));
                };
                if !matches!(
                    t.constraints[i].kind,
                    ConstraintKind::Check { .. }
                        | ConstraintKind::ForeignKey { .. }
                        | ConstraintKind::NotNull { .. }
                ) {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "cannot validate constraint \"{}\" of relation \"{}\"",
                            cmd.name, t.name
                        ),
                    )
                    .detail("This operation is not supported for this type of constraint."));
                }
                if !t.constraints[i].enforced {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        "cannot validate NOT ENFORCED constraint",
                    ));
                }
                if !t.constraints[i].validated {
                    match t.constraints[i].kind.clone() {
                        ConstraintKind::Check { expr, .. } => {
                            let e = self.bind_check(&t, &expr)?;
                            self.validate_check(&t, &cmd.name, &e)?;
                        }
                        ConstraintKind::ForeignKey {
                            columns,
                            ref_table,
                            ref_columns,
                            match_full,
                            ..
                        } => {
                            let parent = self
                                .cat
                                .tables
                                .get(&ref_table)
                                .cloned()
                                .ok_or_else(|| crate::error::internal("parent"))?;
                            // (ATExecValidateConstraint: ROW SHARE on the referenced table.)
                            self.validate_fk_locking(
                                &t,
                                &parent,
                                &columns,
                                &ref_columns,
                                &cmd.name,
                                match_full,
                                t.constraints[i].is_period(),
                                lockmode::ROW_SHARE,
                            )?;
                        }
                        ConstraintKind::NotNull { column } => {
                            self.check_not_null(&t, column)?;
                        }
                        _ => {}
                    }
                    t.constraints[i].validated = true;
                    // QueueFKConstraintValidation: the constraints derived from a foreign
                    // key for a partitioned referenced table's partitions are valid with it.
                    if matches!(t.constraints[i].kind, ConstraintKind::ForeignKey { .. }) {
                        let mut done = vec![t.constraints[i].oid];
                        loop {
                            let more: Vec<usize> = (0..t.constraints.len())
                                .filter(|k| {
                                    done.contains(&t.constraints[*k].parent)
                                        && !done.contains(&t.constraints[*k].oid)
                                })
                                .collect();
                            if more.is_empty() {
                                break;
                            }
                            for k in more {
                                t.constraints[k].validated = true;
                                done.push(t.constraints[k].oid);
                            }
                        }
                    }
                }
            }
            A::AtAddIdentity | A::AtDropIdentity | A::AtSetIdentity if t.partbound.is_some() => {
                return Err(Error::new(
                    code::INVALID_TABLE_DEFINITION,
                    match A::try_from(cmd.subtype) {
                        Ok(A::AtAddIdentity) => "cannot add identity to a column of a partition",
                        Ok(A::AtDropIdentity) => {
                            "cannot drop identity from a column of a partition"
                        }
                        _ => "cannot change identity column of a partition",
                    },
                ));
            }
            A::AtAddIdentity => {
                let idx = col_idx(&t, &cmd.name)?;
                let Some(NodeEnum::Constraint(c)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                let ty = t.columns[idx].ty.clone();
                // (The sequence's CREATE checks the type: DefineSequence.)
                if !matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8) {
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        "identity column type must be smallint, integer, or bigint",
                    ));
                }
                let seq = self.owned_sequence(&t, &cmd.name, idx, &ty, &c.options)?;
                if let Some(k) = t.constraints.iter().find(|k| {
                    !k.validated && matches!(k.kind, ConstraintKind::NotNull { column } if column == idx)
                }) {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("incompatible NOT VALID constraint \"{}\" on relation \"{}\"", k.name, t.name),
                    )
                    .hint("You might need to validate it using ALTER TABLE ... VALIDATE CONSTRAINT."));
                }
                // The sequence is made first (transformAlterTableStmt), then checked.
                if !t.columns[idx].not_null {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "column \"{}\" of relation \"{}\" must be declared NOT NULL before identity can be added",
                            cmd.name, t.name
                        ),
                    ));
                }
                if t.columns[idx].identity.is_some() {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "column \"{}\" of relation \"{}\" is already an identity column",
                            cmd.name, t.name
                        ),
                    ));
                }
                if t.columns[idx].default.is_some() {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "column \"{}\" of relation \"{}\" already has a default value",
                            cmd.name, t.name
                        ),
                    ));
                }
                t.columns[idx].identity = Some(if c.generated_when == "a" {
                    Identity::Always
                } else {
                    Identity::ByDefault
                });
                t.columns[idx].seq = Some(seq.oid);
                self.cat.put_sequence(seq);
                t = self.apply_pending(
                    t,
                    vec![Pending::NotNull {
                        col: idx,
                        name: None,
                        no_inherit: false,
                        not_valid: false,
                        table_level: false,
                    }],
                    true,
                )?;
                self.identity_to_partitions(&t);
            }
            A::AtDropIdentity => {
                let idx = col_idx(&t, &cmd.name)?;
                if t.columns[idx].identity.is_none() {
                    if cmd.missing_ok {
                        self.notice(format!(
                            "column \"{}\" of relation \"{}\" is not an identity column, skipping",
                            cmd.name, t.name
                        ));
                        return Ok(t);
                    }
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "column \"{}\" of relation \"{}\" is not an identity column",
                            cmd.name, t.name
                        ),
                    ));
                }
                t.columns[idx].identity = None;
                if let Some(s) = t.columns[idx].seq.take()
                    && let Some(seq) = self.cat.sequences.remove(&s)
                {
                    self.cat.remove_name(seq.schema, &seq.name);
                }
                self.identity_to_partitions(&t);
            }
            A::AtSetIdentity => {
                let idx = col_idx(&t, &cmd.name)?;
                let Some(seq_oid) = t.columns[idx].seq else {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "column \"{}\" of relation \"{}\" is not an identity column",
                            cmd.name, t.name
                        ),
                    ));
                };
                let opts: Vec<pb::Node> = match cmd.def.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => l.items.clone(),
                    _ => Vec::new(),
                };
                for d in def_elems(&opts) {
                    if d.defname == "generated" {
                        let v = ast::def_arg_int(d).unwrap_or(0);
                        t.columns[idx].identity = Some(if v == 'a' as i64 {
                            Identity::Always
                        } else {
                            Identity::ByDefault
                        });
                    }
                }
                self.identity_to_partitions(&t);
                let rest: Vec<pb::Node> = opts.into_iter().filter(|n| !matches!(inner(n), Some(NodeEnum::DefElem(d)) if d.defname == "generated")).collect();
                if !rest.is_empty()
                    && let Some(seq) = self.cat.sequences.get(&seq_oid).cloned()
                {
                    let mut sq = (*seq).clone();
                    let restart = self.apply_seq_options(&mut sq, &rest, false)?;
                    // AlterSequence rewrites it into a new file.
                    self.cat.new_filenode(sq.oid);
                    self.cat.put_sequence(sq.clone());
                    if let Some(r) = restart {
                        self.ctx.sess.db.reset_seq(sq.oid, Some(r))?;
                    }
                }
            }
            A::AtSetExpression | A::AtDropExpression => {
                let idx = col_idx(&t, &cmd.name)?;
                let c = &t.columns[idx];
                let virtual_detail = format!(
                    "Column \"{}\" of relation \"{}\" is a virtual generated column.",
                    cmd.name, t.name
                );
                let not_generated = format!(
                    "column \"{}\" of relation \"{}\" is not a generated column",
                    cmd.name, t.name
                );
                if A::try_from(cmd.subtype) == Ok(A::AtDropExpression) {
                    if c.virtual_gen {
                        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "ALTER TABLE / DROP EXPRESSION is not supported for virtual generated columns").detail(virtual_detail));
                    }
                    if c.generated.is_none() {
                        if cmd.missing_ok {
                            self.notice(format!("{not_generated}, skipping"));
                            return Ok(t);
                        }
                        return Err(Error::new(
                            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            not_generated,
                        ));
                    }
                    t.columns[idx].generated = None;
                } else {
                    if c.generated.is_none() {
                        return Err(Error::new(
                            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            not_generated,
                        ));
                    }
                    if c.virtual_gen
                        && t.constraints
                            .iter()
                            .any(|c| matches!(c.kind, ConstraintKind::Check { .. }))
                    {
                        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in tables with check constraints")
                            .detail(virtual_detail));
                    }
                    // GetRelationPublications: publications naming the table.
                    if c.virtual_gen
                        && self
                            .cat
                            .publications
                            .values()
                            .any(|p| p.rels.iter().any(|r| r.rel == t.oid))
                    {
                        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in tables that are part of a publication")
                            .detail(virtual_detail));
                    }
                    let e = cmd.def.as_deref().unwrap();
                    let old = t.clone();
                    t.columns[idx].generated = Some(ast::deparse_expr(e)?);
                    self.check_generated(&t, idx, &ast::deparse_expr(e)?, None)?;
                    t.columns[idx].def_oid = self.cat.alloc_oid();
                    if !t.columns[idx].virtual_gen {
                        // A rewrite can't convert rows stored in other tables' columns.
                        if let Some((tn, c)) = self.column_using(t.rowtype) {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "cannot alter table \"{}\" because column \"{tn}.{c}\" uses its row type",
                                    t.name
                                ),
                            ));
                        }
                        self.rewrite_later(t.oid, 4);
                    }
                    let nt = Arc::new(t.clone());
                    let rules = crate::sql::binder::table_rules(&nt, self.ctx)?;
                    let g = rules.generated[idx].clone().unwrap();
                    self.rewrite(&old, &t, |mut r| {
                        r[idx] = eval(&g, &r, self.ctx)?;
                        Ok(r)
                    })?;
                    if t.columns[idx].virtual_gen && t.columns[idx].not_null {
                        self.check_not_null(&t, idx)?;
                    }
                    // The constraints on the column are rebuilt, so checked again
                    // (RememberAllDependentForRebuilding).
                    for c in t.constraints.clone() {
                        if !c.validated || !c.enforced {
                            continue;
                        }
                        match &c.kind {
                            ConstraintKind::Check { expr, .. } => {
                                let e = self.bind_check(&t, expr)?;
                                if e.columns().contains(&idx) {
                                    self.validate_check(&t, &c.name, &e)?;
                                }
                            }
                            ConstraintKind::ForeignKey {
                                columns,
                                ref_table,
                                ref_columns,
                                match_full,
                                ..
                            } if columns.contains(&idx) => {
                                if let Some(parent) = self.cat.tables.get(ref_table).cloned() {
                                    self.validate_fk(
                                        &t,
                                        &parent,
                                        columns,
                                        ref_columns,
                                        &c.name,
                                        *match_full,
                                        c.is_period(),
                                    )?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            A::AtChangeOwner => {
                if let Some(o) = &cmd.newowner {
                    let new = self.role_oid(o)?;
                    self.change_rel_owner(t.oid, t.owner, t.schema, "table", &t.name, new)?;
                    // Sequences the table owns follow it.
                    for q in self
                        .cat
                        .sequences
                        .values()
                        .filter(|q| q.owned_by.is_some_and(|(o, _)| o == t.oid))
                        .map(|q| q.oid)
                        .collect::<Vec<_>>()
                    {
                        let seq = self.cat.sequences[&q].clone();
                        self.reown_acls(crate::catalog::acl::ObjKind::Sequence, q, seq.owner, new);
                        self.cat.sequences.insert(
                            q,
                            Arc::new(crate::catalog::Sequence {
                                owner: new,
                                ..(*seq).clone()
                            }),
                        );
                    }
                    t.owner = new;
                }
            }
            A::AtSetLogged | A::AtSetUnLogged if t.partkey.is_some() => {
                let what = if A::try_from(cmd.subtype) == Ok(A::AtSetLogged) {
                    "SET LOGGED"
                } else {
                    "SET UNLOGGED"
                };
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "ALTER action {what} cannot be performed on relation \"{}\"",
                        t.name
                    ),
                )
                .detail("This operation is not supported for partitioned tables."));
            }
            A::AtSetLogged => {
                if t.unlogged {
                    // ATPrepChangePersistence: a permanent table can't reference an
                    // unlogged one (its keys in name order).
                    let mut fks: Vec<&Constraint> = t
                        .constraints
                        .iter()
                        .filter(|c| matches!(c.kind, ConstraintKind::ForeignKey { .. }))
                        .collect();
                    fks.sort_by(|a, b| a.name.cmp(&b.name));
                    for c in fks {
                        if let ConstraintKind::ForeignKey { ref_table, .. } = c.kind
                            && ref_table != t.oid
                            && let Some(r) = self.cat.tables.get(&ref_table)
                            && (r.unlogged || r.temporary)
                        {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "could not change table \"{}\" to logged because it references unlogged table \"{}\"",
                                    t.name, r.name
                                ),
                            )
                            .table(self.cat.schema_name(t.schema), &t.name)
                            .constraint(&c.name));
                        }
                    }
                    self.rewrite_later(t.oid, 1);
                }
                t.unlogged = false;
            }
            A::AtSetUnLogged => {
                if !t.unlogged {
                    // ... nor can a permanent table referencing this one stay so.
                    let mut refs: Vec<(u32, String, String)> = self
                        .cat
                        .tables
                        .values()
                        .filter(|o| o.oid != t.oid && !o.unlogged && !o.temporary)
                        .flat_map(|o| {
                            o.constraints.iter().filter_map(move |c| match c.kind {
                                ConstraintKind::ForeignKey { ref_table, .. }
                                    if ref_table == t.oid =>
                                {
                                    Some((c.oid, o.name.clone(), c.name.clone()))
                                }
                                _ => None,
                            })
                        })
                        .collect();
                    refs.sort();
                    if let Some((_, other, conname)) = refs.first() {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            format!(
                                "could not change table \"{}\" to unlogged because it references logged table \"{other}\"",
                                t.name
                            ),
                        )
                        .table(self.cat.schema_name(t.schema), &t.name)
                        .constraint(conname));
                    }
                    self.rewrite_later(t.oid, 1);
                }
                t.unlogged = true;
            }
            A::AtSetRelOptions | A::AtResetRelOptions | A::AtReplaceRelOptions => {
                let opts: Vec<pb::Node> = match cmd.def.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => l.items.clone(),
                    _ => Vec::new(),
                };
                let sub = A::try_from(cmd.subtype);
                let base = if sub == Ok(A::AtReplaceRelOptions) {
                    Vec::new()
                } else {
                    t.options.clone()
                };
                let named = def_elems(&opts)
                    .any(|d| d.defnamespace.is_empty() && d.defname == crate::catalog::PK_CLUSTERED);
                let opts = heap_options(&opts, &base, sub == Ok(A::AtResetRelOptions))?;
                if t.partkey.is_some() && partitioned_refuses(&opts) {
                    return Err(partitioned_options());
                }
                if t.is_foreign() && names_pk_clustered(&opts) {
                    return Err(pk_clustered_unusable(&t, "is a foreign table"));
                }
                t = self.pk_clustered_change(t, opts, named)?;
            }
            A::AtEnableTrig => self.enable_trigger(&mut t, Some(&cmd.name), false, 'O')?,
            A::AtEnableRule => self.set_rule_enabled(t.oid, &cmd.name, 'O')?,
            A::AtEnableAlwaysRule => self.set_rule_enabled(t.oid, &cmd.name, 'A')?,
            A::AtEnableReplicaRule => self.set_rule_enabled(t.oid, &cmd.name, 'R')?,
            A::AtDisableRule => self.set_rule_enabled(t.oid, &cmd.name, 'D')?,
            A::AtEnableAlwaysTrig => self.enable_trigger(&mut t, Some(&cmd.name), false, 'A')?,
            A::AtEnableReplicaTrig => self.enable_trigger(&mut t, Some(&cmd.name), false, 'R')?,
            A::AtDisableTrig => self.enable_trigger(&mut t, Some(&cmd.name), false, 'D')?,
            A::AtEnableTrigAll => self.enable_trigger(&mut t, None, true, 'O')?,
            A::AtEnableTrigUser => self.enable_trigger(&mut t, None, false, 'O')?,
            A::AtDisableTrigAll => self.enable_trigger(&mut t, None, true, 'D')?,
            A::AtDisableTrigUser => self.enable_trigger(&mut t, None, false, 'D')?,
            A::AtEnableRowSecurity => t.rls = true,
            A::AtDisableRowSecurity => t.rls = false,
            A::AtForceRowSecurity => t.rls_force = true,
            A::AtNoForceRowSecurity => t.rls_force = false,
            A::AtClusterOn => self.cluster_on(&t, Some(&cmd.name))?,
            A::AtDropCluster => self.cluster_on(&t, None)?,
            A::AtSetStatistics
            | A::AtSetOptions
            | A::AtResetOptions
            | A::AtSetStorage
            | A::AtSetCompression => {
                if cmd.name.is_empty() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot refer to non-index column by number",
                    ));
                }
                if crate::exec::scan::SYS_COLS
                    .iter()
                    .any(|(n, _)| *n == cmd.name)
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("cannot alter system column \"{}\"", cmd.name),
                    ));
                }
                let i = col_idx(&t, &cmd.name)?;
                let c = &mut t.columns[i];
                match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
                    A::AtSetStatistics => {
                        if c.virtual_gen {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "cannot alter statistics on virtual generated column \"{}\"",
                                    c.name
                                ),
                            ));
                        }
                        c.stattarget = self.stat_target(cmd.def.as_deref())?
                    }
                    A::AtSetStorage => {
                        c.storage = storage_kind(
                            cmd.def.as_deref().and_then(ast::sval).unwrap_or(""),
                            &c.ty,
                        )?
                    }
                    A::AtSetCompression => {
                        let name = cmd
                            .def
                            .as_deref()
                            .and_then(ast::sval)
                            .unwrap_or("")
                            .to_string();
                        if c.ty.typlen() != -1 {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "column data type {} does not support compression",
                                    c.ty.sql_name()
                                ),
                            ));
                        }
                        c.compression = compression_method(&name)?;
                    }
                    sub => {
                        let items: Vec<pb::Node> = match cmd.def.as_deref().and_then(inner) {
                            Some(NodeEnum::List(l)) => l.items.clone(),
                            _ => Vec::new(),
                        };
                        c.options = attribute_options(
                            &items,
                            std::mem::take(&mut c.options),
                            sub == A::AtResetOptions,
                        )?;
                    }
                }
            }
            A::AtReplicaIdentity => {
                let Some(NodeEnum::ReplicaIdentityStmt(r)) = cmd.def.as_deref().and_then(inner)
                else {
                    return Ok(t);
                };
                let chosen = if r.identity_type == "i" {
                    Some(self.replica_index(&t, &r.name)?)
                } else {
                    None
                };
                t.replident = match r.identity_type.as_str() {
                    "d" => None,
                    k => k.chars().next(),
                };
                for i in t.indexes.iter().chain(&t.pkey_index) {
                    if let Some(x) = self.cat.indexes.get(i)
                        && x.replident != (Some(*i) == chosen)
                    {
                        let x = crate::catalog::Index {
                            replident: Some(*i) == chosen,
                            ..(**x).clone()
                        };
                        self.cat.put_index(x);
                    }
                }
            }
            A::AtSetAccessMethod => {
                // DEFAULT: the default access method, or none for a partitioned table.
                let am = match cmd.name.as_str() {
                    "" if t.partkey.is_some() => None,
                    "" => Some(self.default_table_am()?),
                    n => Some(self.table_am(n)?),
                };
                match am {
                    Some(a) if a != 2 || t.partkey.is_some() => {
                        self.cat.rel_am.insert(t.oid, a);
                    }
                    _ => {
                        self.cat.rel_am.remove(&t.oid);
                    }
                }
            }
            A::AtSetTableSpace => {
                let before = self.cat.rel_tablespace.get(&t.oid).copied();
                self.move_to_tablespace(t.oid, &cmd.name)?;
                // (ATExecSetTableSpace: a new relfilenode, RelationAssumeNewRelfilelocator.)
                if t.partkey.is_none() && self.cat.rel_tablespace.get(&t.oid).copied() != before {
                    self.ctx.txn.borrow_mut().mark_new_storage(t.oid);
                }
                crate::exec::pgstat::io_rewrite(self.ctx, t.oid);
            }
            A::AtAlterConstraint => {
                let Some(NodeEnum::AtalterConstraint(ac)) = cmd.def.as_deref().and_then(inner)
                else {
                    return Ok(t);
                };
                self.alter_constraint(&mut t, ac)?;
            }
            A::AtAttachPartition | A::AtDetachPartition => {
                let Some(NodeEnum::PartitionCmd(pc)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                t = if A::try_from(cmd.subtype) == Ok(A::AtAttachPartition) {
                    self.attach_partition(t, pc)?
                } else {
                    self.detach_partition(t, pc)?
                };
            }
            A::AtDetachPartitionFinalize => {
                let Some(NodeEnum::PartitionCmd(pc)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                t = self.detach_partition_finalize(t, pc)?;
            }
            A::AtAddInherit | A::AtDropInherit => {
                let Some(NodeEnum::RangeVar(prv)) = cmd.def.as_deref().and_then(inner) else {
                    return Ok(t);
                };
                if t.partbound.is_some() {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "cannot change inheritance of a partition",
                    ));
                }
                if t.partkey.is_some() {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "cannot change inheritance of partitioned table",
                    ));
                }
                let parent = self.table(prv)?;
                if A::try_from(cmd.subtype) == Ok(A::AtAddInherit) {
                    if parent.partkey.is_some() {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("cannot inherit from partitioned table \"{}\"", parent.name),
                        ));
                    }
                    if parent.partbound.is_some() {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            "cannot inherit from a partition".to_string(),
                        ));
                    }
                }
                t = if A::try_from(cmd.subtype) == Ok(A::AtAddInherit) {
                    self.add_inherit(t, &parent)?
                } else {
                    self.drop_inherit(t, &parent)?
                };
            }
            // SET WITHOUT OIDS: tables have no OIDs any more.
            A::AtDropOids => {}
            A::AtGenericOptions => {
                let opts = match cmd.def.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => l.items.clone(),
                    _ => Vec::new(),
                };
                self.foreign_table_options(&mut t, &opts)?;
            }
            A::AtAlterColumnGenericOptions => {
                let opts = match cmd.def.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => l.items.clone(),
                    _ => Vec::new(),
                };
                self.foreign_column_options(&mut t, &cmd.name, &opts)?;
            }
            _ => return Err(crate::error::unsupported("this ALTER TABLE command")),
        }
        Ok(t)
    }

    /// After ADD COLUMN: existing rows get the default (stored as the column's "missing"
    /// value when it's a constant, otherwise computed per row).
    fn fill_new_column(&mut self, old: &Table, t: &mut Table, idx: usize) -> Result<()> {
        // A serial or identity column: each row takes the next value of the sequence this
        // statement made (which name resolution can't see until the statement ends).
        if let Some(seq) = t.columns[idx]
            .seq
            .and_then(|o| self.cat.sequences.get(&o).cloned())
        {
            self.rewrite_later(t.oid, 2);
            if self.has_rows(old)? {
                let ty = t.columns[idx].ty.clone();
                let ctx = self.ctx;
                self.rewrite(old, t, |mut r| {
                    let v = ctx.sess.db.nextval(&seq)?;
                    ctx.sess.currval.borrow_mut().insert(seq.oid, v);
                    ctx.sess.lastval.set(Some(v));
                    ctx.sess.lastval_seq.set(seq.oid);
                    r.resize(idx + 1, Value::Null);
                    r[idx] = crate::types::cast::cast_value(
                        Value::Int8(v),
                        &DataType::Int8,
                        &ty,
                        ctx.fmt(),
                        false,
                    )?;
                    Ok(r)
                })?;
            }
            return Ok(());
        }
        // Defaults reading sequences made by earlier commands of this statement can't be
        // bound yet (and only this column's default is needed).
        let known = self.ctx.catalog();
        let mut bindable = t.clone();
        for c in &mut bindable.columns {
            if c.seq.is_some_and(|o| !known.sequences.contains_key(&o)) {
                c.default = None;
            }
        }
        let nt = Arc::new(bindable);
        let rules = crate::sql::binder::table_rules(&nt, self.ctx)?;
        let has_rows = self.has_rows(old)?;
        // Postgres rewrites the table (rows or not) unless the value can be kept as the
        // column's missing value: for a stored generated column, a volatile default, or a
        // domain with constraints.
        let col = &t.columns[idx];
        let domain_checked = col
            .domain
            .and_then(|d| self.cat.domains.get(&d))
            .is_some_and(|d| {
                let (not_null, checks) = self.cat.domain_constraints(d);
                not_null || !checks.is_empty()
            });
        if t.partkey.is_none()
            && (col.generated.is_some() && !col.virtual_gen
                || col.identity.is_some()
                || col.seq.is_some()
                || rules.defaults[idx]
                    .as_ref()
                    .is_some_and(|d| d.is_volatile())
                || domain_checked)
        {
            self.rewrite_later(t.oid, 2);
        }
        if let Some(g) = rules.generated[idx].clone() {
            if has_rows {
                self.rewrite(old, t, |mut r| {
                    r.resize(idx + 1, Value::Null);
                    r[idx] = eval(&g, &r, self.ctx)?;
                    Ok(r)
                })?;
            }
            return Ok(());
        }
        let Some(d) = rules.defaults[idx].clone() else {
            if t.columns[idx].not_null && has_rows {
                return Err(Error::new(
                    code::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        t.columns[idx].name, t.name
                    ),
                )
                .table(self.cat.schema_name(t.schema), &t.name)
                .column(&t.columns[idx].name));
            }
            return Ok(());
        };
        if !d.is_volatile() {
            let v = eval(&d, &[], self.ctx)?;
            if v.is_null() && t.columns[idx].not_null && has_rows {
                return Err(Error::new(
                    code::NOT_NULL_VIOLATION,
                    format!(
                        "column \"{}\" of relation \"{}\" contains null values",
                        t.columns[idx].name, t.name
                    ),
                )
                .table(self.cat.schema_name(t.schema), &t.name)
                .column(&t.columns[idx].name));
            }
            t.columns[idx].missing = if v.is_null() { None } else { Some(v) };
            return Ok(());
        }
        if has_rows {
            self.rewrite(old, t, |mut r| {
                r.resize(idx + 1, Value::Null);
                r[idx] = eval(&d, &[], self.ctx)?;
                Ok(r)
            })?;
        }
        Ok(())
    }

    /// ALTER TABLE DROP COLUMN: the column, the inheriting children's copies (unless ONLY or
    /// defined elsewhere too), and whatever depends on them, in one deletion.
    pub(super) fn drop_column(&mut self, t: Table, idx: usize, cascade: bool) -> Result<Table> {
        let mut roots = Vec::new();
        self.child_column_roots(t.oid, &t.columns[idx].name.clone(), !self.only, &mut roots);
        roots.push((crate::catalog::depend::REL, t.oid, idx as i32 + 1));
        let oid = t.oid;
        self.cat.put_table(t);
        self.drop_objects(&roots, cascade)?;
        self.cat
            .tables
            .get(&oid)
            .map(|t| (**t).clone())
            .ok_or_else(|| crate::error::internal("table"))
    }

    /// `ATExecDropColumn` recursing: children's columns of the name that go too (theirs
    /// first); the others stop inheriting it.
    fn child_column_roots(
        &mut self,
        parent: u32,
        name: &str,
        recurse: bool,
        roots: &mut Vec<crate::catalog::depend::Addr>,
    ) {
        for mut child in self.child_tables(parent) {
            let Some(i) = child.column_index(name) else {
                continue;
            };
            let c = &mut child.columns[i];
            if recurse && c.inhcount == 1 && !c.islocal {
                self.child_column_roots(child.oid, name, true, roots);
                roots.push((crate::catalog::depend::REL, child.oid, i as i32 + 1));
                continue;
            }
            c.inhcount = c.inhcount.saturating_sub(1);
            if !recurse {
                c.islocal = true;
            }
            self.cat.put_table(child);
        }
    }

    /// Remove column `idx` itself (its dependents are gone): its indexes, constraints,
    /// statistics and owned sequence go with it.
    pub(super) fn remove_column(&mut self, mut t: Table, idx: usize) -> Result<Table> {
        let name = t.columns[idx].name.clone();
        // Statistics objects on it go silently.
        self.drop_column_statistics(&t, idx);
        // Indexes and constraints over the column go with it.
        let uses = |sql: &str| view_mentions(sql, &name);
        let mut drop_indexes = Vec::new();
        for ix in self
            .cat
            .table_indexes(&t)
            .into_iter()
            .chain(t.pkey_index.and_then(|p| self.cat.index(p).cloned()))
        {
            let hit = ix.columns.iter().any(|c| match &c.key {
                IndexKey::Column(i) => *i == idx,
                IndexKey::Expr(e) => uses(e),
            }) || ix.include.contains(&idx)
                || ix.predicate.as_deref().is_some_and(uses);
            if hit {
                drop_indexes.push(ix.oid);
            }
        }
        let old = t.clone();
        let mut pk_dropped = false;
        for oid in &drop_indexes {
            if t.pkey_index == Some(*oid) {
                t.pkey = None;
                t.pkey_index = None;
                // (Rows under the rowid already stay there.)
                pk_dropped = !t.pkey_unkeyed;
                t.pkey_unkeyed = false;
            }
            t.indexes.retain(|i| i != oid);
            if let Some(ix) = self.cat.indexes.remove(oid) {
                self.cat.remove_name(t.schema, &ix.name);
                self.delete_index_entries(&old, &ix)?;
            }
        }
        t.constraints.retain(|c| match &c.kind {
            ConstraintKind::PrimaryKey { index }
            | ConstraintKind::Unique { index }
            | ConstraintKind::Exclusion { index, .. } => !drop_indexes.contains(index),
            ConstraintKind::Check { expr, .. } => !uses(expr),
            ConstraintKind::ForeignKey { columns, .. } => !columns.contains(&idx),
            ConstraintKind::NotNull { column } => *column != idx,
        });
        // Owned sequence.
        if let Some(s) = t.columns[idx].seq
            && let Some(seq) = self.cat.sequences.remove(&s)
        {
            self.cat.remove_name(seq.schema, &seq.name);
        }
        let c = &mut t.columns[idx];
        c.dropped = true;
        c.name = format!("........pg.dropped.{}........", idx + 1);
        c.not_null = false;
        c.default = None;
        c.generated = None;
        c.identity = None;
        c.seq = None;
        if pk_dropped {
            // Re-key by rowid.
            self.pk_clustered_dropped(&mut t);
            self.relayout(&old, &t)?;
        }
        Ok(t)
    }

    fn delete_index_entries(&self, t: &Table, ix: &Index) -> Result<()> {
        if ix.id == 0 {
            return Ok(());
        }
        let start = crate::storage::keys::index_prefix(t.oid, ix.id);
        let end = crate::storage::keys::prefix_end(&start);
        let keys: Vec<Vec<u8>> = self
            .ctx
            .txn
            .borrow()
            .scan(&start, &end)
            .map(|e| e.map(|(k, _)| k))
            .collect::<Result<_>>()?;
        let mut txn = self.ctx.txn.borrow_mut();
        for k in keys {
            txn.delete(k)?;
        }
        Ok(())
    }

    pub(super) fn alter_column_type(
        &mut self,
        mut t: Table,
        idx: usize,
        cd: &pb::ColumnDef,
    ) -> Result<Table> {
        let tn = cd.type_name.as_ref().unwrap();
        let (ty, domain) = resolve_type_name(&self.cat, self.search_path(), tn)?;
        check_not_self_member(&t, &ty)?;
        // The column's collation: COLLATE's, else the new type's (GetColumnDefCollation).
        let collation = match &cd.coll_clause {
            Some(cc) => {
                let cn = names(&cc.collname);
                if crate::types::collation::resolve(&self.cat, self.search_path(), &cn).is_none() {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!(
                                "collation \"{}\" for encoding \"UTF8\" does not exist",
                                cn.join(".")
                            ),
                        ),
                        cc.location,
                    ));
                }
                if !ty.collatable() {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!("collations are not supported by type {}", ty.display()),
                        ),
                        cc.location,
                    ));
                }
                cn.last().cloned()
            }
            None => domain
                .and_then(|d| self.cat.domains.get(&d))
                .and_then(|d| d.collation.clone()),
        };
        let old = t.clone();
        let from = t.columns[idx].ty.clone();
        let colname = t.columns[idx].name.clone();
        // An identity column's sequence follows its type (ALTER SEQUENCE ... AS).
        if t.columns[idx].identity.is_some() {
            if !matches!(ty, DataType::Int2 | DataType::Int4 | DataType::Int8) {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    "identity column type must be smallint, integer, or bigint",
                ));
            }
            if let Some(sq) = t.columns[idx]
                .seq
                .and_then(|s| self.cat.sequences.get(&s).cloned())
                && sq.ty != ty
            {
                let mut q = (*sq).clone();
                let (old_max, new_max) = (type_max(&q.ty), type_max(&ty));
                if q.max == old_max {
                    q.max = new_max;
                }
                if q.min == -old_max - 1 {
                    q.min = -new_max - 1;
                }
                q.ty = ty.clone();
                self.cat.put_sequence(q);
            }
        }
        if t.columns[idx].generated.is_some() && cd.raw_default.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot alter type of a column used by a generated column",
            ));
        }
        if let Some(g) = t.columns.iter().enumerate().find(|(j, c)| {
            *j != idx
                && !c.dropped
                && c.generated
                    .as_deref()
                    .is_some_and(|g| view_mentions(g, &colname))
        }) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot alter type of a column used by a generated column",
            )
            .detail(format!(
                "Column \"{colname}\" is used by generated column \"{}\".",
                g.1.name
            )));
        }
        self.column_type_dependents(t.oid, idx, &colname)?;
        // A foreign table has no rows to convert; its row type mustn't be used (for other
        // tables that is checked when they are rewritten).
        // Conversion: USING expression over the old row, or an assignment cast. (With several
        // in the statement, over the row as the statement found it.)
        let ot = Arc::new(old.clone());
        let row_of = match &self.combined_types {
            Some((orig, _)) => orig.clone(),
            None => ot.clone(),
        };
        let from_type = row_of.columns[idx].ty.clone();
        let mut b = self.binder();
        let conv = match cd.raw_default.as_deref() {
            _ if t.is_foreign() => Expr::Col(idx, from.clone()),
            Some(using) => {
                // ATPrepAlterColumnType: a whole-row reference to the table can't convert.
                let whole_row = using.node.as_ref().is_some_and(|n| {
                    n.nodes().iter().any(|(x, ..)| match x {
                        pg_query::NodeRef::ColumnRef(c) => {
                            let f: Vec<&str> = c.fields.iter().filter_map(ast::sval).collect();
                            matches!(f.as_slice(), [n] if *n == t.name && t.column_index(n).is_none())
                                || matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
                        }
                        _ => false,
                    })
                });
                if whole_row {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot convert whole-row table reference",
                    )
                    .detail("USING expression contains a whole-row table reference."));
                }
                let e = b.bind_over_table(&row_of, using, Clause::Other)?;
                b.coerce_assign(e, &ty, &colname).map_err(|e| {
                    if e.code == code::DATATYPE_MISMATCH {
                        Error::new(code::DATATYPE_MISMATCH, format!("result of USING clause for column \"{colname}\" cannot be cast automatically to type {}", ty.display()))
                            .hint("You might need to add an explicit cast.")
                    } else {
                        e
                    }
                })?
            }
            None => {
                let e = Expr::Col(idx, from_type.clone());
                b.coerce_assign(e, &ty, &colname).map_err(|e| {
                    if e.code == code::DATATYPE_MISMATCH {
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "column \"{colname}\" cannot be cast automatically to type {}",
                                ty.display()
                            ),
                        )
                        .hint(format!(
                            "You might need to specify \"USING {}::{}\".",
                            quote_ident(&colname),
                            ty.display()
                        ))
                    } else {
                        e
                    }
                })?
            }
        };
        // find_composite_type_dependencies (for a table, when ATRewriteTables would convert
        // its rows).
        if let Some((tn, c)) = self.column_using(t.rowtype) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot alter {} \"{}\" because column \"{tn}.{c}\" uses its row type",
                    if t.is_foreign() { "foreign table" } else { "table" },
                    t.name
                ),
            ));
        }
        // What the column's constraints and indexes say is made anew from their definitions
        // as printed under the old type (ATPostAlterTypeParse): the casts it needed stay.
        let reprint = |d: &Self, sql: &str, kind: crate::sql::ruleutils::Stored| -> Option<String> {
            let node = ast::parse_expr(sql).ok()?;
            let mut b = d.binder();
            b.deparse = true;
            let e = b.bind_over_table(&ot, &node, Clause::Check).ok()?;
            if !e.columns().contains(&idx) {
                return None;
            }
            crate::sql::ruleutils::stored_expr(d.ctx, &ot, sql, kind, false)
        };
        for i in 0..t.constraints.len() {
            if let ConstraintKind::Check { expr, .. } = &t.constraints[i].kind
                && let Some(new) = reprint(self, expr, crate::sql::ruleutils::Stored::Condition)
                && let ConstraintKind::Check { expr, .. } = &mut t.constraints[i].kind
            {
                *expr = new;
                // Made anew, it merges again with a child's own same-named constraint
                // (which outlived the drop).
                let c = &t.constraints[i];
                if c.islocal && !c.no_inherit {
                    for ch in self.cat.children(t.oid).to_vec() {
                        if self.cat.tables.get(&ch).is_some_and(|x| {
                            x.constraints.iter().any(|k| {
                                k.name == c.name
                                    && k.islocal
                                    && k.inhcount > 0
                                    && matches!(k.kind, ConstraintKind::Check { .. })
                            })
                        }) {
                            self.merge_notices.push(format!(
                                "merging constraint \"{}\" with inherited definition",
                                c.name
                            ));
                        }
                    }
                }
            }
        }
        for ixo in t.indexes.clone() {
            let Some(ix) = self.cat.indexes.get(&ixo).cloned() else {
                continue;
            };
            let mut nix = (*ix).clone();
            let mut changed = false;
            for c in &mut nix.columns {
                if let IndexKey::Expr(e) = &mut c.key
                    && let Some(new) = reprint(self, e, crate::sql::ruleutils::Stored::Plain)
                {
                    *e = new;
                    changed = true;
                }
            }
            if let Some(p) = nix.predicate.clone()
                && let Some(new) = reprint(self, &p, crate::sql::ruleutils::Stored::Condition)
            {
                nix.predicate = Some(new);
                changed = true;
            }
            if changed {
                self.cat.put_index(nix);
            }
        }
        t.columns[idx].ty = ty.clone();
        t.columns[idx].domain = domain;
        t.columns[idx].collation = collation;
        t.columns[idx].ndims = Some(tn.array_bounds.len() as i16);
        t.columns[idx].missing = None;
        // The default must still convert; it is stored anew.
        if let Some(d) = t.columns[idx].default.clone() {
            let n = ast::parse_expr(&d)?;
            let e = b.bind_scalar(&n, Clause::Default)?;
            b.coerce_assign(e, &ty, &colname).map_err(|_| {
                Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "default for column \"{colname}\" cannot be cast automatically to type {}",
                        ty.display()
                    ),
                )
            })?;
            t.columns[idx].def_oid = self.cat.alloc_oid();
        }
        // ATPostAlterTypeCleanup makes anew what depends on the column, taking new oids (in
        // the later passes: flush_type_rebuilds); the table is rewritten unless the new type
        // is binary compatible.
        self.remember_type_rebuilds(&old, idx);
        let domain_checked = domain
            .and_then(|d| self.cat.domains.get(&d))
            .is_some_and(|d| {
                let (not_null, checks) = self.cat.domain_constraints(d);
                not_null || !checks.is_empty()
            });
        // timestamp <-> timestamptz keeps the values when the session's zone is always UTC
        // (TimestampTimestampTzRequiresRewrite).
        let utc_swap = matches!(
            (&from, &ty),
            (DataType::Timestamp(_), DataType::Timestamptz(_))
                | (DataType::Timestamptz(_), DataType::Timestamp(_))
        ) && from.typmod() == ty.typmod()
            && crate::types::tz::pg_get_timezone_offset(self.ctx.fmt().tz.pg()) == Some(0);
        // ATColumnChangeRequiresRewrite: a USING that is the column under casts that keep
        // its values (as the planner simplifies them) doesn't rewrite; a virtual generated
        // column has no values stored.
        let using_rewrites = cd.raw_default.is_some() && {
            let mut e = &conv;
            loop {
                match e {
                    Expr::Col(i, _) if *i == idx => break false,
                    Expr::Cast(inner, f, to, _) if !type_change_rewrites(f, to) => e = inner,
                    _ => break true,
                }
            }
        };
        if t.partkey.is_none()
            && !t.is_foreign()
            && !t.columns[idx].virtual_gen
            && (using_rewrites
                || domain_checked
                || !utc_swap && type_change_rewrites(&from, &ty))
        {
            self.rewrite_later(t.oid, 4);
        }
        // Converted with the statement's others, in one rewrite at its end.
        if let Some((_, convs)) = &mut self.combined_types {
            convs.push((idx, conv));
            return Ok(t);
        }
        // Foreign keys must stay type-compatible: re-check by rebuilding them.
        let missing_old = old.columns[idx].missing.clone();
        self.rewrite(&old, &t, |mut r| {
            if r[idx].is_null()
                && let Some(m) = &missing_old
            {
                r[idx] = m.clone();
            }
            r[idx] = eval(&conv, &r, self.ctx)?;
            Ok(r)
        })?;
        // Foreign keys over the column are made anew (ATPostAlterTypeCleanup): their
        // types must still go together.
        let fk_mismatch = |name: &str, ca: &str, cb: &str, a: &DataType, b: &DataType| {
            Error::new(
                code::DATATYPE_MISMATCH,
                format!("foreign key constraint \"{name}\" cannot be implemented"),
            )
            .detail(format!(
                "Key columns \"{ca}\" of the referencing table and \"{cb}\" of the referenced table are of incompatible types: {} and {}.",
                a.without_typmod().display(),
                b.without_typmod().display()
            ))
        };
        for c in &t.constraints {
            if let ConstraintKind::ForeignKey { columns, ref_table, ref_columns, .. } = &c.kind
                && (columns.contains(&idx) || *ref_table == t.oid && ref_columns.contains(&idx))
            {
                let parent = if *ref_table == t.oid {
                    Some(Arc::new(t.clone()))
                } else {
                    self.cat.tables.get(ref_table).cloned()
                };
                if let Some(p) = parent {
                    for (a, r) in columns.iter().zip(ref_columns) {
                        let (ta, tb) = (&t.columns[*a].ty, &p.columns[*r].ty);
                        if !fk_types_compatible(ta, tb) {
                            return Err(fk_mismatch(&c.name, &t.columns[*a].name, &p.columns[*r].name, ta, tb));
                        }
                    }
                }
            }
        }
        for (child, con) in self.cat.referencing_fks(t.oid) {
            if child.oid == t.oid {
                continue;
            }
            if let ConstraintKind::ForeignKey { columns, ref_columns, .. } = &con.kind
                && ref_columns.contains(&idx)
            {
                for (a, r) in columns.iter().zip(ref_columns) {
                    let (ta, tb) = (&child.columns[*a].ty, &t.columns[*r].ty);
                    if !fk_types_compatible(ta, tb) {
                        return Err(fk_mismatch(&con.name, &child.columns[*a].name, &t.columns[*r].name, ta, tb));
                    }
                }
            }
        }
        // Re-validate constraints with the new type (made anew from their text: errors
        // carry no position).
        let nt = Arc::new(t.clone());
        let rules = crate::sql::binder::table_rules(&nt, self.ctx)
            .map_err(|e| Error { position: None, ..e })?;
        for (_, row) in self.table_rows(&t)? {
            crate::exec::dml::check_row(&rules, &row, self.ctx).map_err(|e| {
                if e.code == code::CHECK_VIOLATION {
                    with_failing_row(
                        Error::new(
                            code::CHECK_VIOLATION,
                            format!(
                                "check constraint of relation \"{}\" is violated by some row",
                                t.name
                            ),
                        ),
                        &t,
                        &row,
                        self.ctx,
                    )
                } else {
                    e
                }
            })?;
        }
        Ok(t)
    }

    /// The one rewrite of an ALTER TABLE with several ALTER COLUMN TYPEs: each converted
    /// column computed from the row as stored before the statement (`from`), then the
    /// constraints checked over the new rows.
    fn combined_rewrite(
        &mut self,
        from: &Table,
        t: &Table,
        convs: &[(usize, Expr)],
    ) -> Result<()> {
        let missing: Vec<Option<Value>> = from.columns.iter().map(|c| c.missing.clone()).collect();
        self.rewrite(from, t, |r| {
            let mut src = r.clone();
            for (i, v) in src.iter_mut().enumerate() {
                if v.is_null()
                    && let Some(Some(m)) = missing.get(i)
                {
                    *v = m.clone();
                }
            }
            let mut out = r;
            for (idx, conv) in convs {
                out[*idx] = eval(conv, &src, self.ctx)?;
            }
            Ok(out)
        })?;
        let nt = Arc::new(t.clone());
        let rules = crate::sql::binder::table_rules(&nt, self.ctx)
            .map_err(|e| Error { position: None, ..e })?;
        for (_, row) in self.table_rows(t)? {
            crate::exec::dml::check_row(&rules, &row, self.ctx).map_err(|e| {
                if e.code == code::CHECK_VIOLATION {
                    with_failing_row(
                        Error::new(
                            code::CHECK_VIOLATION,
                            format!(
                                "check constraint of relation \"{}\" is violated by some row",
                                t.name
                            ),
                        ),
                        t,
                        &row,
                        self.ctx,
                    )
                } else {
                    e
                }
            })?;
        }
        Ok(())
    }

    /// ALTER TABLE DROP CONSTRAINT: the constraint and whatever depends on it.
    pub(super) fn drop_constraint(
        &mut self,
        t: Table,
        name: &str,
        missing_ok: bool,
        cascade: bool,
    ) -> Result<Table> {
        let Some(con) = t.constraints.iter().find(|c| c.name == name).cloned() else {
            if missing_ok {
                self.notice(format!(
                    "constraint \"{name}\" of relation \"{}\" does not exist, skipping",
                    t.name
                ));
                return Ok(t);
            }
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "constraint \"{name}\" of relation \"{}\" does not exist",
                    t.name
                ),
            ));
        };
        if let ConstraintKind::NotNull { column } = con.kind {
            not_null_droppable(&self.cat, &t, column)?;
        }
        // (dropconstraint_internal: the referenced table must be safe to alter too, and
        // the partitions whose copies go with it.)
        if let ConstraintKind::ForeignKey { ref_table, .. } = con.kind {
            if ref_table != t.oid
                && let Some(r) = self.cat.tables.get(&ref_table)
            {
                self.not_in_use(r.oid, "ALTER TABLE", &r.name)?;
            }
            let mut under = vec![con.oid];
            let mut i = 0;
            while i < under.len() {
                let p = under[i];
                i += 1;
                for x in self.cat.tables.values() {
                    for k in x.constraints.iter().filter(|k| k.parent == p) {
                        if x.oid != t.oid {
                            self.not_in_use(x.oid, "ALTER TABLE", &x.name)?;
                        }
                        under.push(k.oid);
                    }
                }
            }
        }
        let oid = t.oid;
        self.cat.put_table(t);
        self.drop_objects(&[(crate::catalog::depend::CONSTRAINT, con.oid, 0)], cascade)?;
        self.cat
            .tables
            .get(&oid)
            .map(|t| (**t).clone())
            .ok_or_else(|| crate::error::internal("table"))
    }

    /// Remove constraint `oid` itself (its dependents are gone), with its index.
    pub(super) fn remove_constraint(&mut self, mut t: Table, oid: u32) -> Result<Table> {
        let Some(i) = t.constraints.iter().position(|c| c.oid == oid) else {
            return Ok(t);
        };
        let con = t.constraints.remove(i);
        match con.kind {
            ConstraintKind::PrimaryKey { index }
            | ConstraintKind::Unique { index }
            | ConstraintKind::Exclusion { index, .. } => {
                if let Some(ix) = self.cat.indexes.remove(&index) {
                    let old = t.clone();
                    self.cat.remove_name(t.schema, &ix.name);
                    t.indexes.retain(|i| *i != ix.oid);
                    if t.pkey_index == Some(ix.oid) && !t.pkey_unkeyed {
                        t.pkey = None;
                        t.pkey_index = None;
                        self.pk_clustered_dropped(&mut t);
                        self.relayout(&old, &t)?;
                    } else {
                        if t.pkey_index == Some(ix.oid) {
                            t.pkey = None;
                            t.pkey_index = None;
                            t.pkey_unkeyed = false;
                        }
                        self.delete_index_entries(&t, &ix)?;
                    }
                }
            }
            ConstraintKind::NotNull { column } => t.columns[column].not_null = false,
            _ => {}
        }
        Ok(t)
    }

    // ── RENAME / SET SCHEMA ──

    /// RENAME COLUMN on one table: the column and every stored definition naming it.
    fn rename_table_column(&mut self, mut t: Table, idx: usize, newname: &str) -> Result<()> {
        if t.column_index(newname).is_some() {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                format!(
                    "column \"{newname}\" of relation \"{}\" already exists",
                    t.name
                ),
            ));
        }
        let before = Arc::new(self.cat.clone());
        t.columns[idx].name = newname.to_string();
        let oid = t.oid;
        self.cat.put_table(t);
        self.follow_renames(&before, &[super::stored::Change::Column(oid, idx as i16 + 1)])
    }

    /// ALTER TABLE / INDEX ... RENAME of a TOAST table or its index (`index`) of table `t`:
    /// RangeVarCallbackForAlterRelation's checks (owner, then IsSystemClass, which only
    /// allow_system_table_mods lets through), RenameRelation's lock.
    fn rename_toast(
        &mut self,
        kind: pb::ObjectType,
        t: &Table,
        index: bool,
        name: &str,
        newname: &str,
    ) -> Result<String> {
        use pb::ObjectType as O;
        let tag = format!("ALTER {}", super::object_type_word(kind));
        self.require_owner(t.owner, if index { "index" } else { "table" }, name)?;
        let wrong = |what: &str| {
            Err(Error::new(code::WRONG_OBJECT_TYPE, format!("\"{name}\" is not {what}")))
        };
        match kind {
            O::ObjectView => return wrong("a view"),
            O::ObjectMatview => return wrong("a materialized view"),
            O::ObjectSequence => return wrong("a sequence"),
            O::ObjectForeignTable => return wrong("a foreign table"),
            _ => {}
        }
        if !self.ctx.sess.settings.borrow().bool("allow_system_table_mods") {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let oid = t.toast + index as u32;
        self.note_lock(
            oid,
            if kind == O::ObjectIndex {
                lockmode::SHARE_UPDATE_EXCLUSIVE
            } else {
                lockmode::ACCESS_EXCLUSIVE
            },
        )?;
        let schema = self.cat.toast_schema(t);
        let schema_name = self.cat.schema_name(schema).to_string();
        if self.cat.name_taken(schema, newname)
            || self.cat.find_toast(Some(&schema_name), newname, &[]).is_some()
        {
            return Err(Error::new(
                code::DUPLICATE_TABLE,
                format!("relation \"{newname}\" already exists"),
            ));
        }
        let Some(cur) = self.cat.index_table(t.oid) else {
            return Ok(tag);
        };
        let mut cur = (*cur).clone();
        let given = (newname != crate::catalog::toast_name(cur.oid, index)).then(|| newname.to_string());
        if index {
            cur.toast_indname = given;
        } else {
            cur.toast_relname = given;
        }
        self.cat.put_storage(cur);
        self.save()?;
        Ok(tag)
    }

    pub(super) fn rename(&mut self, s: &pb::RenameStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.rename_type).unwrap_or(O::ObjectTable);
        if let Some(k) = super::tsearch::ts_kind(kind) {
            return self.rename_ts(k, s);
        }
        let newname = crate::catalog::truncate_ident(&s.newname);
        match kind {
            O::ObjectTable
            | O::ObjectView
            | O::ObjectMatview
            | O::ObjectSequence
            | O::ObjectIndex
            | O::ObjectForeignTable => {
                let rv = s.relation.as_ref().unwrap();
                let Some(rel) = self.lookup(rv)? else {
                    if let Some((t, index)) =
                        self.cat.find_toast(Self::rv_schema(rv), &rv.relname, self.search_path())
                    {
                        return self.rename_toast(kind, &t, index, &rv.relname, &newname);
                    }
                    if s.missing_ok {
                        self.notice(format!(
                            "relation \"{}\" does not exist, skipping",
                            rv.relname
                        ));
                        return Ok(format!("ALTER {}", super::object_type_word(kind)));
                    }
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                };
                self.require_rel_owner(rel.oid(), rel.kind_name(), &rv.relname)?;
                foreign_kind_check(&rel, kind, s.relation_type, &rv.relname)?;
                let schema = match &rel {
                    Relation::Table(t) => t.schema,
                    Relation::View(v) => v.schema,
                    Relation::Sequence(q) => q.schema,
                    Relation::Index(i) => self
                        .cat
                        .index_table(i.table)
                        .map(|t| t.schema)
                        .unwrap_or(crate::catalog::PUBLIC_OID),
                };
                if self.cat.name_taken(schema, &newname) {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!("relation \"{newname}\" already exists"),
                    ));
                }
                // RenameRelation's lock: an index renamed by ALTER INDEX takes SHARE UPDATE
                // EXCLUSIVE.
                self.note_lock(
                    rel.oid(),
                    if kind == O::ObjectIndex && matches!(rel, Relation::Index(_)) {
                        lockmode::SHARE_UPDATE_EXCLUSIVE
                    } else {
                        lockmode::ACCESS_EXCLUSIVE
                    },
                )?;
                let before = Arc::new(self.cat.clone());
                let renamed = (!matches!(rel, Relation::Index(_))).then(|| rel.oid());
                match rel {
                    Relation::Table(t) => {
                        let mut t = (*t).clone();
                        // (RenameTypeInternal: an array type called that moves aside; the
                        // row type's own array type is named anew.)
                        self.cat.move_array_type_name(t.schema, &newname, t.array_type());
                        self.cat.remove_name(t.schema, &t.name);
                        t.name = newname.clone();
                        let (schema, array) = (t.schema, t.array_type());
                        self.cat.put_table(t);
                        self.cat.rename_array_type(schema, array, &newname);
                    }
                    Relation::View(v) => {
                        let mut v = (*v).clone();
                        self.cat.move_array_type_name(v.schema, &newname, v.array_type());
                        self.cat.remove_name(v.schema, &v.name);
                        v.name = newname.clone();
                        let (schema, array) = (v.schema, v.array_type());
                        self.cat.put_view(v);
                        self.cat.rename_array_type(schema, array, &newname);
                    }
                    Relation::Sequence(q) => {
                        let mut q = (*q).clone();
                        self.cat.remove_name(q.schema, &q.name);
                        q.name = newname.clone();
                        self.cat.put_sequence(q.clone());
                        self.rewrite_seq_defaults(q.oid)?;
                    }
                    Relation::Index(i) => {
                        let mut i = (*i).clone();
                        self.cat.remove_name(schema, &i.name);
                        let old = i.name.clone();
                        i.name = newname.clone();
                        // A constraint backed by the index is renamed with it.
                        if let Some(t) = self.cat.index_table(i.table) {
                            let mut t = (*t).clone();
                            for c in &mut t.constraints {
                                if c.name == old && c.kind.index().is_some() {
                                    c.name = newname.clone();
                                }
                            }
                            self.cat.put_storage(t);
                        }
                        self.cat.put_index(i);
                    }
                }
                if let Some(oid) = renamed {
                    self.follow_renames(&before, &[super::stored::Change::Rel(oid)])?;
                }
                self.save()?;
                Ok(format!("ALTER {}", super::object_type_word(kind)))
            }
            O::ObjectCollation => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                self.rename_collation(&parts, &newname)?;
                self.save()?;
                Ok("ALTER COLLATION".into())
            }
            O::ObjectStatisticExt => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let Some(st) = self.statistics_object(&parts, s.missing_ok)? else {
                    return Ok("ALTER STATISTICS".into());
                };
                self.require_owner(st.owner, "statistics object", &st.name)?;
                if self
                    .cat
                    .statistics
                    .values()
                    .any(|x| x.schema == st.schema && x.name == newname)
                {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "statistics object \"{newname}\" already exists in schema \"{}\"",
                            self.cat.schema_name(st.schema)
                        ),
                    ));
                }
                self.require_schema_create(st.schema)?;
                self.cat.statistics.insert(
                    st.oid,
                    Arc::new(crate::catalog::StatExt {
                        name: newname,
                        ..(*st).clone()
                    }),
                );
                self.save()?;
                Ok("ALTER STATISTICS".into())
            }
            O::ObjectAttribute => self.rename_attribute(s, newname),
            O::ObjectColumn => {
                let rv = s.relation.as_ref().unwrap();
                let tag = format!(
                    "ALTER {}",
                    super::object_type_word(O::try_from(s.relation_type).unwrap_or(O::ObjectTable))
                );
                let Some(rel) = self.lookup(rv)? else {
                    if s.missing_ok {
                        self.notice(format!(
                            "relation \"{}\" does not exist, skipping",
                            rv.relname
                        ));
                        return Ok(tag);
                    }
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                };
                self.require_rel_owner(rel.oid(), rel.kind_name(), &rv.relname)?;
                foreign_kind_check(&rel, kind, s.relation_type, &rv.relname)?;
                match rel {
                    Relation::Table(t) => {
                        if t.of_type != 0 && !self.typed_cascade {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                "cannot rename column of typed table",
                            ));
                        }
                        let t = (*t).clone();
                        // (renameatt_internal: without recursion there must be no children,
                        // checked before the column.)
                        let all = self.cat.inheritors(t.oid);
                        if !rv.inh && all.len() > 1 {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "inherited column \"{}\" must be renamed in child tables too",
                                    s.subname
                                ),
                            ));
                        }
                        let idx = t.column_index(&s.subname).ok_or_else(|| {
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!("column \"{}\" does not exist", s.subname),
                            )
                        })?;
                        system_name(&newname)?;
                        if t.columns[idx].inhcount > 0 {
                            return Err(Error::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!("cannot rename inherited column \"{}\"", s.subname),
                            ));
                        }
                        // A child column inherited from outside this tree too can't be
                        // renamed (renameatt_internal's expected_parents).
                        for oid in all.iter().skip(1) {
                            let Some(c) = self.cat.table(*oid) else {
                                continue;
                            };
                            let parents = c.inherits.iter().filter(|p| all.contains(p)).count();
                            if let Some(i) = c.column_index(&s.subname)
                                && c.columns[i].inhcount as usize > parents
                            {
                                return Err(Error::new(
                                    code::INVALID_TABLE_DEFINITION,
                                    format!("cannot rename inherited column \"{}\"", s.subname),
                                ));
                            }
                        }
                        // (renameatt_internal renames the descendants first, then the table.)
                        let me = t.oid;
                        for oid in all.into_iter().skip(1) {
                            let Some(c) = self.cat.table(oid).map(|c| (**c).clone()) else {
                                continue;
                            };
                            if let Some(i) = c.column_index(&s.subname) {
                                self.rename_table_column(c, i, &newname)?;
                            }
                        }
                        let t = self.cat.table(me).map_or(t, |c| (**c).clone());
                        self.rename_table_column(t, idx, &newname)?;
                    }
                    Relation::View(v) => {
                        let mut v = (*v).clone();
                        let Some(i) = v.columns.iter().position(|(n, _)| *n == s.subname) else {
                            return Err(Error::new(
                                code::UNDEFINED_COLUMN,
                                format!("column \"{}\" does not exist", s.subname),
                            ));
                        };
                        if v.columns.iter().any(|(n, _)| *n == newname) {
                            return Err(Error::new(
                                code::DUPLICATE_COLUMN,
                                format!(
                                    "column \"{newname}\" of relation \"{}\" already exists",
                                    v.name
                                ),
                            ));
                        }
                        let before = Arc::new(self.cat.clone());
                        v.columns[i].0 = newname.clone();
                        self.rename_statistics_column(v.oid, &s.subname, &newname)?;
                        if let Some(st) = &v.storage {
                            let mut st = (**st).clone();
                            if let Some(j) = st.column_index(&s.subname) {
                                st.columns[j].name = newname;
                            }
                            v.storage = Some(Arc::new(st));
                        }
                        let change = super::stored::Change::Column(v.oid, i as i16 + 1);
                        self.cat.put_view(v);
                        self.follow_renames(&before, &[change])?;
                    }
                    _ => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("\"{}\" is not a table", rv.relname),
                        ));
                    }
                }
                self.save()?;
                Ok(tag)
            }
            O::ObjectTabconstraint => {
                let rv = s.relation.as_ref().unwrap();
                if s.missing_ok && self.lookup(rv)?.is_none() {
                    self.notice(format!(
                        "relation \"{}\" does not exist, skipping",
                        rv.relname
                    ));
                    return Ok("ALTER TABLE".into());
                }
                let mut t = (*self.table(rv)?).clone();
                let Some(i) = t.constraints.iter().position(|c| c.name == s.subname) else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!(
                            "constraint \"{}\" for table \"{}\" does not exist",
                            s.subname, t.name
                        ),
                    ));
                };
                // Inherited check constraints are renamed through the parent, in every child.
                if matches!(
                    t.constraints[i].kind,
                    ConstraintKind::Check {
                        no_inherit: false,
                        ..
                    } | ConstraintKind::NotNull { .. }
                ) {
                    if t.constraints[i].inhcount > 0 {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            format!("cannot rename inherited constraint \"{}\"", s.subname),
                        ));
                    }
                    let all = self.cat.inheritors(t.oid);
                    if all.len() > 1 && !rv.inh {
                        return Err(Error::new(
                            code::INVALID_TABLE_DEFINITION,
                            format!(
                                "inherited constraint \"{}\" must be renamed in child tables too",
                                s.subname
                            ),
                        ));
                    }
                    for oid in all.into_iter().skip(1) {
                        let Some(mut c) = self.cat.table(oid).map(|c| (**c).clone()) else {
                            continue;
                        };
                        if let Some(k) = c
                            .constraints
                            .iter_mut()
                            .find(|k| k.name == s.subname && k.inhcount > 0)
                        {
                            k.name = newname.clone();
                            self.cat.put_table(c);
                        }
                    }
                }
                if t.constraints.iter().any(|c| c.name == newname) {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{newname}\" for relation \"{}\" already exists",
                            t.name
                        ),
                    ));
                }
                if let Some(index) = t.constraints[i].kind.index()
                    && let Some(ix) = self.cat.indexes.get(&index).cloned()
                {
                    if self.cat.name_taken(t.schema, &newname) {
                        return Err(Error::new(
                            code::DUPLICATE_TABLE,
                            format!("relation \"{newname}\" already exists"),
                        ));
                    }
                    let mut ix = (*ix).clone();
                    self.cat.remove_name(t.schema, &ix.name);
                    ix.name = newname.clone();
                    self.cat.put_index(ix);
                }
                t.constraints[i].name = newname;
                self.cat.put_table(t);
                self.save()?;
                Ok("ALTER TABLE".into())
            }
            O::ObjectSchema => {
                let Some(sc) = self.cat.schema_by_name(&s.subname).cloned() else {
                    return Err(Error::new(
                        code::UNDEFINED_SCHEMA,
                        format!("schema \"{}\" does not exist", s.subname),
                    ));
                };
                if self.cat.schema_by_name(&newname).is_some() {
                    return Err(Error::new(
                        code::DUPLICATE_SCHEMA,
                        format!("schema \"{newname}\" already exists"),
                    ));
                }
                if newname.starts_with("pg_") {
                    return Err(Error::new(
                        code::RESERVED_NAME,
                        format!("unacceptable schema name \"{newname}\""),
                    )
                    .detail("The prefix \"pg_\" is reserved for system schemas."));
                }
                let before = Arc::new(self.cat.clone());
                self.cat.schemas.get_mut(&sc.oid).unwrap().name = newname;
                self.follow_renames(&before, &[super::stored::Change::Schema(sc.oid)])?;
                self.save()?;
                Ok("ALTER SCHEMA".into())
            }
            O::ObjectType | O::ObjectDomain => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let tname = parts.last().cloned().unwrap_or_default();
                let schema = if parts.len() > 1 {
                    Some(self.cat.schema_oid(&parts[parts.len() - 2])?)
                } else {
                    None
                };
                let Some(oid) = self.cat.type_by_name(schema, &tname, self.search_path()) else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("type \"{tname}\" does not exist"),
                    ));
                };
                let before = Arc::new(self.cat.clone());
                if let Some(e) = self.cat.enums.get(&oid).cloned() {
                    let mut e = (*e).clone();
                    e.name = newname;
                    self.cat.enums.insert(oid, Arc::new(e));
                } else if let Some(d) = self.cat.domains.get(&oid).cloned() {
                    let mut d = (*d).clone();
                    d.name = newname;
                    self.cat.domains.insert(oid, Arc::new(d));
                } else if let Some(c) = self.cat.composites.get(&oid).cloned() {
                    let mut c = (*c).clone();
                    c.name = newname;
                    self.cat.composites.insert(oid, Arc::new(c));
                } else if let Some(r) = self
                    .cat
                    .ranges
                    .values()
                    .find(|r| r.oid == oid || r.multi_oid == oid)
                    .cloned()
                {
                    let mut r = (*r).clone();
                    if r.oid == oid {
                        r.name = newname
                    } else {
                        r.multi_name = newname
                    }
                    self.cat.ranges.insert(r.oid, Arc::new(r));
                }
                self.follow_renames(&before, &[super::stored::Change::Type(oid)])?;
                self.save()?;
                Ok(format!("ALTER {}", super::object_type_word(kind)))
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let tag = format!("ALTER {}", super::object_type_word(kind));
                let Some(NodeEnum::ObjectWithArgs(owa)) = s.object.as_deref().and_then(inner)
                else {
                    return Ok(tag);
                };
                let Some(f) = self.find_routine(owa, kind)? else {
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!(
                            "{} {} does not exist",
                            super::object_type_word(kind).to_lowercase(),
                            self.function_sig(owa)
                        ),
                    ));
                };
                self.require_owner(
                    f.owner,
                    if f.procedure { "procedure" } else { "function" },
                    &f.name,
                )?;
                if self.cat.functions.values().any(|o| {
                    o.oid != f.oid && o.name == newname && o.schema == f.schema && o.same_params(&f)
                }) {
                    let sig = self.cat.param_type_names(&f, self.search_path()).join(", ");
                    let schema = self.cat.schema_name(f.schema).to_string();
                    return Err(Error::new(
                        code::DUPLICATE_FUNCTION,
                        format!(
                            "{} {newname}({sig}) already exists in schema \"{schema}\"",
                            if f.procedure { "procedure" } else { "function" }
                        ),
                    ));
                }
                let before = Arc::new(self.cat.clone());
                let mut f = (*f).clone();
                f.name = newname;
                let oid = f.oid;
                self.cat.functions.insert(f.oid, Arc::new(f));
                self.follow_renames(&before, &[super::stored::Change::Func(oid)])?;
                self.save()?;
                Ok(tag)
            }
            O::ObjectRole => {
                let Some(r) = self
                    .cat
                    .roles
                    .values()
                    .find(|r| r.name == s.subname)
                    .cloned()
                else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("role \"{}\" does not exist", s.subname),
                    ));
                };
                // RenameRole's checks.
                if r.oid == self.ctx.sess.session_role(&self.cat) {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "session user cannot be renamed",
                    ));
                }
                if r.oid == self.me() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "current user cannot be renamed",
                    ));
                }
                for n in [&r.name, &newname] {
                    if n.starts_with("pg_") {
                        return Err(Error::new(
                            code::RESERVED_NAME,
                            format!("role name \"{n}\" is reserved"),
                        )
                        .detail("Role names starting with \"pg_\" are reserved."));
                    }
                }
                if self.cat.role_by_name(&newname).is_some() {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!("role \"{newname}\" already exists"),
                    ));
                }
                let me = self.me();
                if r.superuser {
                    if !self.cat.is_superuser(me) {
                        return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to rename role")
                            .detail("Only roles with the SUPERUSER attribute may rename roles with the SUPERUSER attribute."));
                    }
                } else if !self.cat.roles.get(&me).is_some_and(|x| x.create_role || x.superuser)
                    || !self.is_admin_of_role(me, r.oid)
                {
                    return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to rename role")
                        .detail(format!("Only roles with the CREATEROLE attribute and the ADMIN option on role \"{}\" may rename this role.", r.name)));
                }
                let mut r = r;
                r.name = newname;
                // An MD5 hash is salted with the name.
                if r.password.as_deref().is_some_and(|p| p.starts_with("md5")) {
                    r.password = None;
                    self.notice("MD5 password cleared because of role rename");
                }
                self.cat.roles.insert(r.oid, r);
                self.save()?;
                Ok("ALTER ROLE".into())
            }
            O::ObjectDatabase => {
                self.ctx
                    .sess
                    .cluster
                    .rename_database(&s.subname, &newname)?;
                Ok("ALTER DATABASE".into())
            }
            O::ObjectTrigger => self.rename_trigger(s),
            O::ObjectDomconstraint => self.rename_domain_constraint(s),
            O::ObjectPolicy => self.rename_policy(s),
            O::ObjectRule => self.rename_rule(s),
            O::ObjectEventTrigger => self.rename_event_trigger(s),
            O::ObjectOpclass | O::ObjectOpfamily => self.rename_opc(kind == O::ObjectOpfamily, s),
            O::ObjectConversion | O::ObjectLanguage => self.rename_misc(kind, s),
            O::ObjectFdw | O::ObjectForeignServer => {
                self.rename_fdw(kind == O::ObjectForeignServer, s)
            }
            O::ObjectTablespace => self.rename_tablespace(&s.subname, &newname),
            O::ObjectPublication => {
                let old = s.object.as_deref().and_then(ast::sval).unwrap_or_default();
                self.rename_publication(old, &newname)
            }
            O::ObjectSubscription => {
                let old = s.object.as_deref().and_then(ast::sval).unwrap_or_default();
                self.rename_subscription(old, &newname)
            }
            _ => Err(crate::error::unsupported("this RENAME")),
        }
    }

    /// Serial defaults spell out their sequence's name: follow a rename.
    fn rewrite_seq_defaults(&mut self, seq: u32) -> Result<()> {
        let Some(sq) = self.cat.sequences.get(&seq).cloned() else {
            return Ok(());
        };
        let Some((toid, col)) = sq.owned_by else {
            return Ok(());
        };
        let Some(t) = self.cat.tables.get(&toid).cloned() else {
            return Ok(());
        };
        let mut t = (*t).clone();
        if t.columns[col].identity.is_none() {
            t.columns[col].default = Some(format!(
                "nextval('{}'::regclass)",
                self.qualified(sq.schema, &sq.name).replace('\'', "''")
            ));
        }
        self.cat.put_table(t);
        Ok(())
    }

    pub(super) fn set_schema(&mut self, s: &pb::AlterObjectSchemaStmt) -> Result<String> {
        use pb::ObjectType as O;
        let kind = O::try_from(s.object_type).unwrap_or(O::ObjectTable);
        if let Some(k) = super::tsearch::ts_kind(kind) {
            return self.set_schema_ts(k, s);
        }
        let is_rel = matches!(
            kind,
            O::ObjectTable | O::ObjectView | O::ObjectMatview | O::ObjectSequence | O::ObjectForeignTable
        );
        // (A relation's new schema is looked up once the relation is: IF EXISTS skips first.)
        let new_schema = if is_rel { 0 } else { self.cat.schema_oid(&s.newschema)? };
        match kind {
            O::ObjectTable
            | O::ObjectView
            | O::ObjectMatview
            | O::ObjectSequence
            | O::ObjectForeignTable => {
                let rv = s.relation.as_ref().unwrap();
                let Some(rel) = self.lookup(rv)? else {
                    if s.missing_ok {
                        self.notice(format!(
                            "relation \"{}\" does not exist, skipping",
                            rv.relname
                        ));
                        return Ok(format!("ALTER {}", super::object_type_word(kind)));
                    }
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                };
                let name = rv.relname.clone();
                self.require_rel_owner(rel.oid(), rel.kind_name(), &name)?;
                foreign_kind_check(&rel, kind, s.object_type, &name)?;
                // An owned sequence moves only with its table.
                if let Relation::Sequence(q) = &rel
                    && let Some(t) = q.owned_by.and_then(|o| self.cat.tables.get(&o.0))
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot move an owned sequence into another schema",
                    )
                    .detail(format!(
                        "Sequence \"{}\" is linked to table \"{}\".",
                        q.name, t.name
                    )));
                }
                let new_schema = self.cat.schema_oid(&s.newschema)?;
                self.require_schema_create(new_schema)?;
                // Already there: nothing to do (AlterRelationNamespaceInternal).
                let current = match &rel {
                    Relation::Table(t) => Some(t.schema),
                    Relation::View(v) => Some(v.schema),
                    Relation::Sequence(q) => Some(q.schema),
                    Relation::Index(_) => None,
                };
                if current == Some(new_schema) {
                    return Ok(format!("ALTER {}", super::object_type_word(kind)));
                }
                if self.cat.name_taken(new_schema, &name) {
                    return Err(Error::new(
                        code::DUPLICATE_TABLE,
                        format!(
                            "relation \"{name}\" already exists in schema \"{}\"",
                            s.newschema
                        ),
                    ));
                }
                let before = Arc::new(self.cat.clone());
                let mut moved = vec![super::stored::Change::Rel(rel.oid())];
                match rel {
                    Relation::Table(t) => {
                        let mut t = (*t).clone();
                        self.cat.remove_name(t.schema, &t.name);
                        let old_schema = t.schema;
                        t.schema = new_schema;
                        // Indexes and owned sequences move along.
                        for i in t.indexes.iter().chain(t.pkey_index.iter()) {
                            if let Some(ix) = self.cat.indexes.get(i).cloned() {
                                self.cat.remove_name(old_schema, &ix.name);
                            }
                        }
                        let toid = t.oid;
                        self.cat.put_table(t.clone());
                        for i in t.indexes.iter().chain(t.pkey_index.iter()) {
                            if let Some(ix) = self.cat.indexes.get(i).cloned() {
                                self.cat.put_index((*ix).clone());
                            }
                        }
                        let owned: Vec<Arc<Sequence>> = self
                            .cat
                            .sequences
                            .values()
                            .filter(|q| q.owned_by.map(|o| o.0) == Some(toid))
                            .cloned()
                            .collect();
                        for q in owned {
                            let mut q = (*q).clone();
                            self.cat.remove_name(q.schema, &q.name);
                            q.schema = new_schema;
                            moved.push(super::stored::Change::Rel(q.oid));
                            self.cat.put_sequence(q.clone());
                            self.rewrite_seq_defaults(q.oid)?;
                        }
                    }
                    Relation::View(v) => {
                        let mut v = (*v).clone();
                        self.cat.remove_name(v.schema, &v.name);
                        let old_schema = v.schema;
                        v.schema = new_schema;
                        let indexes = v
                            .storage
                            .as_ref()
                            .map(|t| t.indexes.clone())
                            .unwrap_or_default();
                        self.cat.put_view(v);
                        // A materialized view's indexes move with it.
                        for i in indexes {
                            if let Some(ix) = self.cat.indexes.get(&i).cloned() {
                                self.cat.remove_name(old_schema, &ix.name);
                                self.cat.put_index((*ix).clone());
                            }
                        }
                    }
                    Relation::Sequence(q) => {
                        let mut q = (*q).clone();
                        self.cat.remove_name(q.schema, &q.name);
                        q.schema = new_schema;
                        self.cat.put_sequence(q.clone());
                        self.rewrite_seq_defaults(q.oid)?;
                    }
                    Relation::Index(_) => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            "cannot change schema of index",
                        ));
                    }
                }
                self.follow_renames(&before, &moved)?;
                self.save()?;
                Ok(format!("ALTER {}", super::object_type_word(kind)))
            }
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate => {
                let tag = format!("ALTER {}", super::object_type_word(kind));
                let Some(NodeEnum::ObjectWithArgs(owa)) = s.object.as_deref().and_then(inner)
                else {
                    return Ok(tag);
                };
                let Some(f) = self.find_routine(owa, kind)? else {
                    if s.missing_ok {
                        self.notice(format!(
                            "{} {} does not exist, skipping",
                            super::object_type_word(kind).to_lowercase(),
                            self.function_sig(owa)
                        ));
                        return Ok(tag);
                    }
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!(
                            "{} {} does not exist",
                            super::object_type_word(kind).to_lowercase(),
                            self.function_sig(owa)
                        ),
                    ));
                };
                self.require_owner(
                    f.owner,
                    if f.procedure { "procedure" } else { "function" },
                    &f.name,
                )?;
                self.require_schema_create(new_schema)?;
                if self.cat.functions.values().any(|o| {
                    o.oid != f.oid
                        && o.name == f.name
                        && o.schema == new_schema
                        && o.same_params(&f)
                }) {
                    let sig = self.cat.param_type_names(&f, self.search_path()).join(", ");
                    return Err(Error::new(
                        code::DUPLICATE_FUNCTION,
                        format!(
                            "{} {}({sig}) already exists in schema \"{}\"",
                            if f.procedure { "procedure" } else { "function" },
                            f.name,
                            s.newschema
                        ),
                    ));
                }
                let before = Arc::new(self.cat.clone());
                let mut f = (*f).clone();
                f.schema = new_schema;
                let oid = f.oid;
                self.cat.functions.insert(f.oid, Arc::new(f));
                self.follow_renames(&before, &[super::stored::Change::Func(oid)])?;
                self.save()?;
                Ok(tag)
            }
            O::ObjectOperator => {
                let Some(NodeEnum::ObjectWithArgs(owa)) = s.object.as_deref().and_then(inner)
                else {
                    return Ok("ALTER OPERATOR".into());
                };
                self.operator_set_schema(owa, &s.newschema)?;
                self.save()?;
                Ok("ALTER OPERATOR".into())
            }
            O::ObjectCollation => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                self.collation_set_schema(&parts, &s.newschema)?;
                self.save()?;
                Ok("ALTER COLLATION".into())
            }
            O::ObjectStatisticExt => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let Some(st) = self.statistics_object(&parts, s.missing_ok)? else {
                    return Ok("ALTER STATISTICS".into());
                };
                self.require_owner(st.owner, "statistics object", &st.name)?;
                self.require_schema_create(new_schema)?;
                if st.schema != new_schema
                    && self
                        .cat
                        .statistics
                        .values()
                        .any(|x| x.schema == new_schema && x.name == st.name)
                {
                    return Err(Error::new(
                        code::DUPLICATE_OBJECT,
                        format!(
                            "statistics object \"{}\" already exists in schema \"{}\"",
                            st.name, s.newschema
                        ),
                    ));
                }
                self.cat.statistics.insert(
                    st.oid,
                    Arc::new(crate::catalog::StatExt {
                        schema: new_schema,
                        ..(*st).clone()
                    }),
                );
                self.save()?;
                Ok("ALTER STATISTICS".into())
            }
            O::ObjectType | O::ObjectDomain => {
                let parts = match s.object.as_deref().and_then(inner) {
                    Some(NodeEnum::List(l)) => names(&l.items),
                    _ => Vec::new(),
                };
                let tname = parts.last().cloned().unwrap_or_default();
                let schema = if parts.len() > 1 {
                    Some(self.cat.schema_oid(&parts[parts.len() - 2])?)
                } else {
                    None
                };
                let Some(oid) = self.cat.type_by_name(schema, &tname, self.search_path()) else {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("type \"{}\" does not exist", parts.join(".")),
                    ));
                };
                let owner = self
                    .cat
                    .obj_owner(crate::catalog::acl::ObjKind::Type, oid, self.db_owner())
                    .unwrap_or(0);
                self.require_owner(
                    owner,
                    if kind == O::ObjectDomain {
                        "domain"
                    } else {
                        "type"
                    },
                    &tname,
                )?;
                self.require_schema_create(new_schema)?;
                let before = Arc::new(self.cat.clone());
                if let Some(e) = self.cat.enums.get(&oid).cloned() {
                    let mut e = (*e).clone();
                    e.schema = new_schema;
                    self.cat.enums.insert(oid, Arc::new(e));
                } else if let Some(d) = self.cat.domains.get(&oid).cloned() {
                    let mut d = (*d).clone();
                    d.schema = new_schema;
                    self.cat.domains.insert(oid, Arc::new(d));
                } else if let Some(c) = self.cat.composites.get(&oid).cloned() {
                    let mut c = (*c).clone();
                    c.schema = new_schema;
                    self.cat.composites.insert(oid, Arc::new(c));
                } else if let Some(r) = self.cat.ranges.get(&oid).cloned() {
                    let mut r = (*r).clone();
                    r.schema = new_schema;
                    self.cat.ranges.insert(oid, Arc::new(r));
                }
                self.follow_renames(&before, &[super::stored::Change::Type(oid)])?;
                self.save()?;
                Ok(format!("ALTER {}", super::object_type_word(kind)))
            }
            O::ObjectOpclass | O::ObjectOpfamily => {
                self.set_schema_opc(kind == O::ObjectOpfamily, s)
            }
            O::ObjectConversion => self.set_schema_conversion(s),
            _ => Err(crate::error::unsupported("this ALTER ... SET SCHEMA")),
        }
    }

    // ── TRUNCATE / LOCK ──

    pub(super) fn truncate(&mut self, s: &pb::TruncateStmt) -> Result<String> {
        let mut tables: Vec<Arc<Table>> = Vec::new();
        for r in &s.relations {
            let Some(NodeEnum::RangeVar(rv)) = inner(r) else {
                continue;
            };
            let t = match self.lookup(rv)? {
                Some(Relation::Table(t)) => t,
                Some(_) => {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a table", rv.relname),
                    ));
                }
                None => {
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                }
            };
            self.require_rel_priv(t.oid, crate::catalog::acl::TRUNCATE, false)?;
            self.note_lock(t.oid, lockmode::ACCESS_EXCLUSIVE)?;
            if !rv.inh && t.partkey.is_some() {
                return Err(Error::new(code::WRONG_OBJECT_TYPE, "cannot truncate only a partitioned table")
                    .hint("Do not specify the ONLY keyword, or use TRUNCATE ONLY on the partitions directly."));
            }
            // Without ONLY, the inheritance children too.
            let all = if rv.inh {
                self.cat.inheritors(t.oid)
            } else {
                vec![t.oid]
            };
            let my_temp = self.cat.schema_by_name("pg_temp").map(|s| s.oid);
            for oid in all {
                let Some(t) = self.cat.table(oid).cloned() else {
                    continue;
                };
                if tables.iter().any(|x| x.oid == oid) {
                    continue;
                }
                // (Another session's temporary child is left alone.)
                if t.temporary && Some(t.schema) != my_temp {
                    continue;
                }
                // (Inherited TRUNCATE checks the parent's privileges only.)
                self.note_lock(t.oid, lockmode::ACCESS_EXCLUSIVE)?;
                self.not_in_use(t.oid, "TRUNCATE", &t.name)?;
                // truncate_check_rel: a foreign table's wrapper would do it.
                if let Some(e) = self.cat.fdw_modify_error(&t, "truncate") {
                    return Err(e);
                }
                // (restrict_nonsystem_relation_kind, as GetFdwRoutineForRelation checks it.)
                if t.foreign.is_some() && crate::session::settings::restricted_kinds(&self.ctx.sess.settings.borrow()).1 {
                    return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "access to non-system foreign table is restricted"));
                }
                tables.push(t);
            }
        }
        let cascade = s.behavior == pb::DropBehavior::DropCascade as i32;
        // Tables referencing these through foreign keys, round by round as
        // heap_truncate_find_FKs finds them (each round's in OID order); a referencing
        // partitioned table's partitions reference them too (their clones of its key).
        loop {
            let mut found: Vec<Arc<Table>> = Vec::new();
            for t in tables.clone() {
                for (child, _) in self.cat.referencing_fks(t.oid) {
                    if tables.iter().chain(&found).any(|x| x.oid == child.oid) {
                        continue;
                    }
                    if !cascade {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "cannot truncate a table referenced in a foreign key constraint",
                        )
                        .detail(format!(
                            "Table \"{}\" references \"{}\".",
                            child.name, t.name
                        ))
                        .hint(format!(
                            "Truncate table \"{}\" at the same time, or use TRUNCATE ... CASCADE.",
                            child.name
                        )));
                    }
                    for oid in self.cat.inheritors(child.oid) {
                        if let Some(p) = self.cat.table(oid).cloned()
                            && (oid == child.oid || p.partbound.is_some())
                            && !tables.iter().chain(&found).any(|x| x.oid == oid)
                        {
                            found.push(p);
                        }
                    }
                }
            }
            if found.is_empty() {
                break;
            }
            found.sort_by_key(|t| t.oid);
            for child in found {
                self.require_rel_priv(child.oid, crate::catalog::acl::TRUNCATE, false)?;
                self.note_lock(child.oid, lockmode::ACCESS_EXCLUSIVE)?;
                self.notice(format!("truncate cascades to table \"{}\"", child.name));
                tables.push(child);
            }
        }
        // BEFORE TRUNCATE triggers of every table fire first, AFTER ones once all are empty.
        use crate::catalog::trigger_type as tt;
        let mut trigs = Vec::with_capacity(tables.len());
        for t in &tables {
            trigs.push(self.binder().bound_triggers(t)?);
        }
        for (t, b) in tables.iter().zip(&trigs) {
            crate::exec::trigger::statement(b, t, true, tt::TRUNCATE, &[], self.ctx)?;
        }
        // (Foreign tables go to their wrappers afterwards, a server's all in one call:
        // ExecuteTruncateGuts's per-server lists.)
        let mut foreign: Vec<(u32, Vec<Arc<Table>>)> = Vec::new();
        for t in &tables {
            if let Some(f) = &t.foreign {
                match foreign.iter_mut().find(|(s, _)| *s == f.server) {
                    Some((_, v)) => v.push(t.clone()),
                    None => foreign.push((f.server, vec![t.clone()])),
                }
                continue;
            }
            // ExecuteTruncateGuts gives the TOAST table new files too, under its own
            // AccessExclusiveLock (and its index's).
            if t.toast != 0 {
                self.note_lock(t.toast, lockmode::ACCESS_EXCLUSIVE)?;
                self.note_lock(t.toast + 1, lockmode::ACCESS_EXCLUSIVE)?;
            }
            crate::exec::dml::truncate(t, self.ctx)?;
            crate::exec::pgstat::count_truncate(self.ctx, t.oid);
            if t.partkey.is_none() {
                // ON COMMIT DELETE ROWS, or a table given its files in this subtransaction:
                // emptied in place (heap_truncate_one_rel), its statistics as the index
                // rebuilds leave them.
                let in_place = self.ctx.sess.on_commit_truncating.get()
                    || self.ctx.txn.borrow().storage_new_here(t.oid);
                if in_place {
                    self.truncated_in_place(t);
                } else {
                    self.reset_stats(t);
                }
                // New files for the heap, its TOAST table and index, and its indexes; unless
                // it was created or given new files in this subtransaction, when it's emptied
                // in place (heap_truncate_one_rel).
                let mut txn = self.ctx.txn.borrow_mut();
                if !txn.storage_new_here(t.oid) {
                    txn.mark_new_storage(t.oid);
                    drop(txn);
                    self.cat.truncate_files(t);
                    let xmin = self.recent_xmin();
                    self.set_frozen_xid(t, xmin);
                }
            }
        }
        // Then the foreign tables, by server (ExecForeignTruncate).
        for (_, rels) in &foreign {
            crate::ext::postgres_fdw::modify::truncate(rels, cascade, s.restart_seqs, self.ctx)?;
        }
        // Then the sequences, each with a new file too.
        if s.restart_seqs {
            // The sequences their columns own: serial and identity ones, and any made theirs
            // with ALTER SEQUENCE ... OWNED BY.
            let mut seqs: Vec<u32> = Vec::new();
            for t in &tables {
                for c in &t.columns {
                    if let Some(o) = c.seq
                        && !seqs.contains(&o)
                    {
                        seqs.push(o);
                    }
                }
                let mut owned: Vec<u32> = self
                    .cat
                    .sequences
                    .values()
                    .filter(|q| q.owned_by.is_some_and(|(rel, _)| rel == t.oid))
                    .map(|q| q.oid)
                    .filter(|o| !seqs.contains(o))
                    .collect();
                owned.sort_unstable();
                seqs.extend(owned);
            }
            for oid in seqs {
                if let Some(sq) = self.cat.sequences.get(&oid).cloned() {
                    // (The new file is transactional: a rollback brings the old one back.)
                    let before = self.ctx.sess.db.seq_snapshot(&sq)?;
                    self.ctx.txn.borrow_mut().seq_undo.push((sq.oid, before));
                    self.ctx.sess.db.reset_seq(sq.oid, Some(sq.start))?;
                    self.cat.new_filenode(sq.oid);
                }
            }
        }
        for (t, b) in tables.iter().zip(&trigs) {
            crate::exec::trigger::statement(b, t, false, tt::TRUNCATE, &[], self.ctx)?;
        }
        self.save()?;
        Ok("TRUNCATE TABLE".into())
    }

    pub(super) fn lock(&mut self, s: &pb::LockStmt) -> Result<String> {
        self.lock_nowait = s.nowait;
        let r = self.lock_tables(s);
        self.lock_nowait = false;
        r
    }

    fn lock_tables(&mut self, s: &pb::LockStmt) -> Result<String> {
        for r in &s.relations {
            let Some(NodeEnum::RangeVar(rv)) = inner(r) else {
                continue;
            };
            let Some(rel) = self.lookup(rv)? else {
                // A system catalog or view (pg_dump --schema=pg_catalog locks them all).
                if let Some(t) =
                    system::SysTable::lookup(Self::rv_schema(rv), &rv.relname, self.search_path())
                {
                    self.require_rel_priv(t.oid(), lock_acl_mask(s.mode), true)?;
                    self.note_lock(t.oid(), s.mode)?;
                    continue;
                }
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            };
            // RangeVarCallbackForLockTable: tables and views only.
            let kind = match &rel {
                Relation::Table(t) => t.relkind(),
                Relation::View(v) if v.materialized => 'm',
                Relation::View(_) => 'v',
                Relation::Sequence(_) => 'S',
                Relation::Index(i) => {
                    if self.cat.tables.get(&i.table).is_some_and(|t| t.partkey.is_some()) {
                        'I'
                    } else {
                        'i'
                    }
                }
            };
            if !matches!(kind, 'r' | 'p' | 'v') {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("cannot lock relation \"{}\"", rv.relname),
                )
                .detail(format!(
                    "This operation is not supported for {}.",
                    super::foreign::relkind_plural(kind)
                )));
            }
            self.require_rel_priv(rel.oid(), lock_acl_mask(s.mode), true)?;
            self.note_lock(rel.oid(), s.mode)?;
            match &rel {
                Relation::View(v) => self.lock_view(v, s.mode, &mut vec![v.oid])?,
                _ if rv.inh => {
                    for c in self.cat.inheritors(rel.oid()).into_iter().skip(1) {
                        self.note_lock(c, s.mode)?;
                    }
                }
                _ => {}
            }
        }
        Ok("LOCK TABLE".into())
    }

    /// Lock relation `oid` in `mode` (a lockdefs.h number) unless it isn't free (SKIP
    /// LOCKED): Ok(false) then.
    pub(super) fn try_lock(&self, oid: u32, mode: i32) -> Result<bool> {
        let Some(lm) = crate::txn::locks::LockMode::from_number(mode) else {
            return Ok(true);
        };
        let rel = crate::sql::binder::perms::rel_word(&self.cat, oid).1;
        self.ctx.txn.borrow_mut().lock_relation(
            oid,
            lm,
            crate::txn::locks::WaitPolicy::SkipLocked,
            &|| rel.clone(),
        )
    }

    /// After waiting for a lock: the catalog as other transactions left it (what Postgres
    /// sees through the invalidations its lock acquisition accepts), while this
    /// transaction hasn't changed it itself.
    pub(super) fn refresh_catalog(&mut self) {
        let txn = self.ctx.txn.borrow();
        if txn.changed_catalog().is_none() {
            let cur = txn.catalog();
            if cur.version != self.cat.version {
                self.cat = (*cur).clone();
            }
        }
    }

    /// Lock relation `oid` in `mode` (a lockdefs.h number) until the transaction ends;
    /// pg_locks shows it.
    pub(super) fn note_lock(&self, oid: u32, mode: i32) -> Result<()> {
        const MODES: [&str; 9] = [
            "",
            "AccessShareLock",
            "RowShareLock",
            "RowExclusiveLock",
            "ShareUpdateExclusiveLock",
            "ShareLock",
            "ShareRowExclusiveLock",
            "ExclusiveLock",
            "AccessExclusiveLock",
        ];
        let Some(&name) = MODES.get(mode as usize).filter(|n| !n.is_empty()) else {
            return Ok(());
        };
        let _ = name;
        let Some(lm) = crate::txn::locks::LockMode::from_number(mode) else {
            return Ok(());
        };
        let rel = crate::sql::binder::perms::rel_word(&self.cat, oid).1;
        let policy = if self.lock_nowait {
            crate::txn::locks::WaitPolicy::NoWait
        } else {
            crate::txn::locks::WaitPolicy::Block
        };
        let before = self.ctx.sess.db.catalog().version;
        self.ctx.txn.borrow_mut().lock_relation(oid, lm, policy, &|| rel.clone())?;
        // RangeVarGetRelidExtended: the name looked up again once the lock is held; a
        // relation another transaction dropped or renamed meanwhile isn't there by it.
        let now = self.ctx.sess.db.catalog();
        if now.version != before && oid >= crate::catalog::FIRST_USER_OID {
            let (word, name) = crate::sql::binder::perms::rel_word(&now, oid);
            if word == "relation" || name != rel {
                return Err(crate::catalog::no_such_relation(None, &rel));
            }
        }
        Ok(())
    }

    /// LockViewRecurse: the relations a view's query reads are locked too, checked as the
    /// view's owner (as the user, for a security_invoker view).
    fn lock_view(
        &self,
        v: &crate::catalog::ViewDef,
        mode: i32,
        ancestors: &mut Vec<u32>,
    ) -> Result<()> {
        let user = if v.option("security_invoker") { self.me() } else { v.owner };
        let Ok(q) = ast::parse_stmt(&v.query) else {
            return Ok(());
        };
        let Some(ne) = q.node.as_ref() else {
            return Ok(());
        };
        let mut ctes = HashSet::new();
        for (node, _, _, _) in ne.nodes() {
            if let pg_query::NodeRef::CommonTableExpr(c) = node {
                ctes.insert(c.ctename.clone());
            }
        }
        for (node, _, _, _) in ne.nodes() {
            let pg_query::NodeRef::RangeVar(rv) = node else {
                continue;
            };
            if rv.schemaname.is_empty() && ctes.contains(&rv.relname) {
                continue;
            }
            let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
            let Ok(Some(rel)) = self.cat.lookup(schema, &rv.relname, self.search_path()) else {
                continue;
            };
            let oid = rel.oid();
            let view = match &rel {
                Relation::Table(t) if matches!(t.relkind(), 'r' | 'p') => None,
                Relation::View(w) if !w.materialized => Some(w.clone()),
                _ => continue,
            };
            if ancestors.contains(&oid) {
                continue;
            }
            if !self.cat.is_superuser(user)
                && self
                    .cat
                    .rel_mask(oid, user, self.db_owner())
                    .is_some_and(|(p, _)| p & lock_acl_mask(mode) == 0)
            {
                let (word, name) = crate::sql::binder::perms::rel_word(&self.cat, oid);
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for {word} {name}"),
                ));
            }
            self.note_lock(oid, mode)?;
            match view {
                Some(w) => {
                    ancestors.push(oid);
                    self.lock_view(&w, mode, ancestors)?;
                    ancestors.pop();
                }
                None if rv.inh => {
                    for c in self.cat.inheritors(oid).into_iter().skip(1) {
                        self.note_lock(c, mode)?;
                    }
                }
                None => {}
            }
        }
        Ok(())
    }

    /// The user functions a call may be of (by name, schema and number of arguments).
    pub(super) fn call_targets(&self, fc: &pb::FuncCall) -> Vec<u32> {
        let parts = names(&fc.funcname);
        let fname = parts.last().cloned().unwrap_or_default();
        let schemas: Vec<u32> = match parts.as_slice() {
            [.., sc, _] => self
                .cat
                .schema_by_name(sc)
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            _ => self.cat.effective_path(self.search_path()),
        };
        let n = if fc.agg_star { 0 } else { fc.args.len() };
        self.cat
            .functions
            .values()
            .filter(|f| {
                f.name == fname
                    && schemas.contains(&f.schema)
                    && (f.args.len() == n
                        || f.is_variadic()
                        || f.defaults.iter().any(Option::is_some))
            })
            .map(|f| f.oid)
            .collect()
    }

    /// Relations a view's query reads (for dependency checks).
    pub(super) fn view_deps(&self, q: &pb::Node) -> Vec<u32> {
        let mut out = Vec::new();
        self.view_deps_in(q, &HashSet::new(), &mut out);
        out
    }

    fn view_deps_in(&self, q: &pb::Node, outer_ctes: &HashSet<String>, out: &mut Vec<u32>) {
        let Some(ne) = q.node.as_ref() else {
            return;
        };
        let mut ctes = outer_ctes.clone();
        for (node, _, _, _) in ne.nodes() {
            if let pg_query::NodeRef::CommonTableExpr(c) = node {
                ctes.insert(c.ctename.clone());
            }
        }
        for (node, _, _, _) in ne.nodes() {
            // (The node walk doesn't enter an indirection's argument: `(f(x)).col`.)
            if let pg_query::NodeRef::AIndirection(ind) = node
                && let Some(arg) = ind.arg.as_deref()
            {
                self.view_deps_in(arg, &ctes, out);
            }
            // User functions it calls.
            if let pg_query::NodeRef::FuncCall(fc) = node {
                for f in self.call_targets(fc) {
                    if !out.contains(&f) {
                        out.push(f);
                    }
                }
            }
            // (The node walk doesn't enter TABLESAMPLE's relation.)
            let rv = match node {
                pg_query::NodeRef::RangeVar(rv) => Some(rv),
                pg_query::NodeRef::RangeTableSample(ts) => {
                    match ts.relation.as_deref().and_then(|n| n.node.as_ref()) {
                        Some(NodeEnum::RangeVar(rv)) => Some(rv),
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(rv) = rv {
                if rv.schemaname.is_empty() && ctes.contains(&rv.relname) {
                    continue;
                }
                let schema = if rv.schemaname.is_empty() {
                    None
                } else {
                    Some(rv.schemaname.as_str())
                };
                if let Ok(Some(r)) = self.cat.lookup(schema, &rv.relname, self.search_path())
                    && !out.contains(&r.oid())
                {
                    out.push(r.oid());
                }
            }
        }
    }
}

/// CheckAttributeType: a table's column can't hold its own row type (directly, in an
/// array, a domain or another composite).
fn check_not_self_member(t: &Table, ty: &DataType) -> Result<()> {
    fn contains(ty: &DataType, target: u32) -> bool {
        match ty {
            DataType::Row(r) => {
                r.oid == target || r.fields.iter().any(|(_, f)| contains(f, target))
            }
            DataType::Array(e) => contains(e, target),
            _ => false,
        }
    }
    if t.rowtype != 0 && contains(ty, t.rowtype) {
        return Err(Error::new(
            code::INVALID_TABLE_DEFINITION,
            format!("composite type {} cannot be made a member of itself", t.name),
        ));
    }
    Ok(())
}

/// Lock modes (as LOCK TABLE numbers them).
pub(super) mod lockmode {
    pub const ACCESS_SHARE: i32 = 1;
    pub const ROW_SHARE: i32 = 2;
    pub const SHARE_UPDATE_EXCLUSIVE: i32 = 4;
    pub const SHARE: i32 = 5;
    pub const SHARE_ROW_EXCLUSIVE: i32 = 6;
    pub const EXCLUSIVE: i32 = 7;
    pub const ACCESS_EXCLUSIVE: i32 = 8;
}

/// AlterTableGetRelOptionsLockLevel: what changing these options locks.
fn reloptions_lock(def: Option<&pb::Node>) -> i32 {
    let items: Vec<pb::Node> = match def.and_then(inner) {
        Some(NodeEnum::List(l)) => l.items.clone(),
        _ => Vec::new(),
    };
    let mut level = 0;
    for d in def_elems(&items) {
        let name = if d.defnamespace.is_empty() {
            d.defname.clone()
        } else {
            format!("{}.{}", d.defnamespace, d.defname)
        };
        let l = match name.as_str() {
            "security_barrier" | "security_invoker" | "check_option" | "user_catalog_table" => {
                lockmode::ACCESS_EXCLUSIVE
            }
            n if n.starts_with("toast.")
                || n.starts_with("autovacuum_")
                || matches!(
                    n,
                    "fillfactor"
                        | "parallel_workers"
                        | "toast_tuple_target"
                        | "vacuum_index_cleanup"
                        | "vacuum_truncate"
                        | "vacuum_max_eager_freeze_failure_rate"
                        | "log_autovacuum_min_duration"
                ) =>
            {
                lockmode::SHARE_UPDATE_EXCLUSIVE
            }
            _ => lockmode::ACCESS_EXCLUSIVE,
        };
        level = level.max(l);
    }
    if level == 0 { lockmode::ACCESS_EXCLUSIVE } else { level }
}

/// AlterTableGetLockLevel for one subcommand.
pub(super) fn alter_cmd_lock(cmd: &pb::AlterTableCmd) -> i32 {
    use pb::AlterTableType as A;
    match A::try_from(cmd.subtype).unwrap_or(A::Undefined) {
        A::AtSetStatistics
        | A::AtClusterOn
        | A::AtDropCluster
        | A::AtSetOptions
        | A::AtResetOptions
        | A::AtValidateConstraint
        | A::AtAttachPartition
        | A::AtDetachPartitionFinalize => lockmode::SHARE_UPDATE_EXCLUSIVE,
        A::AtDetachPartition => match cmd.def.as_deref().and_then(inner) {
            Some(NodeEnum::PartitionCmd(p)) if p.concurrent => lockmode::SHARE_UPDATE_EXCLUSIVE,
            _ => lockmode::ACCESS_EXCLUSIVE,
        },
        A::AtEnableTrig
        | A::AtEnableAlwaysTrig
        | A::AtEnableReplicaTrig
        | A::AtEnableTrigAll
        | A::AtEnableTrigUser
        | A::AtDisableTrig
        | A::AtDisableTrigAll
        | A::AtDisableTrigUser => lockmode::SHARE_ROW_EXCLUSIVE,
        A::AtAddConstraint | A::AtReAddConstraint => match cmd.def.as_deref().and_then(inner) {
            Some(NodeEnum::Constraint(c)) if c.contype == pb::ConstrType::ConstrForeign as i32 => {
                lockmode::SHARE_ROW_EXCLUSIVE
            }
            _ => lockmode::ACCESS_EXCLUSIVE,
        },
        A::AtSetRelOptions | A::AtResetRelOptions => reloptions_lock(cmd.def.as_deref()),
        _ => lockmode::ACCESS_EXCLUSIVE,
    }
}

impl Ddl<'_, '_> {
    /// CheckTableNotInUse: not while this session's queries use the table, nor with
    /// trigger events on it still pending (deferred).
    pub(super) fn not_in_use(&self, oid: u32, stmt: &str, name: &str) -> Result<()> {
        self.ctx.sess.check_not_in_use(oid, stmt, name)?;
        if self.ctx.txn.borrow().deferred_triggers.iter().any(|d| d.table == oid) {
            return Err(Error::new(
                code::OBJECT_IN_USE,
                format!("cannot {stmt} \"{name}\" because it has pending trigger events"),
            ));
        }
        Ok(())
    }

    /// The relation locks an ALTER TABLE takes (for pg_locks): the table at the strongest
    /// level its subcommands need, and what they open besides.
    fn note_alter_locks(&self, t: &Table, cmds: &[pb::Node]) -> Result<()> {
        use pb::AlterTableType as A;
        let cmds: Vec<&pb::AlterTableCmd> = cmds
            .iter()
            .filter_map(|c| match inner(c) {
                Some(NodeEnum::AlterTableCmd(c)) => Some(&**c),
                _ => None,
            })
            .collect();
        let Some(level) = cmds.iter().map(|c| alter_cmd_lock(c)).max() else {
            return Ok(());
        };
        self.note_lock(t.oid, level)?;
        let indexes = |tb: &Table| -> Vec<u32> {
            let mut v: Vec<u32> = tb.indexes.clone();
            v.extend(tb.pkey_index);
            v
        };
        for c in cmds {
            match A::try_from(c.subtype).unwrap_or(A::Undefined) {
                // check_index_is_clusterable opens the index.
                A::AtClusterOn => {
                    if let Some(ix) = indexes(t)
                        .into_iter()
                        .find(|i| self.cat.indexes.get(i).is_some_and(|x| x.name == c.name))
                    {
                        self.note_lock(ix, lockmode::SHARE_UPDATE_EXCLUSIVE)?;
                    }
                }
                // ATExecSetRelOptions changes the TOAST table's too.
                A::AtSetRelOptions | A::AtResetRelOptions | A::AtReplaceRelOptions
                    if t.toast != 0 =>
                {
                    self.note_lock(t.toast, alter_cmd_lock(c))?;
                }
                // A foreign key locks the referenced table; checking existing rows plans a
                // query over both tables (opening their indexes).
                A::AtAddConstraint => {
                    let Some(NodeEnum::Constraint(k)) = c.def.as_deref().and_then(inner) else {
                        continue;
                    };
                    if k.contype != pb::ConstrType::ConstrForeign as i32 {
                        continue;
                    }
                    let Some(rv) = k.pktable.as_ref() else {
                        continue;
                    };
                    let Ok(Some(Relation::Table(r))) = self.lookup(rv) else {
                        continue;
                    };
                    self.note_lock(r.oid, lockmode::SHARE_ROW_EXCLUSIVE)?;
                    let ref_ix: Vec<u32> = indexes(&r)
                        .into_iter()
                        .filter(|i| self.cat.indexes.get(i).is_some_and(|x| x.unique))
                        .collect();
                    if k.skip_validation {
                        if let Some(i) = ref_ix.first() {
                            self.note_lock(*i, lockmode::ACCESS_SHARE)?;
                        }
                        continue;
                    }
                    for i in indexes(t).into_iter().chain(indexes(&r)) {
                        self.note_lock(i, lockmode::ACCESS_SHARE)?;
                    }
                }
                A::AtValidateConstraint => {
                    let Some(ConstraintKind::ForeignKey { ref_table, .. }) = t
                        .constraints
                        .iter()
                        .find(|k| k.name == c.name)
                        .map(|k| &k.kind)
                    else {
                        continue;
                    };
                    let Some(r) = self.cat.tables.get(ref_table).cloned() else {
                        continue;
                    };
                    self.note_lock(r.oid, lockmode::ROW_SHARE)?;
                    for i in indexes(t).into_iter().chain(indexes(&r)) {
                        self.note_lock(i, lockmode::ACCESS_SHARE)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// transformColumnNameList: a key column that isn't there (a system column can't be one).
fn fk_column_missing(n: &str) -> Error {
    if matches!(n, "tableoid" | "ctid" | "xmin" | "cmin" | "xmax" | "cmax") {
        return Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "system columns cannot be used in foreign keys",
        );
    }
    Error::new(
        code::UNDEFINED_COLUMN,
        format!("column \"{n}\" referenced in foreign key constraint does not exist"),
    )
}

/// LockTableAclCheck: any of these privileges allows any lock mode; SELECT also ACCESS
/// SHARE, INSERT also up to ROW EXCLUSIVE.
fn lock_acl_mask(mode: i32) -> u32 {
    use crate::catalog::acl::{DELETE, INSERT, MAINTAIN, SELECT, TRUNCATE, UPDATE};
    let mut need = MAINTAIN | UPDATE | DELETE | TRUNCATE;
    if mode <= 1 {
        need |= SELECT;
    }
    if mode <= 3 {
        need |= INSERT;
    }
    need
}

/// The column a CHECK expression is named after: the only one it mentions, if it mentions
/// exactly one.
impl Ddl<'_, '_> {
    /// ALTER COLUMN TYPE: remember what depends on column `idx` of `t` for re-creation
    /// (RememberAllDependentForRebuilding), in pg_depend order (taken as oid order): each
    /// constraint on it (a not-null one goes first, as lcons puts it), foreign keys of other
    /// tables to it, and plain indexes. What a partition or inheritor has from its parent
    /// comes back when the parent's is re-created.
    fn remember_type_rebuilds(&mut self, t: &Table, idx: usize) {
        let name = t.columns[idx].name.as_str();
        let index_uses = |ix: &Index| {
            ix.include.contains(&idx)
                || ix.columns.iter().any(|c| match &c.key {
                    IndexKey::Column(c) => *c == idx,
                    IndexKey::Expr(e) => view_mentions(e, name),
                })
                || ix.predicate.as_deref().is_some_and(|p| view_mentions(p, name))
        };
        // (owner table, constraint oid), and plain index oids.
        let mut cons: Vec<(u32, u32, bool)> = Vec::new();
        let mut ixs: Vec<u32> = Vec::new();
        for c in &t.constraints {
            // Inherited constraints come back with their parent's (a merged local one merges
            // into that again, taking no oid), as do a partition's copies of its partitioned
            // table's keys and the constraints derived from a foreign key.
            if !c.islocal || c.parent != 0 || c.inhcount > 0 {
                continue;
            }
            let uses = match &c.kind {
                ConstraintKind::Check { expr, .. } => view_mentions(expr, name),
                ConstraintKind::NotNull { column } => *column == idx,
                ConstraintKind::ForeignKey { columns, .. } => columns.contains(&idx),
                k => k
                    .index()
                    .and_then(|i| self.cat.indexes.get(&i))
                    .is_some_and(|ix| index_uses(ix)),
            };
            if uses {
                cons.push((t.oid, c.oid, matches!(c.kind, ConstraintKind::NotNull { .. })));
            }
        }
        for o in self.cat.tables.values().filter(|o| o.oid != t.oid) {
            for c in &o.constraints {
                if let ConstraintKind::ForeignKey {
                    ref_table,
                    ref_columns,
                    ..
                } = &c.kind
                    && *ref_table == t.oid
                    && ref_columns.contains(&idx)
                    && c.parent == 0
                {
                    cons.push((o.oid, c.oid, false));
                }
            }
        }
        for i in t.indexes.iter().chain(&t.pkey_index) {
            // A partition's index of its partitioned table's comes back with that one.
            if let Some(ix) = self.cat.indexes.get(i)
                && !t.constraints.iter().any(|c| c.kind.index() == Some(ix.oid))
                && index_uses(ix)
                && ix.parent == 0
            {
                ixs.push(ix.oid);
            }
        }
        cons.sort_by_key(|c| c.1);
        ixs.sort();
        let pos = match self.type_rebuilds.iter().position(|r| r.table == t.oid) {
            Some(p) => p,
            None => {
                self.type_rebuilds.push(TypeRebuild {
                    table: t.oid,
                    cons: Vec::new(),
                    indexes: Vec::new(),
                    old_cols: (t.columns.iter())
                        .map(|c| (c.ty.clone(), c.collation.clone()))
                        .collect(),
                    local_not_nulls: Vec::new(),
                });
                self.type_rebuilds.len() - 1
            }
        };
        let r = &mut self.type_rebuilds[pos];
        if t.partbound.is_none() {
            for c in &t.constraints {
                if let ConstraintKind::NotNull { column } = c.kind
                    && column == idx
                    && c.islocal
                    && c.inhcount > 0
                    && !r.local_not_nulls.iter().any(|x| x.0 == column)
                {
                    r.local_not_nulls.push((column, c.name.clone()));
                }
            }
        }
        for (owner, oid, not_null) in cons {
            if r.cons.iter().any(|c| c.1 == oid) {
                continue;
            }
            if not_null {
                r.cons.insert(0, (owner, oid));
            } else {
                r.cons.push((owner, oid));
            }
        }
        for i in ixs {
            if !r.indexes.contains(&i) {
                r.indexes.push(i);
            }
        }
    }

    /// Re-create what ALTER COLUMN TYPE remembered (ATPostAlterTypeCleanup's re-added
    /// definitions), each object taking new oids: the AT_PASS_OLD_INDEX pass (per table,
    /// index-backed constraints, the index's oid before the constraint's, then plain indexes),
    /// then AT_PASS_OLD_CONSTR (per table, the other constraints, a foreign key with its four
    /// triggers, a check or not-null one recursing to the children inheriting it; then the
    /// foreign keys of other tables). `t` is the table in hand, if any.
    pub(super) fn flush_type_rebuilds(&mut self, mut t: Option<&mut Table>) -> Result<()> {
        let rebuilds = std::mem::take(&mut self.type_rebuilds);
        if rebuilds.is_empty() {
            return Ok(());
        }
        // Work on catalog copies of the tables; the one in hand is put back at the end.
        let mut tables: Vec<Table> = Vec::new();
        let get = get_table;
        let mut remap: Vec<(u32, u32)> = Vec::new();
        // AT_PASS_OLD_INDEX.
        for r in &rebuilds {
            let Some(ti) = get(&self.cat, &t, &mut tables, r.table) else {
                continue;
            };
            let index_cons = r.cons.iter().filter(|c| c.0 == r.table).filter_map(|c| {
                let con = tables[ti].constraints.iter().find(|x| x.oid == c.1)?;
                con.kind.index().map(|i| (c.1, i))
            });
            let mut items: Vec<(Option<u32>, u32)> =
                index_cons.map(|(c, i)| (Some(c), i)).collect();
            items.extend(r.indexes.iter().map(|i| (None, *i)));
            for (con, ix) in items {
                // Without a rewrite, an index that still fits keeps its file.
                let excl = con.and_then(|c| {
                    tables[ti].constraints.iter().find(|x| x.oid == c).and_then(|x| match &x.kind {
                        ConstraintKind::Exclusion { ops, .. } => Some(ops.clone()),
                        _ => None,
                    })
                });
                let reuse = !self.rewrites.contains(&r.table)
                    && (self.cat.indexes.get(&ix)).is_some_and(|o| {
                        index_compatible(o, &r.old_cols, &tables[ti], excl.as_deref())
                    });
                let tb = &mut tables[ti];
                let Some(new) = self.renumber_index(tb, ix) else {
                    continue;
                };
                if reuse {
                    let node = self.cat.filenodes.get(&ix).copied().unwrap_or(ix);
                    self.cat.filenodes.insert(new, node);
                }
                let mut new_con = 0;
                if let Some(con) = con {
                    let deferrable = tb.constraints.iter().any(|x| x.oid == con && x.deferrable);
                    new_con = self.cat.alloc_con_oid(deferrable);
                    if let Some(c) = tb.constraints.iter_mut().find(|x| x.oid == con) {
                        c.oid = new_con;
                    }
                    remap.push((con, new_con));
                }
                // A partitioned index makes its partitions' anew (DefineIndex recursing, depth
                // first in bound order), each named as a new one and with its constraint.
                if tb.partkey.is_none() {
                    continue;
                }
                let mut stack: Vec<(u32, u32, u32)> = (self.cat.partitions(tb.oid).into_iter())
                    .rev()
                    .map(|p| (p, new, new_con))
                    .collect();
                while let Some((part, pix, pcon)) = stack.pop() {
                    let Some(ci) = get(&self.cat, &t, &mut tables, part) else {
                        continue;
                    };
                    let Some(old) = (self.cat.indexes.values())
                        .find(|x| x.parent == pix && x.table == part)
                        .cloned()
                    else {
                        continue;
                    };
                    let ct = &mut tables[ci];
                    let Some(cnew) = self.renumber_index(ct, old.oid) else {
                        continue;
                    };
                    // (Made anew by DefineIndex's recursion: no comment comes along.)
                    self.cat.comments_mut().retain(|c| c.0 != cnew && c.0 != old.oid);
                    let con_kind = (ct.constraints.iter())
                        .find(|c| c.kind.index() == Some(cnew))
                        .map(|c| c.kind.clone());
                    let refs = super::partition::index_name_parts(ct, &old);
                    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
                    let base = match &con_kind {
                        Some(ConstraintKind::PrimaryKey { .. }) => {
                            Catalog::object_name(&ct.name, &[], "pkey")
                        }
                        Some(ConstraintKind::Exclusion { .. }) => {
                            Catalog::object_name(&ct.name, &refs, "excl")
                        }
                        Some(_) => Catalog::object_name(&ct.name, &refs, "key"),
                        None => Catalog::object_name(&ct.name, &refs, "idx"),
                    };
                    self.cat.remove_name(ct.schema, &old.name);
                    let name = self.cat.choose_name(ct.schema, &base);
                    if let Some(ix) = self.cat.indexes.get(&cnew) {
                        let mut ix = (**ix).clone();
                        ix.name = name.clone();
                        self.cat.put_index(ix);
                    }
                    let mut ccon = 0;
                    if let Some(c) = ct.constraints.iter_mut().find(|c| c.kind.index() == Some(cnew)) {
                        let o = c.oid;
                        self.cat.comments_mut().retain(|x| x.0 != o);
                        ccon = self.cat.alloc_con_oid(c.deferrable);
                        c.oid = ccon;
                        c.name = name;
                        c.parent = pcon;
                        remap.push((o, ccon));
                    }
                    if ct.partkey.is_some() {
                        let subs = self.cat.partitions(part);
                        stack.extend(subs.into_iter().rev().map(|p| (p, cnew, ccon)));
                    }
                }
            }
        }
        // AT_PASS_OLD_CONSTR: the altered tables', then other tables' foreign keys.
        let mut order: Vec<u32> = rebuilds.iter().map(|r| r.table).collect();
        for r in &rebuilds {
            for c in &r.cons {
                if !order.contains(&c.0) {
                    order.push(c.0);
                }
            }
        }
        for owner in order {
            for r in &rebuilds {
                for &(o, con) in &r.cons {
                    if o != owner {
                        continue;
                    }
                    let Some(ti) = get(&self.cat, &t, &mut tables, owner) else {
                        continue;
                    };
                    let Some(c) = tables[ti].constraints.iter_mut().find(|x| x.oid == con)
                    else {
                        continue;
                    };
                    if c.kind.index().is_some() {
                        continue;
                    }
                    let fk = matches!(c.kind, ConstraintKind::ForeignKey { .. });
                    if fk {
                        self.rebuild_fk(&mut tables, &t, ti, con, &mut remap);
                        continue;
                    }
                    let new = self.cat.alloc_oid();
                    c.oid = new;
                    remap.push((con, new));
                    // ATAddCheckNNConstraint recurses: the children's inherited copies.
                    let (kind, cname) = (c.kind.clone(), c.name.clone());
                    let col = match &kind {
                        ConstraintKind::NotNull { column } => {
                            Some(tables[ti].columns[*column].name.clone())
                        }
                        _ => None,
                    };
                    let mut stack: Vec<u32> =
                        self.cat.children(owner).iter().rev().copied().collect();
                    while let Some(child) = stack.pop() {
                        stack.extend(self.cat.children(child).iter().rev().copied());
                        let Some(ci) = get(&self.cat, &t, &mut tables, child) else {
                            continue;
                        };
                        let ct = &mut tables[ci];
                        let found = ct.constraints.iter().position(|x| {
                            x.inhcount > 0
                                && match (&x.kind, &col) {
                                    (ConstraintKind::NotNull { column }, Some(n)) => {
                                        ct.columns[*column].name == *n
                                    }
                                    (ConstraintKind::Check { .. }, None) => x.name == cname,
                                    _ => false,
                                }
                        });
                        if let Some(k) = found {
                            let old = ct.constraints[k].oid;
                            let new = self.cat.alloc_oid();
                            ct.constraints[k].oid = new;
                            // A not-null constraint is made anew under the parent's name.
                            if col.is_some() {
                                ct.constraints[k].name = cname.clone();
                            }
                            remap.push((old, new));
                        }
                    }
                }
            }
        }
        // An inheritor's own not-null constraint merged with its parent's is added again
        // (ADD CONSTRAINT name NOT NULL), after the parent's came back under the parent's name.
        for r in &rebuilds {
            for (col, name) in &r.local_not_nulls {
                let Some(ti) = get(&self.cat, &t, &mut tables, r.table) else {
                    continue;
                };
                let tb = &tables[ti];
                if let Some(c) = (tb.constraints.iter()).find(
                    |c| matches!(c.kind, ConstraintKind::NotNull { column } if column == *col),
                ) && c.name != *name
                {
                    return Err(Error::new(
                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!(
                            "cannot create not-null constraint \"{name}\" on column \"{}\" of table \"{}\"",
                            tb.columns[*col].name, tb.name
                        ),
                    )
                    .detail(format!(
                        "A not-null constraint named \"{}\" already exists for this column.",
                        c.name
                    )));
                }
            }
        }
        for (old, new) in remap {
            for c in self.cat.comments_mut().iter_mut().filter(|c| c.0 == old) {
                c.0 = new;
            }
        }
        for tb in tables {
            match &mut t {
                Some(t) if t.oid == tb.oid => **t = tb,
                _ => self.cat.put_table(tb),
            }
        }
        Ok(())
    }
}

impl Ddl<'_, '_> {
    /// Re-create foreign key `con` of `tables[ti]` with new oids (ATAddForeignKeyConstraint):
    /// the key and its action triggers, the constraints derived for the referenced table's
    /// partitions (each with its action triggers), its check triggers, then its partitions'
    /// copies (each with check triggers). `get` finds or loads a table's working copy.
    fn rebuild_fk(
        &mut self,
        tables: &mut Vec<Table>,
        t: &Option<&mut Table>,
        ti: usize,
        con: u32,
        remap: &mut Vec<(u32, u32)>,
    ) {
        let Some(k) = tables[ti].constraints.iter().position(|x| x.oid == con) else {
            return;
        };
        let enforced = tables[ti].constraints[k].enforced;
        let ri_check = tables[ti].constraints[k].ri_check;
        // Derived constraints, in creation (oid) order, of this one transitively.
        let mut derived: Vec<u32> = Vec::new();
        let mut frontier = vec![con];
        while let Some(p) = frontier.pop() {
            for c in tables[ti].constraints.iter().filter(|c| c.parent == p) {
                derived.push(c.oid);
                frontier.push(c.oid);
            }
        }
        derived.sort_unstable();
        let mut map: Vec<(u32, u32)> = Vec::new();
        let new = if !enforced {
            self.cat.alloc_oid()
        } else if ri_check != 0 || !derived.is_empty() {
            self.cat.alloc_oids(3)
        } else {
            self.cat.alloc_oids(5)
        };
        map.push((con, new));
        for d in &derived {
            map.push((*d, self.cat.alloc_oids(3)));
        }
        let new_check = if enforced && (ri_check != 0 || !derived.is_empty()) {
            self.cat.alloc_oids(2) - 1
        } else {
            0
        };
        let old_check = if ri_check != 0 { ri_check } else { con + 2 };
        let look = |m: &[(u32, u32)], o: u32| m.iter().find(|x| x.0 == o).map(|x| x.1);
        for c in tables[ti].constraints.iter_mut() {
            let Some(n) = look(&map, c.oid) else {
                continue;
            };
            let o = c.oid;
            for e in &mut c.ri_enabled {
                if e.0 > o && e.0 - o < 3 {
                    e.0 = n + (e.0 - o);
                } else if o == con && e.0 > old_check && e.0 - old_check < 3 {
                    e.0 = match new_check {
                        0 => n + 2 + (e.0 - old_check),
                        b => b + (e.0 - old_check),
                    };
                }
            }
            c.oid = n;
            if o == con {
                c.ri_check = new_check;
            }
            if let Some(p) = look(&map, c.parent) {
                c.parent = p;
            }
            remap.push((o, n));
        }
        // The partitions' copies, depth first in bound order.
        let owner = tables[ti].oid;
        let mut stack: Vec<(u32, u32, u32)> = (self.cat.partitions(owner).into_iter())
            .rev()
            .map(|p| (p, con, new))
            .collect();
        while let Some((part, pold, pnew)) = stack.pop() {
            let Some(ci) = get_table(&self.cat, t, tables, part) else {
                continue;
            };
            let Some(c) = tables[ci].constraints.iter_mut().find(|c| c.parent == pold) else {
                continue;
            };
            let o = c.oid;
            let n = self.cat.alloc_oids(if enforced { 3 } else { 1 });
            for e in &mut c.ri_enabled {
                if e.0 > o && e.0 - o < 3 {
                    e.0 = n + (e.0 - o);
                }
            }
            c.oid = n;
            c.parent = pnew;
            remap.push((o, n));
            if tables[ci].partkey.is_some() {
                let subs = self.cat.partitions(part);
                stack.extend(subs.into_iter().rev().map(|p| (p, o, n)));
            }
        }
    }

    /// Give index `ix` of `tb` a new oid, as re-creating it does (its comment and statistics
    /// follow; a constraint it backs points at it anew). None: no such index.
    pub(super) fn renumber_index(&mut self, tb: &mut Table, ix: u32) -> Option<u32> {
        let old = self.cat.indexes.remove(&ix)?;
        let new = self.cat.alloc_oid();
        for i in tb.indexes.iter_mut().chain(tb.pkey_index.iter_mut()) {
            if *i == ix {
                *i = new;
            }
        }
        for c in &mut tb.constraints {
            match &mut c.kind {
                ConstraintKind::PrimaryKey { index }
                | ConstraintKind::Unique { index }
                | ConstraintKind::Exclusion { index, .. }
                    if *index == ix =>
                {
                    *index = new
                }
                _ => {}
            }
        }
        // Partitions' indexes attached to it.
        let children: Vec<Arc<Index>> = (self.cat.indexes.values())
            .filter(|c| c.parent == ix)
            .cloned()
            .collect();
        for c in children {
            self.cat.put_index(Index {
                parent: new,
                ..(*c).clone()
            });
        }
        self.cat.put_index(Index {
            oid: new,
            ..(*old).clone()
        });
        for c in self.cat.comments_mut().iter_mut().filter(|c| c.0 == ix) {
            c.0 = new;
        }
        if let Some(s) = self.cat.relstats.remove(&ix) {
            self.cat.relstats.insert(new, s);
        }
        if let Some(s) = self.cat.colstats.remove(&ix) {
            self.cat.colstats.insert(new, s);
        }
        if let Some(s) = self.cat.rel_tablespace.remove(&ix) {
            self.cat.rel_tablespace.insert(new, s);
        }
        Some(new)
    }

    /// REINDEX: each index gets a new file (an oid), a table's TOAST index too; CONCURRENTLY
    /// builds a copy of each index, which takes the original's place with its new oid.
    /// Partitioned tables and indexes reindex their partitions'. System catalogs' indexes get
    /// new files too (REINDEX SYSTEM, or naming one), never concurrently.
    /// REINDEX TABLE / INDEX naming a user table's TOAST table or TOAST index (`index_stmt`:
    /// REINDEX INDEX). None: `rv` names neither. Its locks are reindex_relation's /
    /// ReindexRelationConcurrently's on the TOAST table and its index; a TOAST table dropped
    /// while the lock was waited for is gone (RangeVarGetRelidExtended looks again).
    fn reindex_toast(
        &mut self,
        rv: &pb::RangeVar,
        index_stmt: bool,
        concurrently: bool,
        verbose: bool,
        spc: Option<u32>,
    ) -> Result<Option<String>> {
        let schema = Self::rv_schema(rv);
        let Some((t, index)) = self.cat.find_toast(schema, &rv.relname, self.search_path()) else {
            return Ok(None);
        };
        if index_stmt && !index {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not an index", rv.relname),
            ));
        }
        if !index_stmt && index {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a table or materialized view", rv.relname),
            ));
        }
        // MAINTAIN on the TOAST relation: its table's owner's (it has no ACL of its own).
        let me = self.me();
        if !self.cat.is_superuser(me)
            && !self.cat.has_privs_of_role(me, t.owner, self.db_owner())
            && !self.cat.has_privs_of_role(me, crate::catalog::acl::PG_MAINTAIN, self.db_owner())
        {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!(
                    "permission denied for {} {}",
                    if index { "index" } else { "table" },
                    rv.relname
                ),
            ));
        }
        let (toast, toast_index) = (t.toast, t.toast + 1);
        if spc.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot move system relation \"{}\"", t.toast_relname(true)),
            ));
        }
        let (heap_mode, index_mode) = if concurrently {
            (lockmode::SHARE_UPDATE_EXCLUSIVE, lockmode::SHARE_UPDATE_EXCLUSIVE)
        } else {
            (lockmode::SHARE, lockmode::ACCESS_EXCLUSIVE)
        };
        let relname = t.toast_relname(false);
        self.ctx.txn.borrow_mut().lock_relation(
            toast,
            crate::txn::locks::LockMode::from_number(heap_mode).expect("a lock mode"),
            crate::txn::locks::WaitPolicy::Block,
            &|| relname.clone(),
        )?;
        // (Dropped, or renamed, while the lock was waited for.)
        self.refresh_catalog();
        let still = self
            .cat
            .find_toast(schema, &rv.relname, self.search_path())
            .is_some_and(|(t2, ix)| t2.toast == toast && ix == index);
        if !still {
            return Err(crate::catalog::no_such_relation(schema, &rv.relname));
        }
        self.note_lock(toast_index, index_mode)?;
        let info = |d: &Self, msg: String| {
            let mut e = crate::session::notice(msg);
            e.severity = crate::error::Severity::Info;
            d.ctx.notice(e);
        };
        let name = t.toast_relname(true);
        if concurrently && !t.temporary {
            // ReindexRelationConcurrently's phases: writers out (ShareLock), then every
            // transaction using the TOAST table before the swap (AccessExclusiveLock).
            use crate::txn::locks::LockMode as L;
            self.ctx.txn.borrow_mut().wait_for_lockers(toast, L::Share)?;
            self.ctx.txn.borrow_mut().wait_for_lockers(toast, L::AccessExclusive)?;
            // (The copy built takes an oid; the engine's TOAST index keeps its own.)
            self.cat.alloc_oid();
            self.cat.new_filenode(toast_index);
            if verbose {
                let sch = self.cat.schema_name(self.cat.toast_schema(&t)).to_string();
                info(self, format!("index \"{sch}.{name}\" was reindexed"));
            }
        } else {
            self.cat.new_filenode(toast_index);
            if verbose {
                info(self, format!("index \"{name}\" was reindexed"));
            }
        }
        self.save()?;
        Ok(Some("REINDEX".into()))
    }

    pub(super) fn reindex(&mut self, s: &pb::ReindexStmt) -> Result<String> {
        use pb::ReindexObjectType as K;
        let concurrently = def_elems(&s.params).any(|d| {
            d.defname == "concurrently"
                && !matches!(
                    d.arg.as_deref().and_then(inner),
                    Some(NodeEnum::String(v)) if v.sval == "false"
                )
        });
        if concurrently {
            super::prevent_in_block("REINDEX CONCURRENTLY")?;
        }
        let verbose = def_elems(&s.params).any(|d| {
            d.defname == "verbose"
                && !matches!(
                    d.arg.as_deref().and_then(inner),
                    Some(NodeEnum::String(v)) if v.sval == "false"
                )
        });
        // ReindexMultipleTables runs outside transaction blocks only.
        if !concurrently {
            match K::try_from(s.kind).unwrap_or(K::Undefined) {
                K::ReindexObjectSchema => super::prevent_in_block("REINDEX SCHEMA")?,
                K::ReindexObjectSystem => super::prevent_in_block("REINDEX SYSTEM")?,
                K::ReindexObjectDatabase => super::prevent_in_block("REINDEX DATABASE")?,
                _ => {}
            }
        }
        // (RangeVarCallbackForReindexIndex / ReindexTable: a system relation needs MAINTAIN,
        // which only superusers and pg_maintain have.)
        let may_maintain_system = self.cat.is_superuser(self.me())
            || self.cat.has_privs_of_role(
                self.me(),
                crate::catalog::acl::PG_MAINTAIN,
                self.db_owner(),
            );
        let spc = self.reindex_tablespace_param(&s.params)?;
        // A system relation can't move (RelationIsMapped / IsSystemRelation).
        let no_move = |rel: u32| -> Result<()> {
            match spc {
                Some(_) => Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "cannot move system relation \"{}\"",
                        super::tblspc::system_index_name(rel)
                    ),
                )),
                None => Ok(()),
            }
        };
        // (table, its indexes to rebuild, with its TOAST index).
        let mut work: Vec<(u32, Vec<u32>, bool)> = Vec::new();
        let storage_indexes = |t: &Table| -> Vec<u32> {
            let mut v: Vec<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
            v.sort();
            v
        };
        let leaves = |cat: &Catalog, t: &Table| -> Vec<Table> {
            if t.partkey.is_some() {
                cat.leaves(t.oid).iter().map(|l| (**l).clone()).collect()
            } else {
                vec![t.clone()]
            }
        };
        match K::try_from(s.kind).unwrap_or(K::Undefined) {
            K::ReindexObjectIndex => {
                let rv = s.relation.as_ref().unwrap();
                let ix = match self.lookup(rv)? {
                    Some(Relation::Index(ix)) => ix,
                    Some(_) => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("\"{}\" is not an index", rv.relname),
                        ));
                    }
                    None => {
                        if let Some(r) = self.reindex_toast(rv, true, concurrently, verbose, spc)? {
                            return Ok(r);
                        }
                        match system_relation(&self.cat, rv) {
                        Some((oid, b'i')) => {
                            if !may_maintain_system {
                                return Err(Error::new(
                                    code::INSUFFICIENT_PRIVILEGE,
                                    format!("permission denied for index {}", rv.relname),
                                ));
                            }
                            if concurrently {
                                return Err(system_concurrently());
                            }
                            no_move(oid)?;
                            self.cat.new_filenode(oid);
                            self.save()?;
                            return Ok("REINDEX".into());
                        }
                        Some(_) => {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!("\"{}\" is not an index", rv.relname),
                            ));
                        }
                        None => {
                            return Err(crate::catalog::no_such_relation(
                                Self::rv_schema(rv),
                                &rv.relname,
                            ));
                        }
                        }
                    }
                };
                let Some(t) = self.cat.index_table(ix.table) else {
                    return Ok("REINDEX".into());
                };
                // RangeVarCallbackForReindexIndex: MAINTAIN on the table.
                if self
                    .require_rel_priv(t.oid, crate::catalog::acl::MAINTAIN, false)
                    .is_err()
                {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for index {}", ix.name),
                    ));
                }
                // (index_concurrently_create_copy: no exclusion constraints.)
                if concurrently
                    && !t.temporary
                    && t.constraints.iter().any(|c| {
                        c.kind.index() == Some(ix.oid)
                            && matches!(c.kind, ConstraintKind::Exclusion { .. })
                    })
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "concurrent index creation for exclusion constraints is not supported",
                    ));
                }
                if t.partkey.is_some() && !concurrently {
                    super::prevent_in_block("REINDEX INDEX").map_err(|e| {
                        e.context(format!(
                            "while reindexing partitioned index \"{}.{}\"",
                            self.cat.schema_name(t.schema),
                            ix.name
                        ))
                    })?;
                }
                for l in leaves(&self.cat, &t) {
                    let mine: Vec<u32> = storage_indexes(&l)
                        .into_iter()
                        .filter(|i| {
                            let mut cur = Some(*i);
                            while let Some(c) = cur {
                                if c == ix.oid {
                                    return true;
                                }
                                cur = (self.cat.indexes.get(&c))
                                    .map(|x| x.parent)
                                    .filter(|p| *p != 0);
                            }
                            false
                        })
                        .collect();
                    work.push((l.oid, mine, false));
                }
            }
            K::ReindexObjectTable => {
                let rv = s.relation.as_ref().unwrap();
                let t = match self.lookup(rv)? {
                    Some(Relation::Table(t)) => (*t).clone(),
                    Some(Relation::View(v)) if v.storage.is_some() => {
                        (**v.storage.as_ref().unwrap()).clone()
                    }
                    Some(_) => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!(
                                "\"{}\" is not a table or materialized view",
                                rv.relname
                            ),
                        ));
                    }
                    None => {
                        if let Some(r) = self.reindex_toast(rv, false, concurrently, verbose, spc)? {
                            return Ok(r);
                        }
                        match system_relation(&self.cat, rv) {
                        Some((oid, b'r' | b't')) => {
                            if !may_maintain_system {
                                return Err(Error::new(
                                    code::INSUFFICIENT_PRIVILEGE,
                                    format!("permission denied for table {}", rv.relname),
                                ));
                            }
                            if concurrently {
                                return Err(system_concurrently());
                            }
                            no_move(oid)?;
                            for ix in system_reindex_order(oid) {
                                self.cat.new_filenode(ix);
                            }
                            self.save()?;
                            return Ok("REINDEX".into());
                        }
                        Some(_) => {
                            return Err(Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!(
                                    "\"{}\" is not a table or materialized view",
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
                        }
                    }
                };
                self.require_rel_priv(t.oid, crate::catalog::acl::MAINTAIN, false)?;
                if t.partkey.is_some() && !concurrently {
                    super::prevent_in_block("REINDEX TABLE").map_err(|e| {
                        e.context(format!(
                            "while reindexing partitioned table \"{}.{}\"",
                            self.cat.schema_name(t.schema),
                            t.name
                        ))
                    })?;
                }
                let ls = leaves(&self.cat, &t);
                // ReindexRelationConcurrently skips the indexes a failed concurrent build
                // left INVALID.
                let mut skip: Vec<u32> = Vec::new();
                if concurrently && t.partkey.is_none() && !t.temporary {
                    for i in storage_indexes(&t) {
                        if t.constraints.iter().any(|c| {
                            c.kind.index() == Some(i)
                                && matches!(c.kind, ConstraintKind::Exclusion { .. })
                        }) && let Some(ix) = self.cat.indexes.get(&i)
                        {
                            self.ctx.notice(crate::session::warning(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "cannot reindex exclusion constraint index \"{}.{}\" concurrently, skipping",
                                    self.cat.schema_name(t.schema),
                                    ix.name
                                ),
                            ));
                            skip.push(i);
                            continue;
                        }
                        if let Some(ix) = self.cat.indexes.get(&i).filter(|x| !x.valid) {
                            self.ctx.notice(
                                crate::session::warning(
                                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                    format!(
                                        "skipping reindex of invalid index \"{}.{}\"",
                                        self.cat.schema_name(t.schema),
                                        ix.name
                                    ),
                                )
                                .hint("Use DROP INDEX or REINDEX INDEX."),
                            );
                            skip.push(i);
                        }
                    }
                }
                let has = storage_indexes(&t).iter().any(|i| !skip.contains(i));
                if t.partkey.is_none() && !has && t.toast == 0 {
                    self.notice(if concurrently {
                        format!(
                            "table \"{}\" has no indexes that can be reindexed concurrently",
                            t.name
                        )
                    } else {
                        format!("table \"{}\" has no indexes to reindex", t.name)
                    });
                }
                for l in ls {
                    let ixs = storage_indexes(&l).into_iter().filter(|i| !skip.contains(i)).collect();
                    work.push((l.oid, ixs, l.toast != 0));
                }
            }
            K::ReindexObjectSystem => {
                if concurrently {
                    return Err(system_concurrently());
                }
                if !s.name.is_empty() && s.name != self.ctx.sess.database {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "can only reindex the currently open database",
                    ));
                }
                for t in system_catalogs() {
                    for ix in system_reindex_order(t) {
                        self.cat.new_filenode(ix);
                    }
                }
            }
            K::ReindexObjectSchema | K::ReindexObjectDatabase => {
                let database = K::try_from(s.kind) == Ok(K::ReindexObjectDatabase);
                if database && !s.name.is_empty() && s.name != self.ctx.sess.database {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "can only reindex the currently open database",
                    ));
                }
                // REINDEX SCHEMA pg_catalog: the system catalogs (in pg_class order), none
                // concurrently.
                if !database && s.name == "pg_catalog" {
                    if concurrently {
                        self.ctx.notice(crate::session::warning(
                            code::FEATURE_NOT_SUPPORTED,
                            "cannot reindex system catalogs concurrently, skipping all",
                        ));
                    } else {
                        for t in system_catalogs() {
                            for ix in system_reindex_order(t) {
                                self.cat.new_filenode(ix);
                            }
                        }
                    }
                }
                let schema = if K::try_from(s.kind) == Ok(K::ReindexObjectSchema) {
                    match self.cat.schema_by_name(&s.name).cloned() {
                        Some(sc) => {
                            if !self.cat.has_privs_of_role(
                                self.me(),
                                crate::catalog::acl::PG_MAINTAIN,
                                self.db_owner(),
                            ) {
                                self.require_owner(sc.owner, "schema", &sc.name)?;
                            }
                            Some(sc.oid)
                        }
                        None => {
                            return Err(Error::new(
                                code::UNDEFINED_SCHEMA,
                                format!("schema \"{}\" does not exist", s.name),
                            ));
                        }
                    }
                } else {
                    None
                };
                // information_schema's tables (not system catalogs) come first.
                if !concurrently {
                    for t in builtin_tables(schema) {
                        for ix in system_reindex_order(t) {
                            self.cat.new_filenode(ix);
                        }
                    }
                }
                let tables: Vec<Table> = (self.cat.tables.values())
                    .map(|t| (**t).clone())
                    .chain(
                        (self.cat.views.values())
                            .filter_map(|v| v.storage.as_ref().map(|t| (**t).clone())),
                    )
                    .filter(|t| t.partkey.is_none() && schema.is_none_or(|sc| t.schema == sc))
                    .collect();
                // In the order a pg_class heap scan returns them.
                let mut tables = tables;
                tables.sort_by_key(|t| self.cat.class_scan_key(t.oid));
                for t in tables {
                    work.push((t.oid, storage_indexes(&t), t.toast != 0));
                }
            }
            _ => {}
        }
        for (table, indexes, toast) in work {
            // reindex_index / ReindexRelationConcurrently: SHARE on the table and ACCESS
            // EXCLUSIVE on its indexes (CONCURRENTLY: SHARE UPDATE EXCLUSIVE on both).
            let (heap_mode, index_mode) = if concurrently {
                (lockmode::SHARE_UPDATE_EXCLUSIVE, lockmode::SHARE_UPDATE_EXCLUSIVE)
            } else {
                (lockmode::SHARE, lockmode::ACCESS_EXCLUSIVE)
            };
            self.note_lock(table, heap_mode)?;
            // (ReindexMultipleTables: a table dropped while we waited for it is skipped.)
            if !self.ctx.sess.db.catalog().tables.contains_key(&table)
                && self.ctx.txn.borrow().catalog_base().is_none_or(|b| b.tables.contains_key(&table))
                && self.cat.tables.get(&table).is_some_and(|t| !t.temporary)
            {
                continue;
            }
            for &i in &indexes {
                self.note_lock(i, index_mode)?;
            }
            // Its TOAST table and index (reindex_relation's recursion, or
            // ReindexRelationConcurrently's toast indexes).
            let toast_rel = if toast { self.cat.index_table(table).map_or(0, |t| t.toast) } else { 0 };
            if toast_rel != 0 {
                self.note_lock(toast_rel, heap_mode)?;
                self.note_lock(toast_rel + 1, index_mode)?;
            }
            // ReindexRelationConcurrently's phases: writers out before the build and again
            // before the validation (ShareLock), the transactions with older snapshots
            // (WaitForOlderSnapshots), then every transaction using the table before the
            // swap and again before the old indexes go (AccessExclusiveLock): each wait is
            // for the lockers there are when it begins.
            if concurrently {
                use crate::txn::locks::LockMode as L;
                let rels: Vec<u32> = std::iter::once(table).chain((toast_rel != 0).then_some(toast_rel)).collect();
                let mut txn = self.ctx.txn.borrow_mut();
                for _ in 0..2 {
                    for &r in &rels {
                        txn.wait_for_lockers(r, L::Share)?;
                    }
                }
                txn.wait_for_older_snapshots()?;
                for _ in 0..2 {
                    for &r in &rels {
                        txn.wait_for_lockers(r, L::AccessExclusive)?;
                    }
                }
            }
            if let Some(spc) = spc {
                self.reindex_move(&indexes, spc, concurrently)?;
            }
            // An INVALID index is built again (and made valid), or fails as it did.
            if let Some(t) = self.cat.index_table(table).map(|t| (*t).clone())
                && t.partkey.is_none()
            {
                let valid: Vec<u32> = indexes
                    .iter()
                    .copied()
                    .filter(|i| self.cat.indexes.get(i).is_some_and(|x| x.valid))
                    .collect();
                for &i in &indexes {
                    let Some(ix) = self.cat.indexes.get(&i).filter(|x| !x.valid).cloned() else {
                        continue;
                    };
                    let ix = (*ix).clone();
                    if concurrently {
                        // (The copy it builds is committed first: left INVALID when its
                        // build fails.)
                        let copy_name = format!("{}_ccnew", ix.name);
                        if let Some(e) = self.index_duplicate(&t, &Index {
                            name: copy_name.clone(),
                            ..ix.clone()
                        })? {
                            let mut tt = t.clone();
                            let oid = self.cat.alloc_oid();
                            let copy = Index {
                                oid,
                                name: copy_name,
                                id: tt.new_index_id(oid),
                                valid: false,
                                ..ix.clone()
                            };
                            tt.indexes.push(oid);
                            self.cat.put_storage(tt);
                            self.cat.put_index(copy);
                            self.save()?;
                            self.ctx.sess.commit_on_error.set(true);
                            return Err(e);
                        }
                    }
                    self.clear_index_entries(&t, &ix)?;
                    self.build_index(&t, &ix)?;
                    self.cat.put_index(Index { valid: true, ..ix });
                }
                // The valid ones are rebuilt too: their expressions are computed.
                self.evaluate_index_exprs(&t, Some(&valid))?;
            }
            // A temporary table's indexes are rebuilt in place even CONCURRENTLY.
            let temp = self.cat.index_table(table).is_some_and(|t| t.temporary);
            let info = |d: &Self, msg: String| {
                let mut e = crate::session::notice(msg);
                e.severity = crate::error::Severity::Info;
                d.ctx.notice(e);
            };
            if concurrently && !temp {
                let Some(mut t) = self.cat.index_table(table).map(|t| (*t).clone()) else {
                    continue;
                };
                for ix in indexes {
                    self.renumber_index(&mut t, ix);
                    if verbose && let Some(x) = self.cat.indexes.get(&ix) {
                        info(
                            self,
                            format!(
                                "index \"{}.{}\" was reindexed",
                                self.cat.schema_name(t.schema),
                                x.name
                            ),
                        );
                    }
                }
                self.cat.put_storage(t);
                if toast {
                    self.cat.alloc_oid();
                }
            } else {
                // reindex_relation does the TOAST table's index first.
                let toast_rel = self.cat.index_table(table).map_or(0, |t| t.toast);
                if toast && toast_rel != 0 {
                    self.cat.new_filenode(toast_rel + 1);
                    if verbose {
                        info(self, format!("index \"pg_toast_{table}_index\" was reindexed"));
                    }
                }
                for ix in indexes {
                    self.cat.new_filenode(ix);
                    if verbose && let Some(x) = self.cat.indexes.get(&ix) {
                        info(self, format!("index \"{}\" was reindexed", x.name));
                    }
                }
            }
        }
        self.save()?;
        Ok("REINDEX".into())
    }
}

/// RangeVarCallbackForAlterRelation: ALTER FOREIGN TABLE names a foreign table (`kind` is
/// the statement's object type, `relkind` the relation type of a column rename).
fn foreign_kind_check(rel: &Relation, kind: pb::ObjectType, relkind: i32, name: &str) -> Result<()> {
    let foreign_stmt = kind == pb::ObjectType::ObjectForeignTable
        || relkind == pb::ObjectType::ObjectForeignTable as i32;
    if foreign_stmt && !matches!(rel, Relation::Table(t) if t.is_foreign()) {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is not a foreign table"),
        ));
    }
    Ok(())
}

/// A system catalog relation (or TOAST table, or index) named by `rv`: (oid, relkind).
fn system_relation(cat: &Catalog, rv: &pb::RangeVar) -> Option<(u32, u8)> {
    use crate::catalog::static_rows::{oid_at, rows, text_at};
    let ns = if rv.schemaname.is_empty() {
        crate::catalog::PG_CATALOG_OID
    } else {
        cat.schema_by_name(&rv.schemaname)?.oid
    };
    rows(system::SysTable::PgClass)
        .iter()
        .find(|r| oid_at(r, 2) == ns && text_at(r, 1) == rv.relname)
        .map(|r| {
            let kind = match r.get(17) {
                Some(Value::Char(c)) => *c,
                _ => 0,
            };
            (oid_at(r, 0), kind)
        })
}

/// The system catalogs REINDEX SYSTEM goes through: pg_catalog's tables, in pg_class order.
fn system_catalogs() -> Vec<u32> {
    use crate::catalog::static_rows::{oid_at, rows};
    rows(system::SysTable::PgClass)
        .iter()
        .filter(|r| {
            oid_at(r, 2) == crate::catalog::PG_CATALOG_OID
                && matches!(r.get(17), Some(Value::Char(b'r')))
        })
        .map(|r| oid_at(r, 0))
        .collect()
}

/// The built-in tables that aren't system catalogs (information_schema's), in pg_class
/// order; only those in `schema` if given.
fn builtin_tables(schema: Option<u32>) -> Vec<u32> {
    use crate::catalog::static_rows::{oid_at, rows};
    rows(system::SysTable::PgClass)
        .iter()
        .filter(|r| {
            let ns = oid_at(r, 2);
            ns != crate::catalog::PG_CATALOG_OID
                && ns != crate::catalog::PG_TOAST_OID
                && schema.is_none_or(|s| s == ns)
                && matches!(r.get(17), Some(Value::Char(b'r')))
        })
        .map(|r| oid_at(r, 0))
        .collect()
}

/// The indexes reindex_relation gives new files for a system table (or TOAST table): its
/// TOAST table's index first, then its own in oid order.
fn system_reindex_order(table: u32) -> Vec<u32> {
    use crate::catalog::static_rows::{oid_at, rows};
    let class = rows(system::SysTable::PgClass);
    let toast = class
        .iter()
        .find(|r| oid_at(r, 0) == table)
        .map_or(0, |r| oid_at(r, 13));
    let indexes = |rel: u32| -> Vec<u32> {
        let mut v: Vec<u32> = rows(system::SysTable::PgIndex)
            .iter()
            .filter(|r| oid_at(r, 1) == rel)
            .map(|r| oid_at(r, 0))
            .collect();
        v.sort_unstable();
        v
    };
    let mut out = if toast != 0 { indexes(toast) } else { Vec::new() };
    out.extend(indexes(table));
    out
}

fn system_concurrently() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "cannot reindex system catalogs concurrently",
    )
}

/// What ALTER COLUMN TYPE re-creates for one altered table (an AlteredTableInfo's
/// changedConstraintOids and changedIndexOids).
pub(crate) struct TypeRebuild {
    table: u32,
    /// (owner table, constraint oid): constraints of the table, and other tables' foreign keys
    /// to it.
    cons: Vec<(u32, u32)>,
    indexes: Vec<u32>,
    /// The table's column types and collations before the first type change.
    old_cols: Vec<(DataType, Option<String>)>,
    /// (column, name) of an inheritor's not-null constraints of its own also inherited, which
    /// are added again by name.
    local_not_nulls: Vec<(usize, String)>,
}

/// The position of table `oid`'s working copy in `tables`, taken from the table in hand or the
/// catalog first if needed.
fn get_table(
    cat: &Catalog,
    t: &Option<&mut Table>,
    tables: &mut Vec<Table>,
    oid: u32,
) -> Option<usize> {
    if let Some(i) = tables.iter().position(|x| x.oid == oid) {
        return Some(i);
    }
    let tb = match t {
        Some(t) if t.oid == oid => (**t).clone(),
        _ => (**cat.tables.get(&oid)?).clone(),
    };
    tables.push(tb);
    Some(tables.len() - 1)
}

/// Whether index `ix` still fits its columns' new types, so re-creating it without a rewrite
/// keeps its file (TryReuseIndex, CheckIndexCompatible: the same operator classes and
/// collations; an expression's type isn't tracked, so it never does).
fn index_compatible(
    ix: &Index,
    old: &[(DataType, Option<String>)],
    t: &Table,
    excl: Option<&[String]>,
) -> bool {
    // A predicate isn't assessed, nor is an invalid index reused.
    if ix.predicate.is_some() || !ix.valid {
        return false;
    }
    let am = match ix.method.as_str() {
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => 403,
    };
    // The exclusion operators, as the new types resolve their names (a polymorphic one
    // needs the same type).
    if let Some(ops) = excl {
        for (c, op) in ix.columns.iter().zip(ops) {
            let IndexKey::Column(i) = c.key else {
                return false;
            };
            let (Some((oty, _)), Some(col)) = (old.get(i), t.columns.get(i)) else {
                return false;
            };
            if *oty == col.ty {
                continue;
            }
            let (a, b) = (
                crate::sql::nodetree::operator_oid(op, oty),
                crate::sql::nodetree::operator_oid(op, &col.ty),
            );
            if a.is_none() || a != b || a.is_some_and(|(_, poly)| poly) {
                return false;
            }
        }
    }
    ix.columns.iter().all(|c| {
        let IndexKey::Column(i) = c.key else {
            return false;
        };
        let (Some((oty, ocoll)), Some(col)) = (old.get(i), t.columns.get(i)) else {
            return false;
        };
        // An explicit COLLATE stays; else the column's.
        let same_coll = c.collation.is_some() || !col.ty.collatable() || *ocoll == col.collation;
        if *oty == col.ty {
            return same_coll;
        }
        let (a, b) = (
            crate::catalog::system::default_opclass(am, oty),
            crate::catalog::system::default_opclass(am, &col.ty),
        );
        // A polymorphic operator class's input type changes with the column's.
        let polymorphic = matches!(
            oty,
            DataType::Array(_) | DataType::Enum(..) | DataType::Range(_) | DataType::Multirange(_)
        );
        (c.opclass != 0 || (a == b && a != 0)) && !polymorphic && same_coll
    })
}

/// Whether an ALTER TABLE subcommand runs in a pass before AT_PASS_OLD_INDEX (dropping,
/// changing a column's type, adding a column, SET EXPRESSION).
fn before_old_index_pass(cmd: &pb::AlterTableCmd) -> bool {
    use pb::AlterTableType as A;
    matches!(
        A::try_from(cmd.subtype).unwrap_or(A::Undefined),
        A::AtDropColumn
            | A::AtDropConstraint
            | A::AtAlterColumnType
            | A::AtAddColumn
            | A::AtSetExpression
    )
}

/// Whether changing a column's type from `from` to `to` rewrites the table
/// (ATColumnChangeRequiresRewrite): not when the conversion is binary (pg_cast's method `b`)
/// to an unconstrained type, or only relaxes the type modifier (a longer varchar, a wider
/// numeric of the same scale, a finer time precision).
fn type_change_rewrites(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    let (fm, tm) = (from.typmod(), to.typmod());
    if from == to {
        return false;
    }
    if from.oid() == to.oid() && !from.is_array() {
        return match to {
            T::Varchar(_) | T::Varbit(_) => tm >= 0 && (fm < 0 || tm < fm),
            // (precision << 16 | scale) + 4
            T::Numeric(_) => {
                let (fm, tm) = (fm - 4, tm - 4);
                tm >= 0 && (fm < 0 || (tm & 0xffff) != (fm & 0xffff) || tm >> 16 < fm >> 16)
            }
            T::Time(_) | T::Timetz(_) | T::Timestamp(_) | T::Timestamptz(_) | T::Interval(_) => {
                tm >= 0 && (fm < 0 || tm < fm)
            }
            _ => true,
        };
    }
    !(tm < 0 && system::builtin_cast_method(from.oid(), to.oid()).is_some_and(|(m, _)| m == 'b'))
}

fn sole_column_ref<'t>(t: &'t Table, expr: &str) -> Option<&'t str> {
    // (A system column counts too: `tableoid` makes a second column.)
    const SYSTEM: [&str; 6] = ["tableoid", "ctid", "xmin", "cmin", "xmax", "cmax"];
    // The column references, inside field selections `(c).f` too (which the node walk
    // doesn't enter).
    fn refs(n: &NodeEnum, t: &Table, cols: &mut Vec<usize>) {
        for (node, _, _, _) in n.nodes() {
            match node {
                pg_query::NodeRef::ColumnRef(c) => {
                    if let Some(i) = c.fields.last().and_then(|f| ast::sval(f)).and_then(|name| {
                        t.column_index(name).or_else(|| {
                            SYSTEM.iter().position(|s| *s == name).map(|k| usize::MAX - k)
                        })
                    }) && !cols.contains(&i)
                    {
                        cols.push(i);
                    }
                }
                pg_query::NodeRef::AIndirection(ai) => {
                    if let Some(arg) = ai.arg.as_ref().and_then(|a| a.node.as_ref()) {
                        refs(arg, t, cols);
                    }
                }
                _ => {}
            }
        }
    }
    let n = ast::parse_expr(expr).ok()?;
    let mut cols = Vec::new();
    refs(n.node.as_ref()?, t, &mut cols);
    match cols[..] {
        [i] => Some(t.columns.get(i).map_or_else(|| SYSTEM[usize::MAX - i], |c| c.name.as_str())),
        _ => None,
    }
}

fn conflicting_null(col: &str, table: &str) -> Error {
    Error::new(
        code::SYNTAX_ERROR,
        format!("conflicting NULL/NOT NULL declarations for column \"{col}\" of table \"{table}\""),
    )
}

pub(super) fn type_max(ty: &DataType) -> i64 {
    match ty {
        DataType::Int2 => i16::MAX as i64,
        DataType::Int4 => i32::MAX as i64,
        _ => i64::MAX,
    }
}

/// The per-table counter behind hidden row ids (keyed by the table's oid).
pub(crate) fn rowid_sequence(t: &Table) -> Sequence {
    Sequence {
        oid: t.oid,
        schema: t.schema,
        name: t.name.clone(),
        owner: t.owner,
        ty: DataType::Int8,
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        // (Logged ahead in large steps: every insert takes a value; one skipped after a
        // crash is harmless.)
        cache: crate::txn::ROWID_CACHE,
        cycle: false,
        owned_by: None,
        unlogged: None,
    }
}

/// The key collations duplicates of an index's keys are found under: its entries' (a
/// clustered primary key's data keys are bytes, but its duplicates are equal keys too).
fn dup_colls(cat: &crate::catalog::Catalog, t: &Table, bix: &BoundIndex) -> Vec<Option<crate::storage::keys::KeyColl>> {
    if bix.index.id == 0 {
        crate::sql::binder::column_key_colls(cat, t, &bix.index)
    } else {
        bix.colls.clone()
    }
}

/// Order-preserving bytes for a tuple of values (for duplicate detection).
pub(super) fn key_bytes(vals: &[Value]) -> Vec<u8> {
    let mut k = Vec::new();
    for v in vals {
        crate::storage::keys::encode_value(&mut k, v, false);
    }
    k
}

/// The unique keys an index build has seen, each with the data key of the row that had it
/// first: a 64-bit hash of the key, and the data keys end to end (about 45 bytes a row,
/// where the keys and data keys themselves took about 170). Two keys with one hash are
/// told apart by the first row's key, computed again from the row.
#[derive(Default)]
pub(super) struct UniqueKeys {
    by_hash: std::collections::HashMap<u64, u32>,
    dkeys: Vec<u8>,
    ends: Vec<u64>,
    /// Keys whose hash an unequal key took first.
    others: std::collections::HashMap<Vec<u8>, Vec<u8>>,
}

impl UniqueKeys {
    /// Note `key` for the row stored under `dkey`; if an earlier row had it, that row's
    /// data key. `key_of` gives a stored row's key (None: it has none) from its data key.
    pub(super) fn insert(
        &mut self,
        key: Vec<u8>,
        dkey: &[u8],
        key_of: impl FnOnce(&[u8]) -> Result<Option<Vec<u8>>>,
    ) -> Result<Option<Vec<u8>>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        let h = h.finish();
        match self.by_hash.get(&h) {
            None => {
                self.by_hash.insert(h, self.ends.len() as u32);
                self.dkeys.extend_from_slice(dkey);
                self.ends.push(self.dkeys.len() as u64);
                Ok(None)
            }
            Some(&i) => {
                let i = i as usize;
                let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
                let first = &self.dkeys[start..self.ends[i] as usize];
                if key_of(first)?.as_deref() == Some(key.as_slice()) {
                    return Ok(Some(first.to_vec()));
                }
                if let Some(d) = self.others.get(&key) {
                    return Ok(Some(d.clone()));
                }
                self.others.insert(key, dkey.to_vec());
                Ok(None)
            }
        }
    }
}

pub(super) fn index_values(ix: &BoundIndex, row: &[Value], d: &Ddl) -> Result<Option<Vec<Value>>> {
    // Building an index runs its expressions as the table's owner (a security-restricted
    // operation).
    let exprs = ix.predicate.is_some() || ix.keys.iter().any(|k| !matches!(k, Expr::Col(..)));
    let owner = exprs
        .then(|| d.cat.index_table(ix.index.table).map(|t| t.owner))
        .flatten()
        .filter(|o| *o != d.ctx.current_user());
    if let Some(o) = owner {
        d.ctx.sess.definer.borrow_mut().push(o);
    }
    let level = exprs.then(|| d.ctx.sess.settings.borrow_mut().restrict_search_path());
    let r = (|| {
        if let Some(p) = &ix.predicate
            && eval(p, row, d.ctx)? != Value::Bool(true)
        {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(ix.keys.len());
        for k in &ix.keys {
            out.push(eval(k, row, d.ctx)?);
        }
        Ok(Some(out))
    })();
    if let Some(level) = level {
        d.ctx.sess.settings.borrow_mut().pop_restricted(level);
    }
    if owner.is_some() {
        d.ctx.sess.definer.borrow_mut().pop();
    }
    r
}

/// Whether SQL text mentions an identifier (a cheap dependency test).
pub(super) fn view_mentions(sql: &str, ident: &str) -> bool {
    let Ok(stmts) = crate::sql::parse::parse(&format!("SELECT {sql}"))
        .or_else(|_| crate::sql::parse::parse(sql))
    else {
        return sql.contains(ident);
    };
    for s in stmts {
        let Some(ne) = s.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            continue;
        };
        for (node, _, _, _) in ne.nodes() {
            if let pg_query::NodeRef::ColumnRef(c) = node
                && c.fields.iter().any(|f| ast::sval(f) == Some(ident))
            {
                return true;
            }
        }
    }
    false
}

/// Rename column references in a stored expression.
pub(super) fn rename_column_in(sql: &str, old: &str, new: &str) -> Result<String> {
    let q = rename_column_in_query(&format!("SELECT {sql}"), old, new)?;
    Ok(q.strip_prefix("SELECT ").unwrap_or(&q).to_string())
}

/// Rename column references in a stored query.
fn rename_column_in_query(sql: &str, old: &str, new: &str) -> Result<String> {
    let mut tree = pg_query::parse(sql, 0)
        .map_err(|e| crate::error::internal(format!("stored query: {e}")))?;
    rename_column_refs(&mut tree.protobuf, old, new);
    tree.deparse(Default::default())
        .map_err(|e| crate::error::internal(format!("deparse: {e}")))
}

/// Rename the column references `…old` in `tree`.
fn rename_column_refs(tree: &mut pb::ParseResult, old: &str, new: &str) {
    // SAFETY: the pointers come from `tree`, which outlives this loop and isn't otherwise
    // touched while they're used.
    unsafe {
        for (node, _, _) in tree.nodes_mut() {
            if let pg_query::NodeMut::ColumnRef(c) = node {
                let c = &mut *c;
                if let Some(last) = c.fields.last_mut()
                    && let Some(NodeEnum::String(s)) = last.node.as_mut()
                    && s.sval == old
                {
                    s.sval = new.to_string();
                }
            }
        }
    }
}

/// User columns can't take a system column's name.
fn system_name(name: &str) -> Result<()> {
    if name == "tableoid" || crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == name) {
        return Err(Error::new(
            code::DUPLICATE_COLUMN,
            format!("column name \"{name}\" conflicts with a system column name"),
        ));
    }
    Ok(())
}

/// Operators that are their own commutator (exclusion constraints accept only these).
fn exclusion_commutative(op: &str) -> bool {
    matches!(
        op,
        "=" | "<>" | "&&" | "~=" | "-|-" | "?#" | "?||" | "?-|" | "#" | "+" | "*" | "&" | "|"
    )
}

/// Whether `op` belongs to the operator family of opclass `opclass` of access method `method`.
/// The primary key's columns: the one keying the rows, or one WITHOUT OVERLAPS.
/// The columns of the index REPLICA IDENTITY USING INDEX chose (none: no such index).
pub(super) fn replident_columns(cat: &Catalog, t: &Table) -> Vec<usize> {
    if t.replident != Some('i') {
        return Vec::new();
    }
    cat.indexes
        .values()
        .filter(|i| i.table == t.oid && i.replident)
        .flat_map(|i| i.columns.iter())
        .filter_map(|c| match c.key {
            IndexKey::Column(i) => Some(i),
            _ => None,
        })
        .collect()
}

/// dropconstraint_internal: a not-null constraint can't go from a primary key's column,
/// or one in the replica identity index.
fn not_null_droppable(cat: &Catalog, t: &Table, column: usize) -> Result<()> {
    if pk_columns(cat, t).is_some_and(|p| p.contains(&column)) {
        return Err(Error::new(
            code::INVALID_TABLE_DEFINITION,
            format!("column \"{}\" is in a primary key", t.columns[column].name),
        ));
    }
    if replident_columns(cat, t).contains(&column) {
        return Err(Error::new(
            code::INVALID_TABLE_DEFINITION,
            format!("column \"{}\" is in index used as replica identity", t.columns[column].name),
        ));
    }
    Ok(())
}

pub(super) fn pk_columns(cat: &Catalog, t: &Table) -> Option<Vec<usize>> {
    if let Some(p) = &t.pkey {
        return Some(p.clone());
    }
    let con = t.constraints.iter().find(|c| c.period == Some('p'))?;
    let ix = cat.indexes.get(&con.kind.index()?)?;
    Some(
        ix.columns
            .iter()
            .filter_map(|c| match c.key {
                IndexKey::Column(i) => Some(i),
                _ => None,
            })
            .collect(),
    )
}

fn exclusion_op_in_family(method: &str, opclass: &str, op: &str) -> bool {
    match (method, opclass) {
        ("btree" | "hash", _) => op == "=",
        (_, "range_ops" | "multirange_ops") => matches!(
            op,
            "&&" | "=" | "@>" | "<@" | "<<" | ">>" | "&<" | "&>" | "-|-"
        ),
        (_, "box_ops" | "poly_ops" | "circle_ops") => {
            matches!(
                op,
                "<<" | "&<"
                    | "&&"
                    | "&>"
                    | ">>"
                    | "~="
                    | "@>"
                    | "<@"
                    | "&<|"
                    | "<<|"
                    | "|>>"
                    | "|&>"
                    | "~"
                    | "@"
            )
        }
        (_, "point_ops" | "quad_point_ops" | "kd_point_ops") => {
            matches!(op, "<<" | ">>" | "~=" | "<<|" | "|>>" | "<^" | ">^" | "<@")
        }
        (_, "inet_ops") => matches!(
            op,
            "&&" | ">>" | ">>=" | "<<" | "<<=" | "=" | "<>" | "<" | "<=" | ">" | ">="
        ),
        (_, "tsvector_ops") => op == "@@",
        (_, "tsquery_ops") => matches!(op, "@>" | "<@"),
        (_, "text_ops") => matches!(
            op,
            "=" | "<" | "<=" | ">" | ">=" | "~<~" | "~<=~" | "~>=~" | "~>~" | "^@"
        ),
        _ => false,
    }
}

/// A partition's columns take their partitioned table's identity (and its sequence).
pub(super) fn share_identity(parent: &Table, part: &mut Table) {
    for c in part.columns.iter_mut().filter(|c| !c.dropped) {
        if let Some(pc) = parent.column_index(&c.name).map(|j| &parent.columns[j]) {
            c.identity = pc.identity.clone();
            c.seq = if pc.identity.is_some() { pc.seq } else { None };
        }
    }
}

impl Ddl<'_, '_> {
    /// After an identity change on a partitioned table: its partitions follow.
    fn identity_to_partitions(&mut self, t: &Table) {
        if t.partkey.is_none() {
            return;
        }
        for p in self.cat.partitions(t.oid) {
            let Some(part) = self.cat.tables.get(&p).cloned() else {
                continue;
            };
            let mut part = (*part).clone();
            share_identity(t, &mut part);
            self.cat.put_table(part.clone());
            self.identity_to_partitions(&part);
        }
    }
}

/// A table's storage parameters (heap_reloptions): `opts` set over `base` (ALTER TABLE
/// SET), or removed from it (`reset`). Values are kept as written.
pub(super) fn heap_options(opts: &[pb::Node], base: &[String], reset: bool) -> Result<Vec<String>> {
    use crate::session::ddl::def_elems;
    enum Kind {
        Int(i64, i64),
        Real(f64, f64),
        Bool,
        Cleanup,
    }
    const AV: &[(&str, Kind)] = &[
        ("autovacuum_enabled", Kind::Bool),
        ("autovacuum_vacuum_threshold", Kind::Int(0, i32::MAX as i64)),
        (
            "autovacuum_vacuum_max_threshold",
            Kind::Int(-1, i32::MAX as i64),
        ),
        (
            "autovacuum_vacuum_insert_threshold",
            Kind::Int(-1, i32::MAX as i64),
        ),
        (
            "autovacuum_analyze_threshold",
            Kind::Int(0, i32::MAX as i64),
        ),
        ("autovacuum_vacuum_cost_limit", Kind::Int(1, 10000)),
        ("autovacuum_freeze_min_age", Kind::Int(0, 1_000_000_000)),
        (
            "autovacuum_freeze_max_age",
            Kind::Int(100_000, 2_000_000_000),
        ),
        ("autovacuum_freeze_table_age", Kind::Int(0, 2_000_000_000)),
        (
            "autovacuum_multixact_freeze_min_age",
            Kind::Int(0, 1_000_000_000),
        ),
        (
            "autovacuum_multixact_freeze_max_age",
            Kind::Int(10_000, 2_000_000_000),
        ),
        (
            "autovacuum_multixact_freeze_table_age",
            Kind::Int(0, 2_000_000_000),
        ),
        (
            "log_autovacuum_min_duration",
            Kind::Int(-1, i32::MAX as i64),
        ),
        ("autovacuum_vacuum_cost_delay", Kind::Real(0.0, 100.0)),
        ("autovacuum_vacuum_scale_factor", Kind::Real(0.0, 100.0)),
        (
            "autovacuum_vacuum_insert_scale_factor",
            Kind::Real(0.0, 100.0),
        ),
        ("autovacuum_analyze_scale_factor", Kind::Real(0.0, 100.0)),
        ("vacuum_index_cleanup", Kind::Cleanup),
        ("vacuum_truncate", Kind::Bool),
        ("vacuum_max_eager_freeze_failure_rate", Kind::Real(0.0, 1.0)),
    ];
    const HEAP: &[(&str, Kind)] = &[
        ("fillfactor", Kind::Int(10, 100)),
        ("toast_tuple_target", Kind::Int(128, 8160)),
        ("parallel_workers", Kind::Int(0, 1024)),
        ("user_catalog_table", Kind::Bool),
        // roll's own, not Postgres's: `roll_pk_clustered = true` stores the rows keyed by
        // a non-deferrable primary key (no index entries of its own: faster writes and key
        // lookups) instead of under a hidden row id in heap order, so scans come out in key
        // order. Partitioned tables take it too (only it), and pass it to new partitions.
        // A table without a primary key keeps it until one is added (as pg_dump adds
        // keys); ALTER TABLE SET / RESET lays the rows out again; dropping the key ends
        // it. See `pk_clustered_change` and tests/regress/STATUS.md (By design).
        (crate::catalog::PK_CLUSTERED, Kind::Bool),
    ];
    let bad = |msg: String| Error::new(code::INVALID_PARAMETER_VALUE, msg);
    let mut out: Vec<String> = base.to_vec();
    let mut seen: Vec<(String, String)> = Vec::new();
    for d in def_elems(opts) {
        let key = if d.defnamespace.is_empty() {
            d.defname.clone()
        } else {
            format!("{}.{}", d.defnamespace, d.defname)
        };
        if reset {
            if d.arg.is_some() {
                return Err(bad("RESET must not include values for parameters".to_string()));
            }
            out.retain(|o| o.split_once('=').map_or(o.as_str(), |(k, _)| k) != key);
            continue;
        }
        if !d.defnamespace.is_empty() && d.defnamespace != "toast" {
            return Err(bad(format!(
                "unrecognized parameter namespace \"{}\"",
                d.defnamespace
            )));
        }
        if d.defnamespace.is_empty() && d.defname == "oids" {
            let v = ast::def_arg_str(d).unwrap_or_else(|| "true".into());
            if crate::types::text::parse_bool(&v).unwrap_or(false) {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "tables declared WITH OIDS are not supported",
                ));
            }
            continue;
        }
        let kind = AV
            .iter()
            .chain(if d.defnamespace.is_empty() { HEAP } else { &[] })
            .find(|(n, _)| *n == d.defname)
            .map(|(_, k)| k);
        let Some(kind) = kind else {
            return Err(bad(format!("unrecognized parameter \"{}\"", d.defname)));
        };
        if seen.iter().any(|(k, _)| *k == key) {
            return Err(bad(format!(
                "parameter \"{}\" specified more than once",
                d.defname
            )));
        }
        let v = ast::def_arg_str(d).unwrap_or_else(|| "true".into());
        match kind {
            Kind::Int(lo, hi) => {
                // (parse_int: a fractional value rounds.)
                let n: i64 = v
                    .trim()
                    .parse()
                    .ok()
                    .or_else(|| {
                        let f: f64 = v.trim().parse().ok()?;
                        (f.is_finite() && f.abs() < 1e18).then(|| f.round_ties_even() as i64)
                    })
                    .ok_or_else(|| {
                        bad(format!(
                            "invalid value for integer option \"{}\": {v}",
                            d.defname
                        ))
                    })?;
                if n < *lo || n > *hi {
                    return Err(bad(format!(
                        "value {v} out of bounds for option \"{}\"",
                        d.defname
                    ))
                    .detail(format!("Valid values are between \"{lo}\" and \"{hi}\".")));
                }
            }
            Kind::Real(lo, hi) => {
                let n: f64 = v.trim().parse().map_err(|_| {
                    bad(format!(
                        "invalid value for floating point option \"{}\": {v}",
                        d.defname
                    ))
                })?;
                if n < *lo || n > *hi {
                    return Err(bad(format!(
                        "value {v} out of bounds for option \"{}\"",
                        d.defname
                    ))
                    .detail(format!(
                        "Valid values are between \"{lo:.6}\" and \"{hi:.6}\"."
                    )));
                }
            }
            Kind::Bool => {
                if crate::types::text::parse_bool(&v).is_err() {
                    return Err(bad(format!(
                        "invalid value for boolean option \"{}\": {v}",
                        d.defname
                    )));
                }
            }
            Kind::Cleanup => {
                if v != "auto" && crate::types::text::parse_bool(&v).is_err() {
                    return Err(bad(format!(
                        "invalid value for enum option \"{}\": {v}",
                        d.defname
                    ))
                    .detail("Valid values are \"on\", \"off\", and \"auto\"."));
                }
            }
        }
        seen.push((key, v));
    }
    // (Toast parameters, kept as `toast.name`, are the TOAST table's.)
    for (k, v) in seen {
        out.retain(|o| o.split_once('=').map_or(o.as_str(), |(n, _)| n) != k);
        out.push(format!("{k}={v}"));
    }
    Ok(out)
}

/// Storage parameters a partitioned table can't have: all of Postgres's (roll's
/// `roll_pk_clustered` it keeps for its partitions).
fn partitioned_refuses(opts: &[String]) -> bool {
    opts.iter()
        .any(|o| o.split_once('=').map_or(o.as_str(), |(k, _)| k) != crate::catalog::PK_CLUSTERED)
}

/// Whether reloptions name `roll_pk_clustered` at all (on or off).
fn names_pk_clustered(opts: &[String]) -> bool {
    opts.iter()
        .any(|o| o.split_once('=').map_or(o.as_str(), |(k, _)| k) == crate::catalog::PK_CLUSTERED)
}

/// `roll_pk_clustered` on a table whose primary key can't key its rows.
fn pk_clustered_unusable(t: &Table, why: &str) -> Error {
    Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!(
            "storage parameter \"{}\" requires a non-deferrable primary key",
            crate::catalog::PK_CLUSTERED
        ),
    )
    .detail(format!("Relation \"{}\" {why}.", t.name))
}

/// Whether a new primary key of `t` keys its rows (index id 0) rather than having entries
/// of its own: under `roll_pk_clustered` (a deferrable key can't), or in the legacy layout.
pub(super) fn pk_keys_rows(t: &Table, deferrable: bool) -> Result<bool> {
    if crate::storage::keys::primary_key_keys_rows() {
        return Ok(true);
    }
    if !t.pk_clustered() {
        return Ok(false);
    }
    if deferrable {
        return Err(pk_clustered_unusable(t, "would have a deferrable primary key"));
    }
    Ok(true)
}

fn partitioned_options() -> Error {
    Error::new(
        code::WRONG_OBJECT_TYPE,
        "cannot specify storage parameters for a partitioned table",
    )
    .hint("Specify storage parameters for its leaf partitions instead.")
}

/// STORAGE plain | external | extended | main | default (None: the type's).
pub(super) fn storage_kind(name: &str, ty: &DataType) -> Result<Option<char>> {
    let name = name.to_ascii_lowercase();
    let storage = match name.as_str() {
        "plain" => Some('p'),
        "external" => Some('e'),
        "extended" => Some('x'),
        "main" => Some('m'),
        "default" => None,
        _ => {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid storage type \"{name}\""),
            ));
        }
    };
    if ty.typlen() != -1 && storage.is_some_and(|s| s != 'p') {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!(
                "column data type {} can only have storage PLAIN",
                ty.sql_name()
            ),
        ));
    }
    Ok(storage)
}

pub(super) fn storage_name(c: char) -> &'static str {
    match c {
        'p' => "PLAIN",
        'e' => "EXTERNAL",
        'm' => "MAIN",
        _ => "EXTENDED",
    }
}

pub(super) fn compression_name(c: Option<char>) -> &'static str {
    match c {
        Some('p') => "pglz",
        Some('l') => "lz4",
        _ => "",
    }
}

/// COMPRESSION pglz | lz4 | default.
pub(super) fn compression_method(name: &str) -> Result<Option<char>> {
    match name {
        "pglz" => Ok(Some('p')),
        "lz4" => Ok(Some('l')),
        "default" => Ok(None),
        _ => Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("invalid compression method \"{name}\""),
        )),
    }
}

/// Column options (ALTER COLUMN SET / RESET (n_distinct = …)) over `base`, as `name=value`.
fn attribute_options(opts: &[pb::Node], mut base: Vec<String>, reset: bool) -> Result<Vec<String>> {
    for d in super::def_elems(opts) {
        if reset {
            if d.arg.is_some() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "RESET must not include values for parameters",
                ));
            }
            base.retain(|o| o.split_once('=').map_or(o.as_str(), |(k, _)| k) != d.defname);
            continue;
        }
        if !matches!(d.defname.as_str(), "n_distinct" | "n_distinct_inherited")
            || !d.defnamespace.is_empty()
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unrecognized parameter \"{}\"", d.defname),
            ));
        }
        let v = ast::def_arg_str(d).unwrap_or_default();
        let n: f64 = v.trim().parse().map_err(|_| {
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "invalid value for floating point option \"{}\": {v}",
                    d.defname
                ),
            )
        })?;
        if n < -1.0 {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("value {v} out of bounds for option \"{}\"", d.defname),
            )
            .detail(format!(
                "Valid values are between \"-1.000000\" and \"{DBL_MAX}\"."
            )));
        }
        base.retain(|o| o.split_once('=').map_or(o.as_str(), |(k, _)| k) != d.defname);
        base.push(format!("{}={v}", d.defname));
    }
    Ok(base)
}

/// DBL_MAX as printf's %f writes it.
const DBL_MAX: &str = "179769313486231570814527423731704356798070567525844996598917476803157260780028538760589558632766878171540458953514382464234321326889464182768467546703537516986049910576551282076245490090389328944075868508455133942304583236903222948165808559332123348274797826204144723168738177180919299881250404026184124858368.000000";

impl Ddl<'_, '_> {
    /// SET STATISTICS n | DEFAULT: None for the default (-1).
    pub(super) fn stat_target(&self, def: Option<&pb::Node>) -> Result<Option<i16>> {
        let Some(n) = def.and_then(ast::ival) else {
            return Ok(None);
        };
        if n < -1 {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("statistics target {n} is too low"),
            ));
        }
        if n > 10000 {
            self.ctx.notice(crate::session::warning(
                code::INVALID_PARAMETER_VALUE,
                "lowering statistics target to 10000",
            ));
            return Ok(Some(10000));
        }
        Ok(if n == -1 { None } else { Some(n as i16) })
    }

    /// REPLICA IDENTITY USING INDEX: a unique, immediate, plain, complete index over NOT NULL
    /// columns of `t`.
    fn replica_index(&self, t: &Table, name: &str) -> Result<u32> {
        let ix = match self.cat.relation_in(t.schema, name) {
            Some(Relation::Index(i)) if i.table == t.oid => i,
            Some(Relation::Index(_)) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{name}\" is not an index for table \"{}\"", t.name),
                ));
            }
            _ => {
                return Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("index \"{name}\" for table \"{}\" does not exist", t.name),
                ));
            }
        };
        let bad = |c: &'static str, m: String| Err(Error::new(c, m));
        // (A WITHOUT OVERLAPS key's index is unique for the catalogs.)
        let temporal = t
            .constraints
            .iter()
            .any(|c| c.kind.index() == Some(ix.oid) && c.without_overlaps());
        if !temporal && (!ix.unique || ix.method != "btree") {
            return bad(
                code::WRONG_OBJECT_TYPE,
                format!("cannot use non-unique index \"{name}\" as replica identity"),
            );
        }
        if t.constraints
            .iter()
            .any(|c| c.kind.index() == Some(ix.oid) && c.deferral().0)
        {
            return bad(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot use non-immediate index \"{name}\" as replica identity"),
            );
        }
        if ix
            .columns
            .iter()
            .any(|c| matches!(c.key, IndexKey::Expr(_)))
        {
            return bad(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot use expression index \"{name}\" as replica identity"),
            );
        }
        if ix.predicate.is_some() {
            return bad(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot use partial index \"{name}\" as replica identity"),
            );
        }
        for c in &ix.columns {
            if let IndexKey::Column(i) = c.key
                && !t.columns[i].not_null
            {
                return bad(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "index \"{name}\" cannot be used as replica identity because column \"{}\" is nullable",
                        t.columns[i].name
                    ),
                );
            }
        }
        // (An invalid one will do: ALTER TABLE ONLY on a partitioned table makes those.)
        Ok(ix.oid)
    }

    /// ALTER CONSTRAINT name [NOT] DEFERRABLE [INITIALLY …] of a foreign key, with its
    /// clones on partitions and the constraints derived from it.
    /// ALTER CONSTRAINT: a foreign key's deferrability or enforceability, or whether a
    /// not-null constraint is inherited.
    fn alter_constraint(&mut self, t: &mut Table, ac: &pb::AtAlterConstraint) -> Result<()> {
        let name = ac.conname.as_str();
        let Some(c) = t.constraints.iter().find(|c| c.name == name).cloned() else {
            return Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "constraint \"{name}\" of relation \"{}\" does not exist",
                    t.name
                ),
            ));
        };
        let fk = matches!(c.kind, ConstraintKind::ForeignKey { .. });
        if ac.alter_deferrability && !fk {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "constraint \"{name}\" of relation \"{}\" is not a foreign key constraint",
                    t.name
                ),
            ));
        }
        if ac.alter_enforceability && !fk {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!(
                    "cannot alter enforceability of constraint \"{name}\" of relation \"{}\"",
                    t.name
                ),
            ));
        }
        let ConstraintKind::NotNull { column } = c.kind else {
            if ac.alter_inheritability {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "constraint \"{name}\" of relation \"{}\" is not a not-null constraint",
                        t.name
                    ),
                ));
            }
            return self.alter_fk(t, &c, ac);
        };
        if ac.alter_inheritability && ac.noinherit && c.inhcount > 0 {
            return Err(Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "cannot alter inherited constraint \"{name}\" on relation \"{}\"",
                    t.name
                ),
            ));
        }
        if ac.noinherit && t.partkey.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "not-null constraint \"{name}\" on partitioned table \"{}\" cannot be NO INHERIT",
                    t.name
                ),
            ));
        }
        if c.no_inherit == ac.noinherit {
            return Ok(());
        }
        for x in t.constraints.iter_mut().filter(|x| x.oid == c.oid) {
            x.no_inherit = ac.noinherit;
        }
        // The children's copies stop (or start again) being inherited from here.
        let colname = t.columns[column].name.clone();
        if !ac.noinherit {
            return self.not_null_children(t.oid, &colname, &c.name);
        }
        for child in self.cat.children(t.oid).to_vec() {
            let Some(ch) = self.cat.tables.get(&child).cloned() else {
                continue;
            };
            let Some(ci) = ch.column_index(&colname) else {
                continue;
            };
            let mut ch = (*ch).clone();
            if let Some(x) = ch
                .constraints
                .iter_mut()
                .find(|x| matches!(x.kind, ConstraintKind::NotNull { column } if column == ci))
            {
                // The direct children keep it as their own.
                x.inhcount = x.inhcount.saturating_sub(1);
                x.islocal = true;
            }
            self.cat.put_table(ch);
        }
        Ok(())
    }

    /// ALTER CONSTRAINT of a foreign key, applied to the copies in partitions too.
    fn alter_fk(
        &mut self,
        t: &mut Table,
        c: &Constraint,
        ac: &pb::AtAlterConstraint,
    ) -> Result<()> {
        let name = c.name.as_str();
        if c.parent != 0 {
            // (The topmost constraint it derives from.)
            let find = |oid: u32| {
                self.cat.tables.values().find_map(|x| {
                    x.constraints
                        .iter()
                        .find(|p| p.oid == oid)
                        .map(|p| (p.parent, p.name.clone(), x.name.clone()))
                })
            };
            let (mut pname, mut ptable) = (String::new(), String::new());
            let mut cur = c.parent;
            while let Some((up, n, tn)) = find(cur) {
                (pname, ptable) = (n, tn);
                if up == 0 {
                    break;
                }
                cur = up;
            }
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot alter constraint \"{name}\" on relation \"{}\"", t.name))
                .detail(format!("Constraint \"{name}\" is derived from constraint \"{pname}\" of relation \"{ptable}\"."))
                .hint("You may alter the constraint it derives from instead."));
        }
        // Enforcing it again checks the rows.
        if ac.alter_enforceability
            && ac.is_enforced
            && !c.enforced
            && let ConstraintKind::ForeignKey {
                columns,
                ref_table,
                ref_columns,
                match_full,
                ..
            } = &c.kind
        {
            let parent = self
                .cat
                .tables
                .get(ref_table)
                .cloned()
                .ok_or_else(|| crate::error::internal("parent"))?;
            self.validate_fk(t, &parent, columns, ref_columns, name, *match_full, c.is_period())?;
        }
        let set = |c: &mut Constraint| {
            if ac.alter_deferrability
                && let ConstraintKind::ForeignKey {
                    deferrable: d,
                    initially_deferred: i,
                    ..
                } = &mut c.kind
            {
                (*d, *i) = (ac.deferrable, ac.initdeferred);
            }
            if ac.alter_enforceability {
                c.enforced = ac.is_enforced;
                c.validated = ac.is_enforced;
            }
        };
        let root = c.oid;
        for c in t.constraints.iter_mut().filter(|c| c.oid == root) {
            set(c);
        }
        // Everything descending from it, wherever it lives.
        let mut family = vec![root];
        let mut i = 0;
        while i < family.len() {
            let p = family[i];
            for x in self.cat.tables.values() {
                for c in &x.constraints {
                    if c.parent == p && !family.contains(&c.oid) {
                        family.push(c.oid);
                    }
                }
            }
            i += 1;
        }
        let owners: Vec<u32> = self
            .cat
            .tables
            .values()
            .filter(|x| {
                x.oid != t.oid && x.constraints.iter().any(|c| family[1..].contains(&c.oid))
            })
            .map(|x| x.oid)
            .collect();
        for o in owners {
            let mut x = (*self.cat.tables[&o]).clone();
            for c in x
                .constraints
                .iter_mut()
                .filter(|c| family[1..].contains(&c.oid))
            {
                set(c);
            }
            self.cat.put_table(x);
        }
        for c in t
            .constraints
            .iter_mut()
            .filter(|c| family[1..].contains(&c.oid))
        {
            set(c);
        }
        Ok(())
    }
}

/// Whether an expression computes or produces a value of a user-defined type.
fn uses_user_type(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if !matches!(
            x,
            Expr::Agg(..) | Expr::Window(..) | Expr::Grouping(_) | Expr::Srf(..)
        ) && x.ty().oid() >= crate::catalog::FIRST_USER_OID
        {
            found = true;
        }
    });
    found
}

/// Whether a foreign key's referencing column type `a` works with its referenced key's type
/// `b` (ATAddForeignKeyConstraint: an equality operator of the key's opfamily, or an
/// implicit cast to the key's type).
fn fk_types_compatible(a: &DataType, b: &DataType) -> bool {
    a.same_base(b)
        || matches!(
            (a, b),
            (
                DataType::Int2 | DataType::Int4 | DataType::Int8,
                DataType::Int2 | DataType::Int4 | DataType::Int8
            )
        )
        || matches!(
            (a, b),
            (
                DataType::Text
                    | DataType::Varchar(_)
                    | DataType::Name
                    | DataType::Bpchar(_),
                DataType::Text
                    | DataType::Varchar(_)
                    | DataType::Name
                    | DataType::Bpchar(_)
            )
        )
        // (Cross-type equality in the opfamily: float_ops, datetime_ops.)
        || matches!(
            (a, b),
            (DataType::Float4 | DataType::Float8, DataType::Float4 | DataType::Float8)
                | (
                    DataType::Date | DataType::Timestamp(_) | DataType::Timestamptz(_),
                    DataType::Date | DataType::Timestamp(_) | DataType::Timestamptz(_)
                )
        )
        // Else an implicit cast from the referencing type to the key's type does
        // (with the key's own equality).
        || crate::sql::func::can_coerce(a, b)
}
