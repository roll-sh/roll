//! INSERT, UPDATE and DELETE (`ModificationStatement`, `UpdateStatement`, `DeleteStatement`,
//! `Operation`, `Lists` / `Sets` / `Maps` / `UserTypes` operations, `Attributes`, `Json`;
//! Apache-2.0).
//!
//! A modification becomes partition updates in the storage model: INSERT writes the row marker
//! (UPDATE doesn't), a null is a tombstone, UNSET leaves the column untouched, setting a non-frozen
//! collection or UDT deletes the previous cells (complex deletion at `timestamp - 1`) before
//! writing the new ones. List operations by index, list element removal and counters read the
//! current row under the partition lock first. A conditional statement's `IF` clause is typed by
//! `crate::lwt::prepare` and the statement runs through the LWT hook (`crate::lwt`).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use futures::StreamExt;
use nosql_cql::ast::{
    self, Assignment, Deletion, InsertValues, Operation, Statement, StatementKind,
};
use nosql_storage::manifest::TableId;
use nosql_storage::read::{ClusteringFilter, PartitionRead};
use nosql_storage::{
    Bound as SBound, Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, PartitionKey,
    PartitionUpdate, RangeTombstone, Row, RowDeletion,
};
use nosql_types::{CqlType, Dialect};

use crate::database::Database;
use crate::error::{Error, Result};
use crate::messages as msg;
use crate::prepare::restrictions::{self, InValues, Partitions, Restrictions, StmtKind};
use crate::prepare::terms::{self, Bound, EvalCtx, Term};
use crate::prepare::{PrepareCtx, Receiver};
use crate::response::{ColumnSpec, QueryParams, Response};
use crate::schema::{ColumnKind, Schema, Table};
use crate::storage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKind {
    Insert,
    Update,
    Delete,
}

/// An operation on a column.
#[derive(Clone, Debug)]
pub enum Op {
    /// `c = v` (also INSERT values).
    Set { col: usize, value: Term },
    /// `l[i] = v` / `m[k] = v`.
    SetElement { col: usize, key: Term, value: Term },
    /// `u.f = v` (non-frozen UDT).
    SetField {
        col: usize,
        field: usize,
        value: Term,
    },
    /// `c = c + v`: list append, set add, map put.
    Add { col: usize, value: Term },
    /// `l = v + l`.
    Prepend { col: usize, value: Term },
    /// `c = c - v`: list element removal, set removal, map key removal.
    Remove { col: usize, value: Term },
    /// Counter increment (`negate`: decrement).
    Counter {
        col: usize,
        value: Term,
        negate: bool,
    },
    /// `c = c + v` / `c = c - v` on a non-counter column of a conditional statement
    /// (`crate::lwt::arith`).
    Arith { col: usize, value: Term, sub: bool },
}

impl Op {
    pub fn column(&self) -> usize {
        match self {
            Op::Set { col, .. }
            | Op::SetElement { col, .. }
            | Op::SetField { col, .. }
            | Op::Add { col, .. }
            | Op::Prepend { col, .. }
            | Op::Remove { col, .. }
            | Op::Counter { col, .. }
            | Op::Arith { col, .. } => *col,
        }
    }

    pub fn needs_read(&self, t: &Table) -> bool {
        match self {
            Op::SetElement { col, .. } => matches!(t.columns[*col].ty, CqlType::List { .. }),
            Op::Remove { col, .. } => matches!(t.columns[*col].ty, CqlType::List { .. }),
            Op::Counter { .. } | Op::Arith { .. } => true,
            _ => false,
        }
    }
}

/// A DELETE target.
#[derive(Clone, Debug)]
pub enum Del {
    Column(usize),
    Element(usize, Term),
    Field(usize, usize),
}

impl Del {
    pub fn column(&self) -> usize {
        match self {
            Del::Column(c) | Del::Element(c, _) | Del::Field(c, _) => *c,
        }
    }
}

/// `INSERT … JSON`.
#[derive(Clone, Debug)]
pub struct JsonInsert {
    pub value: Term,
    pub default_unset: bool,
}

/// A prepared modification.
pub struct ModifyPlan {
    pub table: Arc<Table>,
    pub kind: ModKind,
    pub restrictions: Option<Restrictions>,
    /// INSERT: partition key and clustering values by position.
    pub insert_pk: Vec<Option<Term>>,
    pub insert_ck: Vec<Option<Term>>,
    pub ops: Vec<Op>,
    pub deletions: Vec<Del>,
    pub json: Option<JsonInsert>,
    pub timestamp: Option<Term>,
    pub ttl: Option<Term>,
    pub timeout: Option<Term>,
    pub bind: Vec<ColumnSpec>,
    /// Partition key bind variables (PREPARED metadata).
    pub pk_indexes: Vec<u16>,
    pub conditional: bool,
    /// The `IF` clause, typed (executed by the LWT hook).
    pub conditions: Conditions,
    /// Only static columns are written (no clustering needed).
    pub static_only: bool,
    pub needs_read: bool,
    /// The user functions the statement calls (`EXECUTE` checks, `crate::udf`).
    pub functions: Vec<Arc<dyn crate::functions::Function>>,
}

/// Timestamp, TTL and "now" of one execution.
#[derive(Clone, Copy, Debug)]
pub struct Attrs {
    pub timestamp: i64,
    /// `None`: no TTL; `Some(ttl)` with ttl > 0.
    pub ttl: Option<i32>,
    pub now: i64,
}

impl Attrs {
    fn cell(&self, v: Bytes) -> Cell {
        match self.ttl {
            Some(ttl) => Cell::expiring(self.timestamp, ttl, self.now + ttl as i64, v),
            None => Cell::live(self.timestamp, v),
        }
    }

    fn tombstone(&self) -> Cell {
        Cell::tombstone(self.timestamp, self.now)
    }

    fn liveness(&self) -> LivenessInfo {
        match self.ttl {
            Some(ttl) => LivenessInfo::expiring(self.timestamp, ttl, self.now + ttl as i64),
            None => LivenessInfo::live(self.timestamp),
        }
    }

    fn complex_deletion(&self) -> DeletionTime {
        DeletionTime::new(self.timestamp - 1, self.now)
    }

    fn deletion(&self) -> DeletionTime {
        DeletionTime::new(self.timestamp, self.now)
    }
}

fn written(a: &Assignment) -> String {
    a.to_string()
}

fn column_of(t: &Table, ident: &ast::Ident, d: Dialect) -> Result<usize> {
    let name = ident.name();
    t.column_index(&name)
        .ok_or_else(|| msg::unknown_identifier(d, &t.keyspace, &t.name, &name))
}

/// Prepares INSERT / UPDATE / DELETE.
pub fn prepare(
    db: &Arc<Database>,
    schema: &Arc<Schema>,
    kind: &StatementKind<'static>,
    stmt: &Statement<'static>,
    keyspace: Option<&str>,
) -> Result<ModifyPlan> {
    let mut cx = PrepareCtx::new(
        db.dialect(),
        schema,
        keyspace.unwrap_or(""),
        &db.functions,
        &stmt.bind_markers,
    );
    cx.apply_settings(&db.settings());
    let uf = db.user_functions.read().clone();
    cx.user_functions = uf.as_deref();
    let mut plan = prepare_in(db, schema, kind, keyspace, &mut cx)?;
    plan.bind = cx.bind_specs()?;
    plan.pk_indexes = cx.pk_indexes(&plan.table);
    Ok(plan)
}

/// Prepares one modification with a context shared by a whole batch (bind markers are numbered
/// across the batch).
pub fn prepare_in(
    db: &Arc<Database>,
    schema: &Arc<Schema>,
    kind: &StatementKind<'static>,
    keyspace: Option<&str>,
    cx: &mut PrepareCtx,
) -> Result<ModifyPlan> {
    let d = db.dialect();
    let (name, using) = match kind {
        StatementKind::Insert(i) => (&i.table, &i.using),
        StatementKind::Update(u) => (&u.table, &u.using),
        StatementKind::Delete(x) => (&x.table, &x.using),
        _ => return Err(Error::server("not a modification")),
    };
    let table = super::resolve_table(db, schema, name, keyspace)?;
    let t = &table;
    if t.is_view() {
        return Err(Error::invalid("Cannot directly modify a materialized view"));
    }
    if (t.is_virtual() && !crate::guardrails::config_table::accepts_writes(db, t))
        || t.kind == crate::schema::TableKind::SchemaTable
    {
        return Err(Error::unauthorized(format!(
            "{} keyspace is not user-modifiable.",
            t.keyspace
        )));
    }
    cx.keyspace = t.keyspace.clone();
    let cx = &mut *cx;
    let mut plan = ModifyPlan {
        table: table.clone(),
        kind: ModKind::Insert,
        restrictions: None,
        insert_pk: vec![None; t.partition_key.len()],
        insert_ck: vec![None; t.clustering.len()],
        ops: Vec::new(),
        deletions: Vec::new(),
        json: None,
        timestamp: None,
        ttl: None,
        timeout: None,
        bind: Vec::new(),
        pk_indexes: Vec::new(),
        conditional: false,
        conditions: Conditions::None,
        static_only: false,
        needs_read: false,
        functions: Vec::new(),
    };
    // USING first: bind markers are numbered in source order, attributes come first in UPDATE.
    let prepare_using = |plan: &mut ModifyPlan, cx: &mut PrepareCtx| -> Result<()> {
        if let Some(ts) = &using.timestamp {
            plan.timestamp = Some(terms::prepare(
                ts,
                &Receiver::new(&t.keyspace, &t.name, "[timestamp]", CqlType::Bigint),
                cx,
            )?);
        }
        if let Some(ttl) = &using.ttl {
            plan.ttl = Some(terms::prepare(
                ttl,
                &Receiver::new(&t.keyspace, &t.name, "[ttl]", CqlType::Int),
                cx,
            )?);
        }
        if let Some(to) = &using.timeout {
            crate::schema::ddl::check_timeout(using, d)?;
            plan.timeout = Some(terms::prepare(
                to,
                &Receiver::new(&t.keyspace, &t.name, "[timeout]", CqlType::Duration),
                cx,
            )?);
        }
        Ok(())
    };
    match kind {
        StatementKind::Insert(ins) => {
            plan.kind = ModKind::Insert;
            plan.conditional = ins.if_not_exists;
            if ins.if_not_exists {
                plan.conditions = Conditions::NotExists;
            }
            if t.is_counter() {
                return Err(msg::counter_insert(d));
            }
            match &ins.values {
                InsertValues::Values { columns, values } => {
                    if columns.len() != values.len() {
                        return Err(Error::invalid("Unmatched column names/values"));
                    }
                    let mut seen: Vec<usize> = Vec::new();
                    for (c, v) in columns.iter().zip(values) {
                        let ci = column_of(t, c, d)?;
                        if seen.contains(&ci) {
                            return Err(msg::duplicate_insert_column(d, &t.columns[ci].name));
                        }
                        seen.push(ci);
                        let col = &t.columns[ci];
                        let value =
                            terms::prepare(v, &Receiver::column(t, &col.name, &col.ty), cx)?;
                        match col.kind {
                            ColumnKind::PartitionKey => {
                                plan.insert_pk[col.position as usize] = Some(value)
                            }
                            ColumnKind::Clustering => {
                                plan.insert_ck[col.position as usize] = Some(value)
                            }
                            _ => plan.ops.push(Op::Set { col: ci, value }),
                        }
                    }
                    prepare_using(&mut plan, cx)?;
                    let missing_pk: Vec<&str> = t
                        .partition_key
                        .iter()
                        .zip(&plan.insert_pk)
                        .filter(|(_, v)| v.is_none())
                        .map(|(&i, _)| t.columns[i].name.as_str())
                        .collect();
                    if !missing_pk.is_empty() {
                        return Err(msg::missing_partition_key(d, &missing_pk));
                    }
                    let any_ck = plan.insert_ck.iter().any(|v| v.is_some());
                    let only_static = !plan.ops.is_empty()
                        && plan.ops.iter().all(|o| t.columns[o.column()].is_static());
                    // COMPACT STORAGE: a dense table takes a clustering prefix.
                    let compact_prefix = crate::schema::compact::is_dense(t) && any_ck;
                    if compact_prefix {
                        let given: Vec<bool> = plan.insert_ck.iter().map(Option::is_some).collect();
                        crate::schema::compact::insert_prefix(t, &given)?;
                    }
                    let missing_ck: Vec<&str> = t
                        .clustering
                        .iter()
                        .zip(&plan.insert_ck)
                        .filter(|(_, v)| v.is_none())
                        .map(|(&i, _)| t.columns[i].name.as_str())
                        .collect();
                    if !missing_ck.is_empty() && (any_ck || !only_static) && !compact_prefix {
                        return Err(msg::missing_clustering_key(d, &missing_ck));
                    }
                    crate::schema::compact::check_sets_value(
                        t,
                        plan.ops.iter().any(|o| !t.columns[o.column()].is_static()),
                    )?;
                    plan.static_only = only_static && !any_ck && !t.clustering.is_empty();
                }
                InsertValues::Json { value, default } => {
                    let recv = match d {
                        Dialect::Scylla => Receiver::new("", "", "", CqlType::Text),
                        Dialect::Cassandra => {
                            Receiver::new(&t.keyspace, &t.name, "[json]", CqlType::Text)
                        }
                    };
                    let v = terms::prepare(value, &recv, cx)?;
                    plan.json = Some(JsonInsert {
                        value: v,
                        default_unset: matches!(default, Some(ast::JsonDefault::Unset)),
                    });
                    prepare_using(&mut plan, cx)?;
                }
            }
        }
        StatementKind::Update(up) => {
            plan.kind = ModKind::Update;
            prepare_using(&mut plan, cx)?;
            plan.conditional = !matches!(up.conditions, ast::Conditions::None);
            // Cassandra prepares the IF clause first, ScyllaDB after the assignments.
            if !d.is_scylla() {
                check_conditional_timestamp(&plan, &up.conditions)?;
                plan.conditions = crate::lwt::prepare::conditions(t, &up.conditions, cx)?;
            }
            let mut setters: Vec<(usize, bool, Option<String>)> = Vec::new();
            for a in &up.assignments {
                let ci = column_of(t, &a.column, d)?;
                let col = &t.columns[ci];
                if col.is_primary_key() {
                    return Err(msg::update_pk_column(&col.name));
                }
                let full = matches!(a.op, Operation::Set(_));
                let field = if let Operation::SetField(f, _) = &a.op {
                    Some(f.name().to_string())
                } else {
                    None
                };
                for (c, other_full, other_field) in &setters {
                    if *c == ci
                        && (full || *other_full || (field.is_some() && &field == other_field))
                    {
                        return Err(msg::multiple_setting(d, &col.name));
                    }
                }
                setters.push((ci, full, field));
                plan.ops.push(prepare_op(t, ci, a, plan.conditional, cx)?);
            }
            if d.is_scylla()
                && !plan.conditional
                && let Some(Op::Arith { col, .. }) =
                    plan.ops.iter().find(|o| matches!(o, Op::Arith { .. }))
            {
                return Err(msg::non_counter_increment(d, &t.columns[*col].name, ""));
            }
            if d.is_scylla() {
                plan.conditions = crate::lwt::prepare::conditions(t, &up.conditions, cx)?;
            }
            let op_columns: Vec<usize> = plan.ops.iter().map(Op::column).collect();
            plan.static_only = crate::lwt::prepare::applies_only_to_static(
                t,
                ModKind::Update,
                &op_columns,
                &plan.conditions,
            );
            let r = restrictions::analyze(
                t,
                StmtKind::Update,
                &up.where_clause,
                false,
                plan.static_only,
                false,
                cx,
            )?;
            crate::lwt::prepare::check_restrictions(d, &plan, &r)?;
            plan.restrictions = Some(r);
        }
        StatementKind::Delete(del) => {
            plan.kind = ModKind::Delete;
            plan.conditional = !matches!(del.conditions, ast::Conditions::None);
            if !d.is_scylla() {
                plan.conditions = crate::lwt::prepare::conditions(t, &del.conditions, cx)?;
            }
            for target in &del.columns {
                plan.deletions.push(prepare_deletion(t, target, cx)?);
            }
            prepare_using(&mut plan, cx)?;
            if d.is_scylla() {
                plan.conditions = crate::lwt::prepare::conditions(t, &del.conditions, cx)?;
            }
            let del_columns: Vec<usize> = plan.deletions.iter().map(Del::column).collect();
            plan.static_only = crate::lwt::prepare::applies_only_to_static(
                t,
                ModKind::Delete,
                &del_columns,
                &plan.conditions,
            );
            let r = restrictions::analyze(
                t,
                StmtKind::Delete,
                &del.where_clause,
                false,
                plan.static_only,
                false,
                cx,
            )?;
            crate::lwt::prepare::check_restrictions(d, &plan, &r)?;
            if !plan.deletions.is_empty()
                && !plan.static_only
                && (r.ck_prefix < t.clustering.len() || r.ck_filtered)
            {
                return Err(Error::invalid(
                    "Range deletions are not supported for specific columns",
                ));
            }
            plan.restrictions = Some(r);
        }
        _ => unreachable!(),
    }
    // `ModificationStatement.validate`
    if plan.conditional && t.is_counter() {
        return Err(Error::invalid(
            "Conditional updates are not supported on counter tables",
        ));
    }
    if plan.conditional && plan.timestamp.is_some() {
        return Err(Error::invalid(
            "Cannot provide custom timestamp for conditional updates",
        ));
    }
    if t.is_counter() && plan.kind != ModKind::Delete {
        if plan.timestamp.is_some() {
            return Err(Error::invalid(
                "Cannot provide custom timestamp for counter updates",
            ));
        }
        if plan.ttl.is_some() {
            return Err(Error::invalid(
                "Cannot provide custom TTL for counter updates",
            ));
        }
    }
    plan.needs_read = plan.ops.iter().any(|o| o.needs_read(t))
        || plan.deletions.iter().any(
            |x| matches!(x, Del::Element(c, _) if matches!(t.columns[*c].ty, CqlType::List { .. })),
        );
    plan.functions = std::mem::take(&mut cx.used);
    Ok(plan)
}

/// The `IF` clause of a modification (typed by `crate::lwt::prepare`, run by `crate::lwt`).
#[derive(Clone, Debug, Default)]
pub enum Conditions {
    #[default]
    None,
    /// `INSERT ... IF NOT EXISTS`.
    NotExists,
    /// `IF EXISTS`.
    Exists,
    List(Vec<Condition>),
}

/// One `IF` condition (`ColumnCondition`): `col op v`, `col[k] op v` or `col.f op v`.
#[derive(Clone, Debug)]
pub struct Condition {
    /// Table column index.
    pub column: usize,
    pub target: CondTarget,
    pub op: ast::Operator,
    pub value: CondValue,
}

#[derive(Clone, Debug)]
pub enum CondTarget {
    Column,
    /// A list index or map key.
    Element(Term),
    /// A UDT field position.
    Field(usize),
}

#[derive(Clone, Debug)]
pub enum CondValue {
    One(Term),
    In(InValues),
}

/// `ModificationStatement.Parsed.prepareColumnConditions`: Cassandra rejects `USING TIMESTAMP`
/// before preparing column conditions.
fn check_conditional_timestamp(plan: &ModifyPlan, conds: &ast::Conditions) -> Result<()> {
    if plan.timestamp.is_some() && matches!(conds, ast::Conditions::List(_)) {
        return Err(Error::invalid(
            "Cannot provide custom timestamp for conditional updates",
        ));
    }
    Ok(())
}

fn prepare_op(
    t: &Table,
    ci: usize,
    a: &Assignment,
    conditional: bool,
    cx: &mut PrepareCtx,
) -> Result<Op> {
    let d = cx.dialect;
    let col = &t.columns[ci];
    let ty = &col.ty;
    let recv = Receiver::column(t, &col.name, ty);
    let shown = written(a);
    match &a.op {
        Operation::Set(v) => {
            if ty.is_counter() {
                return Err(Error::invalid(format!(
                    "Cannot set the value of counter column {} (counters can only be incremented/decremented, not set)",
                    col.name
                )));
            }
            Ok(Op::Set {
                col: ci,
                value: terms::prepare(v, &recv, cx)?,
            })
        }
        Operation::SetElement(k, v) => {
            if !ty.is_collection() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for non collection column {}",
                    col.name
                )));
            }
            if !ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for frozen collection column {}",
                    col.name
                )));
            }
            match ty {
                CqlType::List { elem, .. } => {
                    let key = terms::prepare(
                        k,
                        &recv.derive(format!("idx({})", col.name), CqlType::Int),
                        cx,
                    )?;
                    let value = terms::prepare(
                        v,
                        &recv.derive(format!("value({})", col.name), (**elem).clone()),
                        cx,
                    )?;
                    Ok(Op::SetElement {
                        col: ci,
                        key,
                        value,
                    })
                }
                CqlType::Map { key, value, .. } => {
                    let k = terms::prepare(
                        k,
                        &recv.derive(format!("key({})", col.name), (**key).clone()),
                        cx,
                    )?;
                    let v = terms::prepare(
                        v,
                        &recv.derive(format!("value({})", col.name), (**value).clone()),
                        cx,
                    )?;
                    Ok(Op::SetElement {
                        col: ci,
                        key: k,
                        value: v,
                    })
                }
                _ => Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for set column {}",
                    col.name
                ))),
            }
        }
        Operation::SetField(f, v) => {
            let CqlType::Udt(def, _) = ty else {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for non-UDT column {}",
                    col.name
                )));
            };
            if !ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for frozen UDT column {}",
                    col.name
                )));
            }
            let fname = f.name();
            let Some(field) = def.field_index(&fname) else {
                return Err(Error::invalid(format!(
                    "UDT column {} does not have a field named {fname}",
                    col.name
                )));
            };
            let value = terms::prepare(
                v,
                &recv.derive(format!("{}.{fname}", col.name), def.fields[field].1.clone()),
                cx,
            )?;
            Ok(Op::SetField {
                col: ci,
                field,
                value,
            })
        }
        Operation::Add(v, _) => {
            if !ty.is_collection() {
                if ty.is_tuple() {
                    return Err(Error::invalid(format!(
                        "Invalid operation ({shown}) for tuple column {}",
                        col.name
                    )));
                }
                if !ty.is_counter() {
                    return arith_op(t, ci, v, &shown, false, conditional, cx);
                }
                return Ok(Op::Counter {
                    col: ci,
                    value: terms::prepare(v, &recv, cx)?,
                    negate: false,
                });
            }
            if !ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for frozen collection column {}",
                    col.name
                )));
            }
            if let CqlType::Map { .. } = ty
                && !d.is_scylla()
                && !matches!(
                    v,
                    ast::Term::Map(_)
                        | ast::Term::Bind(_)
                        | ast::Term::Set(_)
                        | ast::Term::Function(_)
                        | ast::Term::TypeHint(_)
                )
            {
                return Err(Error::invalid(format!(
                    "Value for a map addition has to be a map, but was: '{v}'"
                )));
            }
            Ok(Op::Add {
                col: ci,
                value: terms::prepare(v, &recv, cx)?,
            })
        }
        Operation::Subtract(v, _) => {
            if !ty.is_collection() {
                if !ty.is_counter() {
                    return arith_op(t, ci, v, &shown, true, conditional, cx);
                }
                return Ok(Op::Counter {
                    col: ci,
                    value: terms::prepare(v, &recv, cx)?,
                    negate: true,
                });
            }
            if !ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for frozen collection column {}",
                    col.name
                )));
            }
            let value = match ty {
                // A map subtraction removes a set of keys.
                CqlType::Map { key, .. } => {
                    if !d.is_scylla()
                        && !matches!(
                            v,
                            ast::Term::Set(_)
                                | ast::Term::Bind(_)
                                | ast::Term::Function(_)
                                | ast::Term::TypeHint(_)
                        )
                    {
                        return Err(Error::invalid(format!(
                            "Value for a map substraction has to be a set, but was: '{v}'"
                        )));
                    }
                    terms::prepare(
                        v,
                        &recv.derive(col.name.clone(), CqlType::set((**key).clone(), true)),
                        cx,
                    )?
                }
                _ => terms::prepare(v, &recv, cx)?,
            };
            Ok(Op::Remove { col: ci, value })
        }
        Operation::Prepend(v) => {
            if !matches!(ty, CqlType::List { .. }) {
                // Cassandra's `Prepend.toString` prints a minus.
                let shown = match d {
                    Dialect::Scylla => shown,
                    Dialect::Cassandra => format!("{} = {v} - {}", col.name, col.name),
                };
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for non list column {}",
                    col.name
                )));
            }
            if !ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid operation ({shown}) for frozen list column {}",
                    col.name
                )));
            }
            Ok(Op::Prepend {
                col: ci,
                value: terms::prepare(v, &recv, cx)?,
            })
        }
    }
}

/// `c = c ± v` on a non-counter column (only conditional statements may read the old value).
fn arith_op(
    t: &Table,
    ci: usize,
    v: &ast::Term,
    shown: &str,
    sub: bool,
    conditional: bool,
    cx: &mut PrepareCtx,
) -> Result<Op> {
    let d = cx.dialect;
    let col = &t.columns[ci];
    // ScyllaDB requires the condition once every operation is typed (the UPDATE checks it).
    let conditional = conditional || d.is_scylla();
    let check = || crate::lwt::arith::check(d, &col.ty, &col.name, shown, sub, conditional);
    if crate::lwt::arith::checks_first(d) {
        check()?;
    }
    let value = terms::prepare(v, &Receiver::column(t, &col.name, &col.ty), cx)?;
    check()?;
    Ok(Op::Arith {
        col: ci,
        value,
        sub,
    })
}

fn prepare_deletion(t: &Table, target: &Deletion, cx: &mut PrepareCtx) -> Result<Del> {
    let d = cx.dialect;
    match target {
        Deletion::Column(c) => {
            let ci = column_of(t, c, d)?;
            if t.columns[ci].is_primary_key() {
                return Err(Error::invalid(format!(
                    "Invalid identifier {} for deletion (should not be a PRIMARY KEY part)",
                    t.columns[ci].name
                )));
            }
            Ok(Del::Column(ci))
        }
        Deletion::Element(c, k) => {
            let ci = column_of(t, c, d)?;
            let col = &t.columns[ci];
            if col.is_primary_key() {
                return Err(Error::invalid(format!(
                    "Invalid identifier {} for deletion (should not be a PRIMARY KEY part)",
                    col.name
                )));
            }
            if !col.ty.is_collection() {
                return Err(Error::invalid(format!(
                    "Invalid deletion operation for non collection column {}",
                    col.name
                )));
            }
            if !col.ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Invalid deletion operation for frozen collection column {}",
                    col.name
                )));
            }
            let (name, kt) = match &col.ty {
                CqlType::List { .. } => (format!("idx({})", col.name), CqlType::Int),
                CqlType::Set { elem, .. } => (format!("value({})", col.name), (**elem).clone()),
                CqlType::Map { key, .. } => (format!("key({})", col.name), (**key).clone()),
                _ => unreachable!(),
            };
            let key = terms::prepare(k, &Receiver::new(&t.keyspace, &t.name, name, kt), cx)?;
            Ok(Del::Element(ci, key))
        }
        Deletion::Field(c, f) => {
            let ci = column_of(t, c, d)?;
            let col = &t.columns[ci];
            let CqlType::Udt(def, _) = &col.ty else {
                return Err(Error::invalid(format!(
                    "Invalid field deletion operation for non-UDT column {}",
                    col.name
                )));
            };
            if !col.ty.is_multicell() {
                return Err(Error::invalid(format!(
                    "Frozen UDT column {} does not support field deletions",
                    col.name
                )));
            }
            let fname = f.name();
            let field = def.field_index(&fname).ok_or_else(|| {
                Error::invalid(format!(
                    "UDT column {} does not have a field named {fname}",
                    col.name
                ))
            })?;
            Ok(Del::Field(ci, field))
        }
    }
}

// ---- execution -------------------------------------------------------------------------------

/// `Attributes.getTimestamp` / `getTimeToLive`.
pub fn attrs(
    db: &Database,
    plan: &ModifyPlan,
    params: &QueryParams,
    cx: &EvalCtx,
    batch_ts: Option<i64>,
) -> Result<Attrs> {
    let d = cx.dialect;
    let now = params
        .now_in_seconds
        .map(|n| n as i64)
        .unwrap_or_else(|| db.now_secs());
    let mut timestamp = None;
    if let Some(t) = &plan.timestamp {
        match t.eval(cx)? {
            Bound::Val(b) => {
                let v = <[u8; 8]>::try_from(&b[..])
                    .map(i64::from_be_bytes)
                    .map_err(|_| Error::invalid("Invalid timestamp value"))?;
                if v == i64::MIN {
                    return Err(Error::invalid(
                        "Out of bound timestamp, must be in [-9223372036854775807, 9223372036854775807]",
                    ));
                }
                check_future_timestamp(db, v)?;
                timestamp = Some(v);
            }
            Bound::Null => return Err(Error::invalid("Invalid null value of timestamp")),
            Bound::Unset => {}
        }
    }
    let timestamp = timestamp
        .or(batch_ts)
        .or(params.timestamp)
        .unwrap_or_else(|| db.timestamp());
    let mut ttl: Option<i32> = if plan.table.params.default_time_to_live > 0 {
        Some(plan.table.params.default_time_to_live)
    } else {
        None
    };
    if let Some(t) = &plan.ttl {
        match t.eval(cx)? {
            Bound::Val(b) => {
                let v = <[u8; 4]>::try_from(&b[..])
                    .map(i32::from_be_bytes)
                    .map_err(|_| Error::invalid("Invalid TTL value"))?;
                if v < 0 {
                    return Err(msg::ttl_negative(d, v as i64));
                }
                if v > crate::limits::MAX_TTL {
                    return Err(msg::ttl_too_large(v as i64));
                }
                ttl = if v == 0 { None } else { Some(v) };
            }
            Bound::Null => {
                if d.is_scylla() {
                    return Err(Error::invalid("Invalid null value of TTL"));
                }
                ttl = None;
            }
            Bound::Unset => {}
        }
    }
    if plan.table.is_counter() {
        ttl = None;
    }
    Ok(Attrs {
        timestamp,
        ttl,
        now,
    })
}

/// A key's component values evaluated for a write.
fn insert_values(
    terms_: &[Option<Term>],
    t: &Table,
    cols: &[usize],
    cx: &EvalCtx,
) -> Result<Vec<Bytes>> {
    let mut out = Vec::with_capacity(terms_.len());
    for (i, term) in terms_.iter().enumerate() {
        let name = &t.columns[cols[i]].name;
        let Some(term) = term else {
            return Err(Error::invalid(format!(
                "Invalid null value in condition for column {name}"
            )));
        };
        match term.eval(cx)? {
            Bound::Val(b) => out.push(b),
            Bound::Null => {
                return Err(Error::invalid(format!(
                    "Invalid null value in condition for column {name}"
                )));
            }
            Bound::Unset => return Err(msg::unset_routing_key(name)),
        }
    }
    Ok(out)
}

/// The targets of an execution: partition keys and, per key, clusterings or slices.
pub enum Targets {
    Rows(Vec<Clustering>),
    Slices(Vec<nosql_storage::Slice>),
    /// The whole partition (DELETE without clustering restrictions) or the static row only.
    Partition,
}

/// Partition key and clustering terms by position.
type KeyTerms = (Vec<Option<Term>>, Vec<Option<Term>>);

/// The partition keys and row targets of one execution (`buildPartitionKeyNames`,
/// `createClustering` / `createSlices`).
pub fn resolve_targets(plan: &ModifyPlan, cx: &EvalCtx) -> Result<(Vec<PartitionKey>, Targets)> {
    match &plan.json {
        Some(j) => {
            let (pk, ck, _) = json_values(&plan.table, j, cx)?;
            keys_and_targets(plan, cx, Some(&(pk, ck)))
        }
        None => keys_and_targets(plan, cx, None),
    }
}

fn keys_and_targets(
    plan: &ModifyPlan,
    cx: &EvalCtx,
    json_keys: Option<&KeyTerms>,
) -> Result<(Vec<PartitionKey>, Targets)> {
    let t = &plan.table;
    let d = cx.dialect;
    Ok(match plan.kind {
        ModKind::Insert => {
            let (pk_terms, ck_terms) = match &json_keys {
                Some((p, c)) => (p.clone(), c.clone()),
                None => (plan.insert_pk.clone(), plan.insert_ck.clone()),
            };
            let pk = insert_values(&pk_terms, t, &t.partition_key, cx)?;
            let key = storage::partition_key(&pk, d, true)?;
            let targets = if plan.static_only || t.clustering.is_empty() {
                if t.clustering.is_empty() {
                    Targets::Rows(vec![Clustering::EMPTY])
                } else {
                    Targets::Partition
                }
            } else {
                // A dense compact table's row may be a clustering prefix.
                let n = if crate::schema::compact::is_dense(t) {
                    ck_terms.iter().take_while(|c| c.is_some()).count().max(1)
                } else {
                    ck_terms.len()
                };
                let ck = insert_values(&ck_terms[..n], t, &t.clustering[..n], cx)?;
                let c = Clustering::new(ck.iter());
                crate::schema::compact::check_clustering(t, &c)?;
                storage::check_clustering_length(&c, d)?;
                Targets::Rows(vec![c])
            };
            (vec![key], targets)
        }
        _ => {
            let r = plan
                .restrictions
                .as_ref()
                .ok_or_else(|| Error::server("missing restrictions"))?;
            let keys = match r.partitions(t, cx, None)? {
                Partitions::Keys(ks) => {
                    let mut out = Vec::with_capacity(ks.len());
                    for k in ks {
                        out.push(storage::partition_key(&k, d, true)?);
                    }
                    out
                }
                Partitions::Range { .. } => return Err(Error::server("modification over a range")),
            };
            let has_ck = r.ck_prefix > 0 || r.ck_slice;
            let targets = if t.clustering.is_empty() {
                Targets::Rows(vec![Clustering::EMPTY])
            } else if plan.static_only || !has_ck {
                Targets::Partition
            } else if r.ck_prefix == t.clustering.len()
                || (plan.kind == ModKind::Update && crate::schema::compact::is_dense(t))
            {
                let cs = r.clusterings(t, cx, None)?;
                for c in &cs {
                    if plan.kind != ModKind::Delete {
                        crate::schema::compact::check_clustering(t, c)?;
                    }
                    storage::check_clustering_length(c, d)?;
                }
                Targets::Rows(cs)
            } else {
                match r.clustering(t, cx, None)? {
                    ClusteringFilter::Slices(s) => Targets::Slices(s),
                    ClusteringFilter::Names(n) => Targets::Rows(n),
                }
            };
            (keys, targets)
        }
    })
}

/// Builds the updates of one execution (no observers, not applied).
pub async fn updates(
    db: &Arc<Database>,
    plan: &ModifyPlan,
    cx: &EvalCtx<'_>,
    a: &Attrs,
) -> Result<Vec<PartitionUpdate>> {
    updates_after(db, plan, cx, a, &[]).await
}

/// [`updates`] of a batch member: counter increments read the counter cells the batch's earlier
/// members wrote (`pending`), so increments of one counter add up as Cassandra's counter
/// mutations do.
pub async fn updates_after(
    db: &Arc<Database>,
    plan: &ModifyPlan,
    cx: &EvalCtx<'_>,
    a: &Attrs,
    pending: &[PartitionUpdate],
) -> Result<Vec<PartitionUpdate>> {
    let t = &plan.table;
    // JSON values become regular operations.
    let mut json_ops: Vec<Op> = Vec::new();
    let mut json_keys: Option<KeyTerms> = None;
    if let Some(j) = &plan.json {
        let (pk, ck, ops) = json_values(t, j, cx)?;
        json_ops = ops;
        json_keys = Some((pk, ck));
    }
    let (keys, targets) = keys_and_targets(plan, cx, json_keys.as_ref())?;
    let ops: Vec<Op> = if json_keys.is_some() {
        json_ops
    } else {
        plan.ops.clone()
    };
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let mut u = PartitionUpdate::new(key.clone());
        match plan.kind {
            ModKind::Delete if plan.deletions.is_empty() => match &targets {
                Targets::Partition => u.deletion = a.deletion(),
                Targets::Rows(cs) => {
                    for c in cs {
                        if t.clustering.is_empty() {
                            u.deletion = a.deletion();
                        } else {
                            u.rows.push(
                                Row::new(c.clone())
                                    .with_deletion(RowDeletion::regular(a.deletion())),
                            );
                        }
                    }
                }
                Targets::Slices(slices) => {
                    for s in slices {
                        u.range_tombstones.push(RangeTombstone::new(
                            s.start.clone(),
                            s.end.clone(),
                            a.deletion(),
                        ));
                    }
                }
            },
            ModKind::Delete => {
                let current = if plan.needs_read {
                    read_rows(db, t, &key, &targets, a.now).await?
                } else {
                    Vec::new()
                };
                let clusterings: Vec<Clustering> = match &targets {
                    Targets::Rows(cs) => cs.clone(),
                    _ => vec![Clustering::EMPTY],
                };
                for c in clusterings {
                    let mut row = Row::new(c.clone());
                    let mut stat = Row::new(Clustering::EMPTY);
                    let existing = current.iter().find(|r| r.clustering == c);
                    let existing_static = current.iter().find(|r| r.clustering.is_empty());
                    for del in &plan.deletions {
                        let col = &t.columns[del.column()];
                        let target_row = if col.is_static() { &mut stat } else { &mut row };
                        let ex = if col.is_static() {
                            existing_static
                        } else {
                            existing
                        };
                        delete_column(t, del, col, target_row, ex, a, cx)?;
                    }
                    if !stat.columns.is_empty() {
                        u.static_row = Some(stat);
                    }
                    if !row.columns.is_empty() && !plan.static_only {
                        u.rows.push(row);
                    }
                }
            }
            ModKind::Insert | ModKind::Update => {
                let mut current = if plan.needs_read {
                    read_rows(db, t, &key, &targets, a.now).await?
                } else {
                    Vec::new()
                };
                if t.is_counter() {
                    overlay_counters(&mut current, &key, pending);
                }
                let clusterings: Vec<Clustering> = match &targets {
                    Targets::Rows(cs) => cs.clone(),
                    _ => Vec::new(),
                };
                let mut stat = Row::new(Clustering::EMPTY);
                let existing_static = current
                    .iter()
                    .find(|r| r.clustering.is_empty() && !t.clustering.is_empty());
                for op in ops.iter().filter(|o| t.columns[o.column()].is_static()) {
                    apply_op(t, op, &mut stat, existing_static, a, cx)?;
                }
                if !stat.columns.is_empty() {
                    u.static_row = Some(stat);
                }
                for c in clusterings {
                    let mut row = Row::new(c.clone());
                    // COMPACT STORAGE rows have no marker (they exist while their value does).
                    if plan.kind == ModKind::Insert && crate::schema::compact::insert_marks_rows(t)
                    {
                        row.liveness = a.liveness();
                    }
                    let existing = current.iter().find(|r| r.clustering == c);
                    for op in ops.iter().filter(|o| !t.columns[o.column()].is_static()) {
                        apply_op(t, op, &mut row, existing, a, cx)?;
                    }
                    if !row.columns.is_empty() || !row.liveness.is_empty() {
                        u.rows.push(row);
                    }
                }
            }
        }
        for r in &mut u.rows {
            r.columns.sort_by_key(|c| c.column());
        }
        if let Some(s) = &mut u.static_row {
            s.columns.sort_by_key(|c| c.column());
        }
        out.push(u);
    }
    Ok(out)
}

/// Reads the current rows (and static row) a read-before-write needs.
async fn read_rows(
    db: &Arc<Database>,
    t: &Table,
    key: &PartitionKey,
    targets: &Targets,
    now: i64,
) -> Result<Vec<Row>> {
    let st = db.storage_table(t)?;
    let mut r = PartitionRead::new(key.clone(), now);
    if let Targets::Rows(cs) = targets
        && !t.clustering.is_empty()
    {
        r.clustering = ClusteringFilter::Names(cs.clone());
    }
    let mut stream = st.read(r);
    let mut rows = Vec::new();
    while let Some(c) = stream.next().await {
        let c = c.map_err(Error::internal)?;
        if let Some(s) = c.static_row {
            rows.push(s);
        }
        rows.extend(c.rows);
    }
    Ok(rows)
}

/// Lays the counter cells of `pending` updates of `key` over the rows read for it.
fn overlay_counters(rows: &mut Vec<Row>, key: &PartitionKey, pending: &[PartitionUpdate]) {
    for u in pending.iter().filter(|u| u.key == *key) {
        for r in u.static_row.iter().chain(&u.rows) {
            let i = match rows.iter().position(|x| x.clustering == r.clustering) {
                Some(i) => i,
                None => {
                    rows.push(Row::new(r.clustering.clone()));
                    rows.len() - 1
                }
            };
            let target = &mut rows[i];
            for c in &r.columns {
                if let ColumnData::Simple { column, cell } = c
                    && cell.counter_parts().is_some()
                {
                    let data = ColumnData::simple(*column, cell.clone());
                    match target.columns.binary_search_by_key(column, |c| c.column()) {
                        Ok(j) => target.columns[j] = data,
                        Err(j) => target.columns.insert(j, data),
                    }
                }
            }
        }
    }
}

/// Removes from `earlier` the counter cells `later` (built over them) supersedes.
pub fn supersede_counters(earlier: &mut PartitionUpdate, later: &PartitionUpdate) {
    if earlier.key != later.key {
        return;
    }
    let strip = |row: &mut Row, by: &Row| {
        row.columns.retain(|c| {
            !(matches!(c, ColumnData::Simple { cell, .. } if cell.counter_parts().is_some())
                && by
                    .cell(c.column())
                    .is_some_and(|n| n.counter_parts().is_some()))
        });
    };
    if let (Some(s), Some(by)) = (&mut earlier.static_row, &later.static_row) {
        strip(s, by);
    }
    for r in &mut earlier.rows {
        if let Some(by) = later.rows.iter().find(|b| b.clustering == r.clustering) {
            strip(r, by);
        }
    }
    earlier.rows.retain(|r| !r.is_empty());
    if earlier.static_row.as_ref().is_some_and(Row::is_empty) {
        earlier.static_row = None;
    }
}

/// Live cells of a complex column of an existing row.
fn existing_cells(existing: Option<&Row>, col_id: u32, now: i64) -> Vec<&Cell> {
    match existing.and_then(|r| r.column(col_id)) {
        Some(ColumnData::Complex {
            cells, deletion, ..
        }) => cells
            .iter()
            .filter(|c| c.is_live(now) && !deletion.deletes(c.timestamp))
            .collect(),
        _ => Vec::new(),
    }
}

/// Appends `data` to `row` (merging with an existing entry for the column).
fn push(row: &mut Row, data: ColumnData) {
    let id = data.column();
    if let Some(pos) = row.columns.iter().position(|c| c.column() == id) {
        match (&mut row.columns[pos], data) {
            (
                ColumnData::Complex {
                    cells, deletion, ..
                },
                ColumnData::Complex {
                    cells: more,
                    deletion: d2,
                    ..
                },
            ) => {
                cells.extend(more);
                *deletion = deletion.max(d2);
            }
            (slot, other) => *slot = other,
        }
    } else {
        row.columns.push(data);
    }
}

/// Timeuuid cell paths for appended list elements: strictly increasing.
fn append_paths(n: usize) -> Vec<Bytes> {
    (0..n)
        .map(|_| Bytes::copy_from_slice(&nosql_types::timeuuid::now()))
        .collect()
}

/// Paths for prepended elements: before every earlier element (`Lists.Prepender`, reference time
/// 2010-01-01 mirrored backwards).
fn prepend_paths(n: usize) -> Vec<Bytes> {
    const REFERENCE_MS: i64 = 1_262_304_000_000;
    static LAST: AtomicI64 = AtomicI64::new(i64::MAX);
    let now_ms = nosql_types::temporal::now_millis();
    let mut base = (REFERENCE_MS - (now_ms - REFERENCE_MS)) * 1000;
    // Strictly before the previous prepend's first element.
    let prev = LAST.load(Ordering::Relaxed);
    if base + n as i64 > prev {
        base = prev - n as i64 - 1;
    }
    LAST.store(base, Ordering::Relaxed);
    let lsb = nosql_types::timeuuid::global().clock_seq_and_node();
    (0..n)
        .map(|i| {
            Bytes::copy_from_slice(&nosql_types::timeuuid::at_unix_micros(base + i as i64, lsb))
        })
        .collect()
}

fn eval(term: &Term, cx: &EvalCtx) -> Result<Bound> {
    term.eval(cx)
}

/// Splits a serialised list or set value into its elements.
fn elements(b: &[u8]) -> Result<Vec<Bytes>> {
    let es = nosql_types::collection_elements(b).map_err(|e| Error::invalid(e.message))?;
    let mut out = Vec::with_capacity(es.len());
    for e in es {
        match e {
            Some(x) => out.push(Bytes::copy_from_slice(x)),
            None => return Err(Error::invalid("null is not supported inside collections")),
        }
    }
    Ok(out)
}

fn entries(b: &[u8]) -> Result<Vec<(Bytes, Bytes)>> {
    Ok(nosql_types::map_entries(b)
        .map_err(|e| Error::invalid(e.message))?
        .into_iter()
        .map(|(k, v)| (Bytes::copy_from_slice(k), Bytes::copy_from_slice(v)))
        .collect())
}

/// The cells of a whole non-frozen collection / UDT value.
fn complex_cells(ty: &CqlType, b: &[u8], a: &Attrs) -> Result<Vec<Cell>> {
    Ok(match ty {
        CqlType::List { .. } => {
            let es = elements(b)?;
            let paths = append_paths(es.len());
            es.into_iter()
                .zip(paths)
                .map(|(e, p)| a.cell(e).with_path(p))
                .collect()
        }
        CqlType::Set { .. } => elements(b)?
            .into_iter()
            .map(|e| a.cell(Bytes::new()).with_path(e))
            .collect(),
        CqlType::Map { .. } => entries(b)?
            .into_iter()
            .map(|(k, v)| a.cell(v).with_path(k))
            .collect(),
        CqlType::Udt(..) => {
            let fs = nosql_types::tuple_fields(b).map_err(|e| Error::invalid(e.message))?;
            fs.into_iter()
                .enumerate()
                .filter_map(|(i, f)| {
                    f.map(|x| {
                        a.cell(Bytes::copy_from_slice(x))
                            .with_path(storage::field_path(i))
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    })
}

fn apply_op(
    t: &Table,
    op: &Op,
    row: &mut Row,
    existing: Option<&Row>,
    a: &Attrs,
    cx: &EvalCtx,
) -> Result<()> {
    let col = &t.columns[op.column()];
    let id = col.id;
    match op {
        Op::Set { value, .. } => match eval(value, cx)? {
            Bound::Unset => {}
            Bound::Null => {
                if col.ty.is_multicell() {
                    push(
                        row,
                        ColumnData::complex(id, a.complex_deletion(), Vec::new()),
                    );
                } else {
                    push(row, ColumnData::simple(id, a.tombstone()));
                }
            }
            Bound::Val(b) => {
                if col.ty.is_multicell() {
                    let cells = complex_cells(&col.ty, &b, a)?;
                    push(row, ColumnData::complex(id, a.complex_deletion(), cells));
                } else {
                    push(row, ColumnData::simple(id, a.cell(b)));
                }
            }
        },
        Op::Add { value, .. } => match eval(value, cx)? {
            Bound::Unset | Bound::Null => {}
            Bound::Val(b) => {
                let cells = complex_cells(&col.ty, &b, a)?;
                push(row, ColumnData::complex(id, DeletionTime::LIVE, cells));
            }
        },
        Op::Prepend { value, .. } => match eval(value, cx)? {
            Bound::Unset | Bound::Null => {}
            Bound::Val(b) => {
                let es = elements(&b)?;
                let paths = prepend_paths(es.len());
                let cells = es
                    .into_iter()
                    .zip(paths)
                    .map(|(e, p)| a.cell(e).with_path(p))
                    .collect();
                push(row, ColumnData::complex(id, DeletionTime::LIVE, cells));
            }
        },
        Op::Remove { value, .. } => match eval(value, cx)? {
            Bound::Unset | Bound::Null => {}
            Bound::Val(b) => {
                let cells: Vec<Cell> = match &col.ty {
                    CqlType::List { elem, .. } => {
                        let remove = elements(&b)?;
                        existing_cells(existing, id, a.now)
                            .into_iter()
                            .filter(|c| {
                                remove
                                    .iter()
                                    .any(|r| nosql_types::compare(elem, &c.value, r).is_eq())
                            })
                            .filter_map(|c| c.path.clone().map(|p| a.tombstone().with_path(p)))
                            .collect()
                    }
                    _ => elements(&b)?
                        .into_iter()
                        .map(|e| a.tombstone().with_path(e))
                        .collect(),
                };
                push(row, ColumnData::complex(id, DeletionTime::LIVE, cells));
            }
        },
        Op::SetElement { key, value, .. } => {
            let k = match eval(key, cx)? {
                Bound::Val(k) => k,
                Bound::Null => {
                    return Err(Error::invalid(if matches!(col.ty, CqlType::List { .. }) {
                        "Invalid null value for list index"
                    } else {
                        "Invalid null map key"
                    }));
                }
                Bound::Unset => {
                    if let (true, Term::Bind { index, .. }) = (cx.dialect.is_scylla(), key) {
                        return Err(terms::unexpected_unset(*index));
                    }
                    return Err(Error::invalid(if matches!(col.ty, CqlType::List { .. }) {
                        "Invalid unset value for list index"
                    } else {
                        "Invalid unset map key"
                    }));
                }
            };
            let v = eval(value, cx)?;
            if v.is_unset() {
                return Ok(());
            }
            let path = match &col.ty {
                CqlType::List { .. } => {
                    let current = existing_cells(existing, id, a.now);
                    if current.is_empty() {
                        return Err(Error::invalid(
                            "Attempted to set an element on a list which is null",
                        ));
                    }
                    let idx = <[u8; 4]>::try_from(&k[..])
                        .map(i32::from_be_bytes)
                        .map_err(|_| Error::invalid("Invalid list index"))?;
                    if idx < 0 || idx as usize >= current.len() {
                        return Err(Error::invalid(format!(
                            "List index {idx} out of bound, list has size {}",
                            current.len()
                        )));
                    }
                    current[idx as usize].path.clone().unwrap_or_default()
                }
                _ => k,
            };
            let cell = match v {
                Bound::Val(b) => a.cell(b),
                _ => a.tombstone(),
            };
            push(
                row,
                ColumnData::complex(id, DeletionTime::LIVE, vec![cell.with_path(path)]),
            );
        }
        Op::SetField { field, value, .. } => {
            let cell = match eval(value, cx)? {
                Bound::Unset => return Ok(()),
                Bound::Null => a.tombstone(),
                Bound::Val(b) => a.cell(b),
            };
            push(
                row,
                ColumnData::complex(
                    id,
                    DeletionTime::LIVE,
                    vec![cell.with_path(storage::field_path(*field))],
                ),
            );
        }
        Op::Counter { value, negate, .. } => {
            let delta = match eval(value, cx)? {
                Bound::Val(b) => <[u8; 8]>::try_from(&b[..])
                    .map(i64::from_be_bytes)
                    .map_err(|_| Error::invalid("Invalid counter increment"))?,
                Bound::Null => {
                    return Err(Error::invalid("Invalid null value for counter increment"));
                }
                Bound::Unset => return Ok(()),
            };
            let delta = if *negate { delta.wrapping_neg() } else { delta };
            let (clock, total) = existing
                .and_then(|r| r.cell(id))
                .filter(|c| c.is_live(a.now))
                .and_then(|c| c.counter_parts())
                .unwrap_or((0, 0));
            // Several increments of one column in one statement add up.
            let (clock, total) = match row.cell(id).and_then(|c| c.counter_parts()) {
                Some((c2, t2)) => (c2, t2),
                None => (clock, total),
            };
            push(
                row,
                ColumnData::simple(
                    id,
                    Cell::counter(a.timestamp, clock + 1, total.wrapping_add(delta)),
                ),
            );
        }
        Op::Arith { value, sub, .. } => {
            // Cassandra sees the statement's own pending write (`getPrefetchedRow` merges it).
            let pending = (!cx.dialect.is_scylla())
                .then(|| {
                    row.cell(id)
                        .filter(|c| c.is_live(a.now))
                        .map(|c| c.value.clone())
                })
                .flatten();
            let current = pending.or_else(|| {
                existing
                    .and_then(|r| r.cell(id))
                    .filter(|c| c.is_live(a.now))
                    .map(|c| c.value.clone())
            });
            match crate::lwt::arith::apply(&col.ty, current.as_deref(), value, *sub, cx)? {
                crate::lwt::arith::Write::Nothing => {}
                crate::lwt::arith::Write::Null => push(row, ColumnData::simple(id, a.tombstone())),
                crate::lwt::arith::Write::Value(v) => push(row, ColumnData::simple(id, a.cell(v))),
            }
        }
    }
    let _ = t;
    Ok(())
}

fn delete_column(
    t: &Table,
    del: &Del,
    col: &crate::schema::Column,
    row: &mut Row,
    existing: Option<&Row>,
    a: &Attrs,
    cx: &EvalCtx,
) -> Result<()> {
    let id = col.id;
    match del {
        Del::Column(_) => {
            if col.ty.is_multicell() {
                push(row, ColumnData::complex(id, a.deletion(), Vec::new()));
            } else if col.ty.is_counter() {
                push(
                    row,
                    ColumnData::simple(id, Cell::counter_tombstone(a.timestamp, a.now)),
                );
            } else {
                push(row, ColumnData::simple(id, a.tombstone()));
            }
        }
        Del::Element(_, key) => {
            let k = match key.eval(cx)? {
                Bound::Val(k) => k,
                Bound::Null => {
                    return Err(Error::invalid(match &col.ty {
                        CqlType::List { .. } => "Invalid null value for list index",
                        CqlType::Map { .. } => "Invalid null map key",
                        _ => "Invalid null set element",
                    }));
                }
                // An unset list index deletes nothing; a map key must be set.
                Bound::Unset if matches!(col.ty, CqlType::Map { .. }) => {
                    return Err(match (cx.dialect, key) {
                        (Dialect::Scylla, Term::Bind { index, .. }) => {
                            terms::unexpected_unset(*index)
                        }
                        _ => Error::invalid("Invalid unset map key"),
                    });
                }
                Bound::Unset => return Ok(()),
            };
            let path = match &col.ty {
                CqlType::List { .. } => {
                    let current = existing_cells(existing, id, a.now);
                    if current.is_empty() {
                        return Err(Error::invalid(
                            "Attempted to delete an element from a list which is null",
                        ));
                    }
                    let idx = <[u8; 4]>::try_from(&k[..])
                        .map(i32::from_be_bytes)
                        .map_err(|_| Error::invalid("Invalid list index"))?;
                    if idx < 0 || idx as usize >= current.len() {
                        return Err(Error::invalid(format!(
                            "List index {idx} out of bound, list has size {}",
                            current.len()
                        )));
                    }
                    current[idx as usize].path.clone().unwrap_or_default()
                }
                _ => k,
            };
            push(
                row,
                ColumnData::complex(id, DeletionTime::LIVE, vec![a.tombstone().with_path(path)]),
            );
        }
        Del::Field(_, field) => {
            push(
                row,
                ColumnData::complex(
                    id,
                    DeletionTime::LIVE,
                    vec![a.tombstone().with_path(storage::field_path(*field))],
                ),
            );
        }
    }
    let _ = t;
    Ok(())
}

/// Jackson's `MismatchedInputException` for a JSON document that isn't an object.
fn json_not_a_map(text: &str, parsed: &nosql_types::Json) -> String {
    use nosql_types::Json;
    let (what, token) = match parsed {
        Json::List(_) => ("Array value", "JsonToken.START_ARRAY"),
        Json::Bool(true) => ("Boolean value", "JsonToken.VALUE_TRUE"),
        Json::Bool(false) => ("Boolean value", "JsonToken.VALUE_FALSE"),
        Json::Double(_) => ("Floating-point value", "JsonToken.VALUE_NUMBER_FLOAT"),
        Json::Str(s) => {
            return format!(
                "Could not decode JSON string as a map: com.fasterxml.jackson.databind.exc.MismatchedInputException: Cannot construct instance of `java.util.LinkedHashMap` (although at least one Creator exists): no String-argument constructor/factory method to deserialize from String value ('{s}')\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: 1, column: {}]. (String was: {text})",
                json_token_column(text)
            );
        }
        _ => ("Integer value", "JsonToken.VALUE_NUMBER_INT"),
    };
    format!(
        "Could not decode JSON string as a map: com.fasterxml.jackson.databind.exc.MismatchedInputException: Cannot deserialize value of type `java.util.LinkedHashMap<java.lang.Object,java.lang.Object>` from {what} (token `{token}`)\n at [Source: REDACTED (`StreamReadFeature.INCLUDE_SOURCE_IN_LOCATION` disabled); line: 1, column: {}]. (String was: {text})",
        json_token_column(text)
    )
}

/// The 1-based column of the first token (Jackson's token location).
fn json_token_column(text: &str) -> usize {
    let line = text.rsplit('\n').next().unwrap_or(text);
    let skipped = text.len() - text.trim_start().len();
    if text[..skipped].contains('\n') {
        line.len() - line.trim_start().len() + 1
    } else {
        skipped + 1
    }
}

/// `INSERT JSON`: the key terms and operations its object describes.
#[allow(clippy::type_complexity)]
fn json_values(
    t: &Table,
    j: &JsonInsert,
    cx: &EvalCtx,
) -> Result<(Vec<Option<Term>>, Vec<Option<Term>>, Vec<Op>)> {
    let d = cx.dialect;
    let text = match j.value.eval(cx)? {
        Bound::Val(b) => String::from_utf8_lossy(&b).into_owned(),
        _ => return Err(Error::invalid("Got null for INSERT JSON values")),
    };
    // ScyllaDB parses with RapidJSON (the message carries its own prefix); Cassandra reads a
    // `Map` with Jackson and reports the exception class.
    let parsed = nosql_types::parse_json(&text, d).map_err(|e| match d {
        Dialect::Scylla => Error::server(e.message),
        Dialect::Cassandra => {
            let m = e.message.strip_prefix("Error decoding JSON string: ").unwrap_or(&e.message);
            Error::invalid(format!("Could not decode JSON string as a map: com.fasterxml.jackson.core.JsonParseException: {m}. (String was: {text})"))
        }
    })?;
    let nosql_types::Json::Map(fields) = parsed else {
        return Err(match d {
            Dialect::Scylla => Error::server("JSON assert failed on condition 'IsObject()'"),
            Dialect::Cassandra => Error::invalid(json_not_a_map(&text, &parsed)),
        });
    };
    let mut values: Vec<Option<Option<Bytes>>> = vec![None; t.columns.len()];
    for (k, v) in &fields {
        // `JsonUtils.handleCaseSensitivity`: quoted keys keep their case, others are lowercased.
        let k = if k.len() >= 2 && k.starts_with('"') && k.ends_with('"') {
            k[1..k.len() - 1].to_string()
        } else {
            k.to_lowercase()
        };
        let Some(ci) = t.columns.iter().position(|c| c.name == k) else {
            return Err(Error::invalid(format!(
                "JSON values map contains unrecognized column: {k}"
            )));
        };
        let col = &t.columns[ci];
        if matches!(v, nosql_types::Json::Null) {
            values[ci] = Some(None);
            continue;
        }
        let b = nosql_types::from_json(&col.ty, v, d).map_err(|e| match d {
            Dialect::Scylla => Error::server(e.message),
            Dialect::Cassandra => Error::invalid(format!(
                "Error decoding JSON value for {}: {}",
                col.name, e.message
            )),
        })?;
        values[ci] = Some(Some(b));
    }
    let mut pk = vec![None; t.partition_key.len()];
    let mut ck = vec![None; t.clustering.len()];
    let mut ops = Vec::new();
    for (ci, col) in t.columns.iter().enumerate() {
        let v = values[ci].clone();
        match col.kind {
            ColumnKind::PartitionKey | ColumnKind::Clustering => {
                let term = match v {
                    Some(Some(b)) => Some(Term::Const(Some(b))),
                    _ => {
                        if d.is_scylla() {
                            return Err(msg::missing_partition_key(d, &[col.name.as_str()]));
                        }
                        return Err(Error::invalid(format!(
                            "Invalid null value in condition for column {}",
                            col.name
                        )));
                    }
                };
                if col.kind == ColumnKind::PartitionKey {
                    pk[col.position as usize] = term;
                } else {
                    ck[col.position as usize] = term;
                }
            }
            _ => match v {
                Some(val) => ops.push(Op::Set {
                    col: ci,
                    value: Term::Const(val),
                }),
                None if !j.default_unset => ops.push(Op::Set {
                    col: ci,
                    value: Term::Const(None),
                }),
                None => {}
            },
        }
    }
    Ok((pk, ck, ops))
}

/// Executes a prepared modification.
/// Executes a prepared modification. Boxed so the future is checked `Send` here, with named
/// lifetimes (rust-lang/rust#110338 otherwise rejects callers' futures as not general enough).
pub fn execute<'a>(
    db: &'a Arc<Database>,
    plan: &'a ModifyPlan,
    params: &'a QueryParams,
    stmt: &'a Statement<'static>,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_inner(db, plan, params, stmt))
}

async fn execute_inner(
    db: &Arc<Database>,
    plan: &ModifyPlan,
    params: &QueryParams,
    stmt: &Statement<'static>,
) -> Result<Response> {
    let d = db.dialect();
    let cx = EvalCtx::new(&params.values, d);
    let mut warnings = crate::guardrails::write_cl::check(db, params.consistency)?;
    let timeout = match &plan.timeout {
        Some(to) => crate::schema::ddl::check_timeout_value(to, &cx)?,
        None => None,
    };
    if plan.table.is_virtual() {
        // ScyllaDB's virtual tables (`system.config` takes live option updates).
        let a = attrs(db, plan, params, &cx, None)?;
        let ups = updates(db, plan, &cx, &a).await?;
        crate::guardrails::config_table::apply(
            db,
            &plan.table,
            &ups,
            params.consistency,
            plan.conditional,
        )?;
        return Ok(Response::void().with_warnings(warnings));
    }
    if plan.table.is_counter() && params.consistency == crate::error::Consistency::Any {
        return Err(Error::invalid(format!(
            "Consistency level ANY is not yet supported for counter table {}",
            plan.table.name
        )));
    }
    if plan.conditional {
        let Some(h) = db.hooks().conditional else {
            return Err(Error::invalid("Conditional updates are not supported yet"));
        };
        let request = crate::hooks::ConditionalRequest {
            table: &plan.table,
            statement: Some(stmt),
            params,
            members: vec![(plan, &params.values[..])],
            batch: false,
        };
        let mut r = h.execute(db, request).await?;
        warnings.append(&mut r.warnings);
        r.warnings = warnings;
        return Ok(r);
    }
    if params.consistency.is_serial() {
        return Err(crate::lwt::serial_write_error(plan.table.is_counter()));
    }
    let a = attrs(db, plan, params, &cx, None)?;
    let _guards = if plan.needs_read {
        Some(lock_keys(db, plan, &cx).await?)
    } else {
        None
    };
    let ups = updates(db, plan, &cx, &a).await?;
    let bypass = matches!(&stmt.kind, StatementKind::Delete(x) if x.bypass_guardrails);
    let writes: Vec<(&Table, &PartitionUpdate)> = ups.iter().map(|u| (&*plan.table, u)).collect();
    let write = crate::guardrails::large_data::Write {
        consistency: params.consistency,
        write_type: if plan.table.is_counter() { "COUNTER" } else { "SIMPLE" },
        replica: !bypass,
    };
    warnings.extend(crate::guardrails::large_data::check(db, &writes, write).await?);
    if crate::schema::ddl::no_time(timeout.as_ref()) {
        return Err(crate::schema::ddl::write_timed_out(
            &format!("{}.{}", plan.table.keyspace, plan.table.name),
            params.consistency,
            write.write_type,
        ));
    }
    if !plan.table.is_counter() {
        let tokens: Vec<i64> = ups.iter().map(|u| u.key.token).collect();
        crate::rate_limit::check(db, &plan.table, &tokens, crate::rate_limit::Op::Write)?;
    }
    apply_with_observers(db, &plan.table, ups)
        .await
        .map_err(|e| {
            timed_out_at(
                db,
                e,
                &format!("{}.{}", plan.table.keyspace, plan.table.name),
                params.consistency,
                write.write_type,
            )
        })?;
    Ok(Response::void().with_warnings(warnings))
}

/// ScyllaDB's `restrict_future_timestamp`: a `USING TIMESTAMP` more than 3 days ahead.
pub fn check_future_timestamp(db: &Database, ts: i64) -> Result<()> {
    const AHEAD_MICROS: i64 = 3 * 86_400 * 1_000_000;
    if db.dialect().is_scylla()
        && db.settings().scylla.restrict_future_timestamp
        && ts > db.timestamp().saturating_add(AHEAD_MICROS)
    {
        return Err(Error::invalid(
            "Cannot provide a timestamp more than 3 days into the future. If this was not intended, make sure the timestamp is in microseconds. You can also disable this check by setting the restrict_future_timestamp configuration option to false.",
        ));
    }
    Ok(())
}

/// Takes the partition locks a read-before-write needs (in a fixed order).
async fn lock_keys<'a>(
    db: &'a Arc<Database>,
    plan: &ModifyPlan,
    cx: &EvalCtx<'_>,
) -> Result<Vec<tokio::sync::MutexGuard<'a, ()>>> {
    let t = &plan.table;
    let mut tokens: Vec<i64> = Vec::new();
    match plan.kind {
        ModKind::Insert => {
            let pk = insert_values(&plan.insert_pk, t, &t.partition_key, cx)?;
            tokens.push(storage::partition_key(&pk, cx.dialect, true)?.token);
        }
        _ => {
            if let Some(r) = &plan.restrictions
                && let Partitions::Keys(ks) = r.partitions(t, cx, None)?
            {
                for k in ks {
                    tokens.push(storage::partition_key(&k, cx.dialect, true)?.token);
                }
            }
        }
    }
    tokens.sort_unstable();
    tokens.dedup();
    // Each stripe once, in ascending order (no deadlock between statements).
    let mut stripes: Vec<usize> = tokens.iter().map(|&tok| db.stripe(&t.id, tok)).collect();
    stripes.sort_unstable();
    stripes.dedup();
    let mut guards = Vec::with_capacity(stripes.len());
    for s in stripes {
        guards.push(db.lock_stripe(s).await);
    }
    Ok(guards)
}

/// A write that timed out (memtables full: `Database::apply`) restated at the statement's
/// consistency and write type.
pub(crate) fn timed_out_at(
    db: &Database,
    e: Error,
    table: &str,
    cl: crate::error::Consistency,
    write_type: &str,
) -> Error {
    if e.code != crate::error::ErrorCode::WriteTimeout {
        return e;
    }
    crate::schema::ddl::write_timed_out_in(db.dialect(), table, cl, write_type)
}

/// Applies updates of one table with the write observers' derived updates, atomically.
pub async fn apply_with_observers(
    db: &Arc<Database>,
    table: &Arc<Table>,
    ups: Vec<PartitionUpdate>,
) -> Result<()> {
    apply_writes(db, ups.into_iter().map(|u| (table.clone(), u)).collect()).await
}

/// Applies the updates of one write (a statement or a batch) with the derived updates the write
/// observers return, atomically; the observers' locks are held until it is applied.
pub async fn apply_writes(
    db: &Arc<Database>,
    writes: Vec<(Arc<Table>, PartitionUpdate)>,
) -> Result<()> {
    let observers = db.hooks().observers;
    let mut derived: Vec<(TableId, PartitionUpdate)> = Vec::new();
    let mut guards = Vec::new();
    for o in &observers {
        let d = o.on_write(db, &writes).await?;
        derived.extend(d.updates);
        guards.extend(d.guards);
    }
    let mut items: Vec<(TableId, PartitionUpdate)> = writes
        .into_iter()
        .map(|(t, u)| (t.table_id(), u))
        .collect();
    items.extend(derived);
    let r = db.apply(items).await;
    drop(guards);
    r
}

#[allow(dead_code)]
fn bound_start(c: Clustering) -> SBound {
    SBound::inclusive_start(c)
}
