//! WHERE clause analysis: which partitions and rows a statement touches, and what is left to
//! filter.
//!
//! Ports the semantics of Cassandra's `cql3.restrictions.StatementRestrictions` and its
//! collaborators (`SingleColumnRestriction`, `MultiColumnRestriction`, `TokenRestriction`,
//! `ClusteringColumnRestrictions`, `PartitionKeySingleRestrictionSet`, Apache-2.0), with
//! ScyllaDB's differences learned from its documentation and the reference image: ScyllaDB
//! accepts several restrictions on one column (their intersection), warns rather than fails on
//! clustering-only restrictions, and words several errors differently.
//!
//! The analysis is structural (done once, at prepare time). Execution binds the values and asks
//! for: the partition keys or token range ([`Restrictions::partitions`]), the clustering filter
//! storage can apply ([`Restrictions::clustering`]), and the residual row filter
//! ([`Restrictions::filter`]). Whatever storage can't express exactly is evaluated on each row,
//! so the result is always exact.
//!
//! Indexes come in through `PrepareCtx::indexes` (`crate::hooks::IndexPlanner`): a restriction
//! an index supports lets the query through without `ALLOW FILTERING` (Cassandra's
//! `hasQueriableIndex`), [`Restrictions::uses_index`] records that the read goes through one, and
//! the planner decides whether what is left still needs filtering (`needFiltering`).

use bytes::Bytes;
use nosql_cql::ast::{self, Operator, Relation};
use nosql_storage::read::ClusteringFilter;
use nosql_storage::sstable::cursor::KeyBound;
use nosql_storage::{Bound as SBound, Clustering, Slice};
use nosql_types::{CqlType, Dialect};

use super::terms::{self, Bound, EvalCtx, Term};
use super::{PrepareCtx, Receiver};
use crate::error::{Error, Result};
use crate::messages as msg;
use crate::schema::{ColumnKind, Table};

/// Which statement the restrictions belong to (`StatementType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmtKind {
    Select,
    Update,
    Delete,
}

impl StmtKind {
    pub fn name(self) -> &'static str {
        match self {
            StmtKind::Select => "SELECT",
            StmtKind::Update => "UPDATE",
            StmtKind::Delete => "DELETE",
        }
    }
}

/// The values of an `IN`.
#[derive(Clone, Debug)]
pub enum InValues {
    Terms(Vec<Term>),
    /// `IN ?`: one serialised `list<T>`.
    Marker(Term),
}

/// A restriction on one column.
#[derive(Clone, Debug)]
pub enum Pred {
    Eq(Term),
    In(InValues),
    /// ScyllaDB `NOT IN`.
    NotIn(InValues),
    /// `>`/`>=` (`lower`) or `<`/`<=`.
    Slice {
        lower: bool,
        inclusive: bool,
        value: Term,
    },
    Contains(Term),
    ContainsKey(Term),
    /// `m[k] = v`.
    Entry(Term, Term),
    /// ScyllaDB `LIKE` (filtering).
    Like(Term),
}

impl Pred {
    pub fn is_eq_or_in(&self) -> bool {
        matches!(self, Pred::Eq(_) | Pred::In(_))
    }

    pub fn is_slice(&self) -> bool {
        matches!(self, Pred::Slice { .. })
    }

    pub fn is_contains(&self) -> bool {
        matches!(
            self,
            Pred::Contains(_) | Pred::ContainsKey(_) | Pred::Entry(..)
        )
    }

    fn label(&self) -> &'static str {
        match self {
            Pred::Eq(_) => "Equal",
            Pred::In(_) => "IN",
            _ => "",
        }
    }
}

/// A multi-column restriction on consecutive clustering columns.
#[derive(Clone, Debug)]
pub struct Multi {
    /// Clustering positions.
    pub positions: Vec<usize>,
    pub kind: MultiKind,
}

#[derive(Clone, Debug)]
pub enum MultiKind {
    /// A serialised tuple.
    Eq(Term),
    In(InValues),
    Slice {
        lo: Option<(Term, bool)>,
        hi: Option<(Term, bool)>,
    },
}

/// A `token(…)` restriction.
#[derive(Clone, Debug, Default)]
pub struct TokenPred {
    pub eq: Option<Term>,
    pub lo: Option<(Term, bool)>,
    pub hi: Option<(Term, bool)>,
    /// The relations as ScyllaDB prints them in messages (`(token(pk) = 1)`).
    pub text: Vec<String>,
}

/// How partitions are found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyMode {
    /// Every partition key column restricted by `=` / `IN`.
    Keys,
    /// A token range (from `token(…)`).
    TokenRange,
    /// A full scan.
    Scan,
}

/// The analysed WHERE clause.
#[derive(Clone, Debug)]
pub struct Restrictions {
    pub kind: StmtKind,
    /// By partition key position.
    pub pk: Vec<Vec<Pred>>,
    pub token: Option<TokenPred>,
    /// By clustering position.
    pub ck: Vec<Vec<Pred>>,
    pub multi: Vec<Multi>,
    /// Regular and static columns: `(table column index, predicates)`.
    pub others: Vec<(usize, Vec<Pred>)>,
    /// `IS NOT NULL` columns (views).
    pub not_null: Vec<usize>,
    pub key_mode: KeyMode,
    /// Leading clustering columns restricted by `=` / `IN` that storage applies.
    pub ck_prefix: usize,
    /// Whether the column after the prefix has a slice storage applies (bounds only; the exact
    /// check is a filter when it comes from a multi-column slice).
    pub ck_slice: bool,
    /// Partition key predicates evaluated per row.
    pub pk_filtered: bool,
    /// Some clustering predicate is evaluated per row.
    pub ck_filtered: bool,
    /// Warnings to send with every execution.
    pub warnings: Vec<String>,
    /// ScyllaDB filters a key range by clustering restrictions without ALLOW FILTERING, which
    /// Cassandra refuses (`Settings::strict_allow_filtering` decides).
    pub implicit_filtering: bool,
    /// The read goes through a secondary index (`usesSecondaryIndexing`).
    pub uses_index: bool,
    /// The partition key / clustering restrictions are part of the index query's row filter.
    pub index_filters_pk: bool,
    pub index_filters_ck: bool,
    /// An index is used but some restriction still needs `ALLOW FILTERING`.
    pub index_needs_filtering: bool,
}

fn token_receiver(t: &Table) -> Receiver {
    Receiver::new(&t.keyspace, &t.name, "partition key token", CqlType::Bigint)
}

/// The receiver of `IN ?` (`in(c)` in Cassandra, `IN(c)` in ScyllaDB).
fn in_receiver(t: &Table, name: &str, ty: &CqlType, d: Dialect) -> Receiver {
    let name = match d {
        Dialect::Scylla => format!("IN({name})"),
        Dialect::Cassandra => format!("in({name})"),
    };
    Receiver::new(&t.keyspace, &t.name, name, CqlType::list(ty.clone(), false))
}

/// Analyses `where_clause` for a statement of `kind` on `table`.
///
/// `static_only`: the statement reads or writes only static columns (and the partition key).
pub fn analyze(
    table: &Table,
    kind: StmtKind,
    where_clause: &[Relation],
    allow_filtering: bool,
    static_only: bool,
    for_view: bool,
    cx: &mut PrepareCtx,
) -> Result<Restrictions> {
    let d = cx.dialect;
    let mut r = Restrictions {
        kind,
        pk: vec![Vec::new(); table.partition_key.len()],
        token: None,
        ck: vec![Vec::new(); table.clustering.len()],
        multi: Vec::new(),
        others: Vec::new(),
        not_null: Vec::new(),
        key_mode: KeyMode::Scan,
        ck_prefix: 0,
        ck_slice: false,
        pk_filtered: false,
        ck_filtered: false,
        warnings: Vec::new(),
        implicit_filtering: false,
        uses_index: false,
        index_filters_pk: false,
        index_filters_ck: false,
        index_needs_filtering: false,
    };
    // Restricted clustering columns (for Cassandra's merge checks).
    let mut ck_order: Vec<CkEntry> = Vec::new();
    for rel in where_clause {
        match rel {
            Relation::Single(s) => add_single(
                table,
                kind,
                rel,
                s,
                allow_filtering,
                for_view,
                &mut r,
                &mut ck_order,
                cx,
            )?,
            Relation::MultiColumn(m) => {
                add_multi(table, rel, m, allow_filtering, &mut r, &mut ck_order, cx)?
            }
            Relation::Token(t) => add_token(table, rel, t, &mut r, cx)?,
            Relation::Expression(_) => {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => "Custom index expressions are not supported",
                    Dialect::Cassandra => {
                        "Custom index expressions are not supported by this table"
                    }
                }));
            }
            Relation::Function(f) => {
                return Err(crate::index::fulltext::function_relation_error(f));
            }
        }
    }
    process(
        table,
        kind,
        allow_filtering,
        static_only,
        for_view,
        &mut r,
        cx,
    )?;
    Ok(r)
}

/// A restricted clustering column: its position, the first column of the restriction covering
/// it, and whether that restriction is a slice.
#[derive(Clone, Copy)]
struct CkEntry {
    pos: usize,
    first: usize,
    slice: bool,
}

fn mixing_error() -> Error {
    Error::invalid(
        "Mixing single column relations and multi column relations on clustering columns is not allowed",
    )
}

/// `ClusteringColumnRestrictions.mergeWith`: a restriction starting at `start` added after the
/// existing ones (`order`, sorted by position).
fn merge_order_check(table: &Table, order: &[CkEntry], start: usize, slice: bool) -> Result<()> {
    let Some(last) = order.last() else {
        return Ok(());
    };
    let name = |p: usize| &table.columns[table.clustering[p]].name;
    if last.slice && start > last.first {
        return Err(Error::invalid(format!(
            "Clustering column \"{}\" cannot be restricted (preceding column \"{}\" is restricted by a non-EQ relation)",
            name(start),
            name(last.first)
        )));
    }
    if start < last.first && slice {
        let next = order
            .iter()
            .map(|e| e.pos)
            .find(|&p| p > start)
            .unwrap_or(start + 1);
        return Err(Error::invalid(format!(
            "PRIMARY KEY column \"{}\" cannot be restricted (preceding column \"{}\" is restricted by a non-EQ relation)",
            name(next),
            name(start)
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn add_single(
    table: &Table,
    kind: StmtKind,
    rel: &Relation,
    s: &ast::SingleRelation,
    allow_filtering: bool,
    for_view: bool,
    r: &mut Restrictions,
    ck_order: &mut Vec<CkEntry>,
    cx: &mut PrepareCtx,
) -> Result<()> {
    let d = cx.dialect;
    let name = s.column.name();
    let Some(ci) = table.column_index(&name) else {
        return Err(msg::undefined_column(
            d,
            &table.keyspace,
            &table.name,
            &name,
        ));
    };
    let col = &table.columns[ci];
    // Cassandra refuses CONTAINS in writes first (`Cannot use UPDATE with CONTAINS`); ScyllaDB
    // checks the column's type first (below).
    let contains_in_write =
        matches!(s.op, Operator::Contains | Operator::ContainsKey) && kind != StmtKind::Select;
    if contains_in_write && !d.is_scylla() {
        return Err(Error::invalid(format!(
            "Cannot use {} with {}",
            kind.name(),
            s.op.as_str()
        )));
    }
    if s.op == Operator::IsNot {
        if d.is_scylla() && for_view && col.ty.is_collection() && col.ty.is_multicell() {
            return Err(Error::invalid(format!(
                "Collection column '{}' ({}) cannot be restricted by a 'IS NOT' relation",
                col.name,
                nosql_types::cql_name(&col.ty)
            )));
        }
        if !for_view {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!(
                    "restriction '{} IS NOT null' is only supported in materialized view creation",
                    s.column
                ),
                Dialect::Cassandra => format!("Unsupported restriction: {rel}"),
            }));
        }
        r.not_null.push(ci);
        return Ok(());
    }
    if s.op == Operator::Neq {
        return Err(Error::invalid(format!(
            "Unsupported \"!=\" relation: {rel}"
        )));
    }
    // `toReceivers`
    let ty = col.ty.clone();
    if s.op == Operator::ContainsKey && !matches!(ty, CqlType::Map { .. }) {
        return Err(Error::invalid(format!(
            "Cannot use CONTAINS KEY on non-map column {}",
            col.name
        )));
    }
    if s.op == Operator::Contains && !ty.is_collection() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot use CONTAINS on non-collection column \"{}\"",
                col.name
            ),
            Dialect::Cassandra => {
                format!("Cannot use CONTAINS on non-collection column {}", col.name)
            }
        }));
    }
    // (A non-key column goes on to "Invalid where clause contains non PRIMARY KEY columns".)
    if contains_in_write && col.is_primary_key() {
        return Err(Error::invalid(format!(
            "Cannot use {} with {}",
            s.op.as_str(),
            kind.name()
        )));
    }
    if s.key.is_some() {
        match (&ty, d) {
            (CqlType::Map { .. }, _) => {}
            // ScyllaDB filters on list elements by position.
            (CqlType::List { .. }, Dialect::Scylla) => {}
            (CqlType::List { .. }, Dialect::Cassandra) => {
                return Err(Error::invalid(format!(
                    "Indexes on list entries ({}[index] = value) are not currently supported.",
                    col.name
                )));
            }
            (CqlType::Set { .. }, Dialect::Scylla) => {
                return Err(Error::invalid(format!(
                    "Column {} cannot be subscripted",
                    col.name
                )));
            }
            (_, Dialect::Scylla) => {
                return Err(Error::invalid(format!(
                    "Column {} is not a map/set/list, cannot be subscripted",
                    col.name
                )));
            }
            (_, Dialect::Cassandra) => {
                return Err(Error::invalid(format!(
                    "Column {} cannot be used as a map",
                    col.name
                )));
            }
        }
        if !ty.is_multicell() {
            return Err(Error::invalid(if matches!(ty, CqlType::Map { .. }) {
                format!(
                    "Map-entry equality predicates on frozen map column {} are not supported",
                    col.name
                )
            } else {
                format!(
                    "List element equality predicates on frozen list column {} are not supported",
                    col.name
                )
            }));
        }
        if s.op != Operator::Eq {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "Only EQ relations are supported on map/list entries",
                Dialect::Cassandra => "Only EQ relations are supported on map entries",
            }));
        }
    }
    if matches!(
        s.op,
        Operator::Lt | Operator::Lte | Operator::Gt | Operator::Gte
    ) && ty.references_duration()
    {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!("Duration type is unordered for {}", col.name),
            Dialect::Cassandra => if ty.is_collection() {
                "Slice restrictions are not supported on collections containing durations"
            } else if ty.is_tuple() {
                "Slice restrictions are not supported on tuples containing durations"
            } else if ty.is_udt() {
                "Slice restrictions are not supported on UDTs containing durations"
            } else {
                "Slice restrictions are not supported on duration columns"
            }
            .to_string(),
        }));
    }
    // ScyllaDB compares a non-frozen UDT as a whole value.
    if ty.is_udt() && ty.is_multicell() && !d.is_scylla() {
        return Err(Error::invalid(format!(
            "Non-frozen UDT column '{}' ({}) cannot be restricted by any relation",
            col.name,
            nosql_types::cql_name(&ty)
        )));
    }
    if ty.is_collection()
        && ty.is_multicell()
        && !(matches!(s.op, Operator::Contains | Operator::ContainsKey) || s.key.is_some())
    {
        return Err(Error::invalid(format!(
            "Collection column '{}' ({}) cannot be restricted by a '{}' relation",
            col.name,
            nosql_types::cql_name(&ty),
            s.op.as_str()
        )));
    }
    let receiver = Receiver::column(table, &col.name, &ty);
    let pred = match s.op {
        Operator::Eq => {
            if let Some(k) = &s.key {
                let (key, value, kname) = match &ty {
                    CqlType::Map { key, value, .. } => (key.clone(), value.clone(), "key"),
                    CqlType::List { elem, .. } => {
                        (std::sync::Arc::new(CqlType::Int), elem.clone(), "index")
                    }
                    _ => return Err(Error::invalid("Invalid subscripted relation")),
                };
                let kt = prepare_value(
                    k,
                    &receiver.derive(format!("{kname}({})", col.name), (*key).clone()),
                    cx,
                )?;
                let vt = prepare_relation_term(
                    &s.value,
                    &receiver.derive(format!("value({})", col.name), (*value).clone()),
                    cx,
                )?;
                Pred::Entry(kt, vt)
            } else {
                Pred::Eq(prepare_relation_term(&s.value, &receiver, cx)?)
            }
        }
        Operator::Lt | Operator::Lte | Operator::Gt | Operator::Gte => {
            let value = prepare_relation_term(&s.value, &receiver, cx)?;
            Pred::Slice {
                lower: matches!(s.op, Operator::Gt | Operator::Gte),
                inclusive: matches!(s.op, Operator::Gte | Operator::Lte),
                value,
            }
        }
        Operator::In | Operator::NotIn => {
            // ScyllaDB names the list `IN(c)` / `NOT IN(c)`.
            let list = if s.op == Operator::NotIn {
                format!("NOT IN({})", col.name)
            } else {
                format!("IN({})", col.name)
            };
            let values = match &s.value {
                ast::Value::List(items) => {
                    // ScyllaDB types the items as elements of the list.
                    let item = match d {
                        Dialect::Scylla => receiver.derive(format!("value({list})"), ty.clone()),
                        Dialect::Cassandra => receiver.clone(),
                    };
                    let mut v = Vec::with_capacity(items.len());
                    for it in items {
                        v.push(terms::prepare(it, &item, cx)?);
                    }
                    InValues::Terms(v)
                }
                ast::Value::ListMarker(m) => {
                    let mut rr = in_receiver(table, &col.name, &ty, d);
                    if d.is_scylla() {
                        rr.name = list.clone();
                    }
                    let index = cx.bind(m, &rr);
                    InValues::Marker(Term::Bind {
                        index,
                        ty: rr.ty.clone(),
                        receiver: rr.name.clone(),
                    })
                }
                ast::Value::Term(t) => InValues::Terms(vec![terms::prepare(t, &receiver, cx)?]),
            };
            if s.op == Operator::NotIn {
                Pred::NotIn(values)
            } else {
                match values {
                    InValues::Terms(mut v) if v.len() == 1 && !d.is_scylla() => {
                        Pred::Eq(v.remove(0))
                    }
                    other => Pred::In(other),
                }
            }
        }
        Operator::Contains | Operator::ContainsKey => {
            let et = match (&ty, s.op) {
                (CqlType::Map { key, .. }, Operator::ContainsKey) => (**key).clone(),
                (CqlType::Map { value, .. }, _) => (**value).clone(),
                (CqlType::List { elem, .. } | CqlType::Set { elem, .. }, _) => (**elem).clone(),
                _ => ty.clone(),
            };
            let name = if s.op == Operator::ContainsKey {
                format!("key({})", col.name)
            } else {
                format!("value({})", col.name)
            };
            let v = prepare_relation_term(&s.value, &receiver.derive(name, et), cx)?;
            if s.op == Operator::ContainsKey {
                Pred::ContainsKey(v)
            } else {
                Pred::Contains(v)
            }
        }
        Operator::Like => {
            let v = prepare_relation_term(&s.value, &receiver, cx)?;
            let pred = Pred::Like(v);
            let indexed = kind == StmtKind::Select
                && cx
                    .indexes
                    .as_ref()
                    .is_some_and(|p| p.supports(table, ci, &pred));
            if !d.is_scylla() && !indexed {
                return Err(Error::invalid(format!(
                    "LIKE restriction is only supported on properly indexed columns. {rel} is not valid."
                )));
            }
            pred
        }
        Operator::IsNot | Operator::Neq => unreachable!(),
    };
    match col.kind {
        ColumnKind::PartitionKey => {
            let pos = col.position as usize;
            if !d.is_scylla() {
                merge_check(&r.pk[pos], &pred, &col.name)?;
            }
            r.pk[pos].push(pred);
        }
        ColumnKind::Clustering => {
            let pos = col.position as usize;
            if !d.is_scylla() {
                merge_check(&r.ck[pos], &pred, &col.name)?;
                if let Some(new) = Shape::of_single(&pred, pos) {
                    for m in r.multi.iter().filter(|m| m.positions.contains(&pos)) {
                        Shape::of_multi(&m.kind, m.positions[0]).merge(&new, &col.name, table)?;
                    }
                }
            }
            if d.is_scylla() && !r.multi.is_empty() {
                return Err(mixing_error());
            }
            // `ClusteringColumnRestrictions.mergeWith`
            if !allow_filtering {
                merge_order_check(table, ck_order, pos, pred.is_slice())?;
            }
            let slice = pred.is_slice() || r.ck[pos].iter().any(Pred::is_slice);
            match ck_order.iter_mut().find(|e| e.pos == pos) {
                Some(e) => e.slice |= slice,
                None => ck_order.push(CkEntry {
                    pos,
                    first: pos,
                    slice,
                }),
            }
            ck_order.sort_by_key(|e| e.pos);
            r.ck[pos].push(pred);
        }
        _ => {
            if let Some(e) = r.others.iter_mut().find(|(i, _)| *i == ci) {
                if !d.is_scylla() {
                    merge_check(&e.1, &pred, &col.name)?;
                }
                e.1.push(pred);
            } else {
                r.others.push((ci, vec![pred]));
            }
        }
    }
    Ok(())
}

/// A clustering restriction as Cassandra merges it with a multi-column one: its kind, its first
/// column and, for slices, which bounds it sets.
struct Shape {
    kind: ShapeKind,
    first: usize,
}

#[derive(PartialEq)]
enum ShapeKind {
    Eq,
    In,
    Slice { lo: bool, hi: bool },
}

impl Shape {
    /// A single-column restriction that converts to a multi-column one.
    fn of_single(p: &Pred, pos: usize) -> Option<Shape> {
        let kind = match p {
            Pred::Eq(_) => ShapeKind::Eq,
            Pred::In(_) => ShapeKind::In,
            Pred::Slice { lower, .. } => ShapeKind::Slice {
                lo: *lower,
                hi: !*lower,
            },
            _ => return None,
        };
        Some(Shape { kind, first: pos })
    }

    fn of_multi(k: &MultiKind, first: usize) -> Shape {
        let kind = match k {
            MultiKind::Eq(_) => ShapeKind::Eq,
            MultiKind::In(_) => ShapeKind::In,
            MultiKind::Slice { lo, hi } => ShapeKind::Slice {
                lo: lo.is_some(),
                hi: hi.is_some(),
            },
        };
        Shape { kind, first }
    }

    /// `MultiColumnRestriction.*.doMergeWith` of this (existing) restriction with `new`;
    /// `common`: the shared column names.
    fn merge(&self, new: &Shape, common: &str, table: &Table) -> Result<()> {
        match (&self.kind, &new.kind) {
            (ShapeKind::Eq, _) => Err(Error::invalid(format!(
                "{common} cannot be restricted by more than one relation if it includes an Equal"
            ))),
            (ShapeKind::In, _) => Err(Error::invalid(format!(
                "{common} cannot be restricted by more than one relation if it includes a IN"
            ))),
            (ShapeKind::Slice { .. }, ShapeKind::Eq | ShapeKind::In) => {
                Err(Error::invalid(format!(
                    "Column \"{common}\" cannot be restricted by both an equality and an inequality relation"
                )))
            }
            (ShapeKind::Slice { lo, hi }, ShapeKind::Slice { lo: nlo, hi: nhi }) => {
                if self.first != new.first {
                    let col = self.first.max(new.first);
                    return Err(Error::invalid(format!(
                        "Column \"{}\" cannot be restricted by two inequalities not starting with the same column",
                        table.columns[table.clustering[col]].name
                    )));
                }
                if (*lo && *nlo) || (*hi && *nhi) {
                    let side = if *lo && *nlo { "start" } else { "end" };
                    return Err(Error::invalid(format!(
                        "More than one restriction was found for the {side} bound on {common}"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Cassandra's `doMergeWith` rules for two restrictions on one column.
fn merge_check(existing: &[Pred], new: &Pred, column: &str) -> Result<()> {
    for e in existing {
        // `LikeRestriction.doMergeWith`.
        if matches!(e, Pred::Like(_)) {
            return Err(Error::invalid(format!(
                "{column} cannot be restricted by more than one relation if it includes a LIKE"
            )));
        }
        if e.is_eq_or_in() {
            return Err(Error::invalid(format!(
                "{column} cannot be restricted by more than one relation if it includes {}",
                if matches!(e, Pred::Eq(_)) {
                    "an Equal".to_string()
                } else {
                    "a IN".to_string()
                }
            )));
        }
        if e.is_slice() {
            let Pred::Slice { lower, .. } = new else {
                if new.is_contains() {
                    return Err(Error::invalid(format!(
                        "Collection column {column} can only be restricted by CONTAINS, CONTAINS KEY, or map-entry equality"
                    )));
                }
                return Err(Error::invalid(format!(
                    "Column \"{column}\" cannot be restricted by both an equality and an inequality relation"
                )));
            };
            if let Pred::Slice { lower: el, .. } = e
                && el == lower
            {
                let side = if *lower { "start" } else { "end" };
                return Err(Error::invalid(format!(
                    "More than one restriction was found for the {side} bound on {column}"
                )));
            }
        }
        if e.is_contains() && !new.is_contains() {
            return Err(Error::invalid(format!(
                "Collection column {column} can only be restricted by CONTAINS, CONTAINS KEY, or map-entry equality"
            )));
        }
        let _ = e.label();
    }
    Ok(())
}

/// A relation's right-hand side as a single term (`IN` lists are handled by the caller).
fn prepare_relation_term(v: &ast::Value, r: &Receiver, cx: &mut PrepareCtx) -> Result<Term> {
    match v {
        ast::Value::Term(t) => prepare_value(t, r, cx),
        ast::Value::List(items) if items.len() == 1 => prepare_value(&items[0], r, cx),
        ast::Value::List(_) | ast::Value::ListMarker(_) => Err(Error::invalid("Invalid IN value")),
    }
}

fn prepare_value(t: &ast::Term, r: &Receiver, cx: &mut PrepareCtx) -> Result<Term> {
    terms::prepare(t, r, cx)
}

fn add_multi(
    table: &Table,
    rel: &Relation,
    m: &ast::MultiColumnRelation,
    allow_filtering: bool,
    r: &mut Restrictions,
    ck_order: &mut Vec<CkEntry>,
    cx: &mut PrepareCtx,
) -> Result<()> {
    let d = cx.dialect;
    if m.op == Operator::Neq {
        return Err(Error::invalid(format!(
            "Unsupported \"!=\" relation: {rel}"
        )));
    }
    let mut positions = Vec::with_capacity(m.columns.len());
    let mut prev: Option<i32> = None;
    for c in &m.columns {
        let name = c.name();
        let Some(ci) = table.column_index(&name) else {
            return Err(msg::undefined_column(
                d,
                &table.keyspace,
                &table.name,
                &name,
            ));
        };
        let col = &table.columns[ci];
        if col.kind != ColumnKind::Clustering {
            return Err(Error::invalid(format!(
                "Multi-column relations can only be applied to clustering columns but was applied to: {}",
                col.name
            )));
        }
        if positions.contains(&(col.position as usize)) {
            return Err(Error::invalid(format!(
                "Column \"{}\" appeared twice in a relation: {rel}",
                col.name
            )));
        }
        if let Some(p) = prev
            && col.position != p + 1
        {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "Clustering columns may not be skipped in multi-column relations. They should appear in the PRIMARY KEY order".to_string(),
                Dialect::Cassandra => format!("Clustering columns must appear in the PRIMARY KEY order in multi-column relations: {rel}"),
            }));
        }
        prev = Some(col.position);
        positions.push(col.position as usize);
    }
    // ScyllaDB: every multi-column relation starts at the first clustering column.
    if d.is_scylla() && positions.first().is_some_and(|&p| p != 0) {
        return Err(Error::invalid(
            "Clustering columns may not be skipped in multi-column relations. They should appear in the PRIMARY KEY order",
        ));
    }
    if !matches!(
        m.op,
        Operator::Eq | Operator::In | Operator::Lt | Operator::Lte | Operator::Gt | Operator::Gte
    ) {
        return Err(Error::invalid(format!(
            "{} cannot be used for multi-column relations",
            m.op.as_str()
        )));
    }
    let types: Vec<CqlType> = positions
        .iter()
        .map(|&p| table.columns[table.clustering[p]].ty.clone())
        .collect();
    let names: Vec<String> = positions
        .iter()
        .map(|&p| table.columns[table.clustering[p]].name.clone())
        .collect();
    let tuple_ty = CqlType::Tuple(types.clone());
    let receiver = Receiver::new(
        &table.keyspace,
        &table.name,
        format!("({})", names.join(",")),
        tuple_ty.clone(),
    );
    let tuple_term = |t: &ast::Term, cx: &mut PrepareCtx| -> Result<Term> {
        if let ast::Term::Tuple(items) = t
            && items.len() != types.len()
        {
            return Err(Error::invalid(format!(
                "Expected {} elements in value tuple, but got {}: {t}",
                types.len(),
                items.len()
            )));
        }
        if !matches!(t, ast::Term::Tuple(_) | ast::Term::Bind(_)) {
            return Err(Error::invalid(format!(
                "Expected {} elements in value tuple, but got 1: ({t})",
                types.len()
            )));
        }
        match (d, t) {
            (Dialect::Cassandra, ast::Term::Tuple(items)) => {
                terms::prepare_tuple_named(items, &receiver, Some(&names), cx)
            }
            _ => terms::prepare(t, &receiver, cx),
        }
    };
    let kind = match m.op {
        Operator::Eq => {
            let ast::Value::Term(t) = &m.value else {
                return Err(Error::invalid("Invalid multi-column value"));
            };
            MultiKind::Eq(tuple_term(t, cx)?)
        }
        Operator::In => match &m.value {
            ast::Value::List(items) => {
                let mut v = Vec::new();
                for it in items {
                    v.push(tuple_term(it, cx)?);
                }
                MultiKind::In(InValues::Terms(v))
            }
            ast::Value::ListMarker(mk) => {
                let name = match d {
                    Dialect::Scylla => format!("IN(({}))", names.join(",")),
                    Dialect::Cassandra => format!("in({})", names.join(",")),
                };
                let rr = Receiver::new(
                    &table.keyspace,
                    &table.name,
                    name,
                    CqlType::list(tuple_ty.clone(), false),
                );
                let index = cx.bind(mk, &rr);
                MultiKind::In(InValues::Marker(Term::Bind {
                    index,
                    ty: rr.ty.clone(),
                    receiver: rr.name.clone(),
                }))
            }
            ast::Value::Term(t) => MultiKind::In(InValues::Terms(vec![tuple_term(t, cx)?])),
        },
        op => {
            let ast::Value::Term(t) = &m.value else {
                return Err(Error::invalid("Invalid multi-column value"));
            };
            let v = tuple_term(t, cx)?;
            let incl = matches!(op, Operator::Gte | Operator::Lte);
            if matches!(op, Operator::Gt | Operator::Gte) {
                MultiKind::Slice {
                    lo: Some((v, incl)),
                    hi: None,
                }
            } else {
                MultiKind::Slice {
                    lo: None,
                    hi: Some((v, incl)),
                }
            }
        }
    };
    let first = positions[0];
    if d.is_scylla() && r.ck.iter().any(|p| !p.is_empty()) {
        return Err(mixing_error());
    }
    // Merge with existing multi-column restrictions (`MultiColumnRestriction.doMergeWith`).
    if let Some(existing) = r
        .multi
        .iter_mut()
        .find(|e| e.positions.iter().any(|p| positions.contains(p)))
    {
        let common: Vec<String> = existing
            .positions
            .iter()
            .filter(|p| positions.contains(p))
            .map(|&p| table.columns[table.clustering[p]].name.clone())
            .collect();
        let common = common.join(", ");
        match (&mut existing.kind, kind) {
            (MultiKind::Slice { lo, hi }, MultiKind::Slice { lo: nlo, hi: nhi }) => {
                if existing.positions[0] != first {
                    let col = if existing.positions[0] > first {
                        existing.positions[0]
                    } else {
                        first
                    };
                    return Err(Error::invalid(format!(
                        "Column \"{}\" cannot be restricted by two inequalities not starting with the same column",
                        table.columns[table.clustering[col]].name
                    )));
                }
                if (lo.is_some() && nlo.is_some()) || (hi.is_some() && nhi.is_some()) {
                    let side = if nlo.is_some() { "start" } else { "end" };
                    return Err(Error::invalid(format!(
                        "More than one restriction was found for the {side} bound on {common}"
                    )));
                }
                if nlo.is_some() {
                    *lo = nlo;
                }
                if nhi.is_some() {
                    *hi = nhi;
                }
                if positions.len() > existing.positions.len() {
                    existing.positions = positions;
                }
                return Ok(());
            }
            // Cassandra: the existing restriction's `doMergeWith` decides; ScyllaDB reports an
            // equality next to a slice either way.
            (MultiKind::Slice { .. }, _) | (_, MultiKind::Slice { .. }) if d.is_scylla() => {
                return Err(Error::invalid(format!(
                    "Column \"{common}\" cannot be restricted by both an equality and an inequality relation"
                )));
            }
            (MultiKind::Slice { .. }, _) => {
                return Err(Error::invalid(format!(
                    "Column \"{common}\" cannot be restricted by both an equality and an inequality relation"
                )));
            }
            (MultiKind::Eq(_), _) => {
                return Err(Error::invalid(format!(
                    "{common} cannot be restricted by more than one relation if it includes an Equal"
                )));
            }
            (MultiKind::In(_), _) => {
                return Err(Error::invalid(format!(
                    "{common} cannot be restricted by more than one relation if it includes a IN"
                )));
            }
        }
    }
    // Cassandra: a single-column restriction on a shared column merges as a multi-column one
    // (`SingleColumnRestriction.mergeWith` → `toMultiColumnRestriction`).
    if !d.is_scylla() {
        let new = Shape::of_multi(&kind, first);
        for &p in &positions {
            for e in &r.ck[p] {
                if let Some(old) = Shape::of_single(e, p) {
                    old.merge(&new, &table.columns[table.clustering[p]].name, table)?;
                }
            }
        }
    }
    if !allow_filtering {
        merge_order_check(
            table,
            ck_order,
            first,
            matches!(kind, MultiKind::Slice { .. }),
        )?;
    }
    let is_slice = matches!(kind, MultiKind::Slice { .. });
    for &p in &positions {
        match ck_order.iter_mut().find(|e| e.pos == p) {
            Some(e) => e.slice |= is_slice,
            None => ck_order.push(CkEntry {
                pos: p,
                first,
                slice: is_slice,
            }),
        }
    }
    ck_order.sort_by_key(|e| e.pos);
    r.multi.push(Multi { positions, kind });
    Ok(())
}

fn add_token(
    table: &Table,
    _rel: &Relation,
    t: &ast::TokenRelation,
    r: &mut Restrictions,
    cx: &mut PrepareCtx,
) -> Result<()> {
    let d = cx.dialect;
    let mut cols = Vec::new();
    for c in &t.columns {
        let name = c.name();
        let Some(ci) = table.column_index(&name) else {
            return Err(msg::undefined_column(
                d,
                &table.keyspace,
                &table.name,
                &name,
            ));
        };
        cols.push(ci);
    }
    if cols != table.partition_key {
        let pk_names: Vec<&str> = table
            .partition_key_columns()
            .map(|c| c.name.as_str())
            .collect();
        match d {
            Dialect::Cassandra => {
                if !table.partition_key.iter().all(|p| cols.contains(p)) {
                    return Err(Error::invalid(
                        "The token() function must be applied to all partition key components or none of them",
                    ));
                }
                let mut sorted = cols.clone();
                sorted.sort_unstable();
                sorted.dedup();
                if sorted.len() != cols.len() {
                    return Err(Error::invalid(
                        "The token() function contains duplicate partition key components",
                    ));
                }
                if cols.iter().any(|c| !table.partition_key.contains(c)) {
                    return Err(Error::invalid(
                        "The token() function must contains only partition key components",
                    ));
                }
            }
            Dialect::Scylla => {
                if cols.len() != table.partition_key.len() {
                    return Err(Error::invalid(format!(
                        "Invalid number of arguments in call to function system.token: {} required but {} provided",
                        table.partition_key.len(),
                        cols.len()
                    )));
                }
                let mut sorted = cols.clone();
                sorted.sort_unstable();
                sorted.dedup();
                if sorted.len() != cols.len() {
                    return Err(Error::invalid(
                        "The token() function contains duplicate partition key components",
                    ));
                }
            }
        }
        return Err(Error::invalid(format!(
            "The token function arguments must be in the partition key order: {}",
            pk_names.join(", ")
        )));
    }
    if t.op == Operator::Neq {
        let cols: Vec<String> = t.columns.iter().map(|c| c.to_string()).collect();
        return Err(Error::invalid(format!(
            "Unsupported \"!=\" relation: token({}) != {}",
            cols.join(", "),
            t.value
        )));
    }
    if !matches!(
        t.op,
        Operator::Eq | Operator::Lt | Operator::Lte | Operator::Gt | Operator::Gte
    ) {
        return Err(Error::invalid(format!(
            "{} cannot be used with the token function",
            t.op.as_str()
        )));
    }
    let value = terms::prepare(&t.value, &token_receiver(table), cx)?;
    let names: Vec<&str> = table
        .partition_key_columns()
        .map(|c| c.name.as_str())
        .collect();
    let names = names.join(", ");
    let tp = r.token.get_or_insert_with(TokenPred::default);
    let written: Vec<String> = t.columns.iter().map(|c| c.name().to_string()).collect();
    tp.text.push(format!(
        "(token({}) {} {})",
        written.join(", "),
        t.op.as_str(),
        t.value
    ));
    if tp.eq.is_some() {
        return Err(Error::invalid(format!(
            "{names} cannot be restricted by more than one relation if it includes an Equal"
        )));
    }
    match t.op {
        Operator::Eq => {
            if tp.lo.is_some() || tp.hi.is_some() {
                return Err(Error::invalid(format!(
                    "Columns \"{names}\" cannot be restricted by both an equality and an inequality relation"
                )));
            }
            tp.eq = Some(value);
        }
        op => {
            let incl = matches!(op, Operator::Gte | Operator::Lte);
            if matches!(op, Operator::Gt | Operator::Gte) {
                if tp.lo.is_some() && !d.is_scylla() {
                    return Err(Error::invalid(format!(
                        "More than one restriction was found for the start bound on {names}"
                    )));
                }
                tp.lo = Some((value, incl));
            } else {
                if tp.hi.is_some() && !d.is_scylla() {
                    return Err(Error::invalid(format!(
                        "More than one restriction was found for the end bound on {names}"
                    )));
                }
                tp.hi = Some((value, incl));
            }
        }
    }
    Ok(())
}

/// The statement-level rules (`StatementRestrictions` constructor and `checkNeedsFiltering`).
fn process(
    table: &Table,
    kind: StmtKind,
    allow_filtering: bool,
    static_only: bool,
    for_view: bool,
    r: &mut Restrictions,
    cx: &mut PrepareCtx,
) -> Result<()> {
    let d = cx.dialect;
    let has_ck = r.ck.iter().any(|p| !p.is_empty()) || !r.multi.is_empty();
    // `hasQueriableIndex` / `hasQueriableClusteringColumnIndex`.
    let planner = if kind == StmtKind::Select && !for_view {
        cx.indexes.clone()
    } else {
        None
    };
    let supported = |ci: usize, p: &Pred| planner.as_ref().is_some_and(|x| x.supports(table, ci, p));
    let has_ck_index = r.ck.iter().enumerate().any(|(pos, ps)| {
        ps.iter()
            .any(|p| supported(table.clustering[pos], p))
    });
    let ann = planner.is_some() && cx.ann.is_some();
    let has_index = ann
        || has_ck_index
        || r.pk.iter().enumerate().any(|(pos, ps)| {
            ps.iter()
                .any(|p| supported(table.partition_key[pos], p))
        })
        || r.others
            .iter()
            .any(|(ci, ps)| ps.iter().any(|p| supported(*ci, p)));
    // ScyllaDB rejects clustering restrictions of static-only modifications first.
    if d.is_scylla() && static_only && has_ck && kind != StmtKind::Select {
        return Err(Error::invalid(format!(
            "Invalid restrictions on clustering columns since the {} statement modifies only static columns",
            kind.name()
        )));
    }
    let pk_all_eq =
        r.pk.iter()
            .all(|p| !p.is_empty() && p.iter().all(Pred::is_eq_or_in));
    let pk_any = r.pk.iter().any(|p| !p.is_empty());
    if kind != StmtKind::Select {
        if let Some(tp) = &r.token {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!(
                    "The token function cannot be used in WHERE clauses for UPDATE and DELETE statements: {}",
                    tp.text.join(" AND ")
                ),
                Dialect::Cassandra => format!(
                    "The token function cannot be used in WHERE clauses for {} statements",
                    kind.name()
                ),
            }));
        }
        let missing: Vec<&str> = table
            .partition_key
            .iter()
            .zip(&r.pk)
            .filter(|(_, p)| p.is_empty())
            .map(|(&i, _)| table.columns[i].name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(msg::missing_partition_key(d, &missing));
        }
        if !pk_all_eq {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "Only EQ and IN relation are supported on the partition key (unless you use the token() function)".to_string(),
                Dialect::Cassandra => format!("Only EQ and IN relation are supported on the partition key (unless you use the token() function) for {} statements", kind.name()),
            }));
        }
        r.key_mode = KeyMode::Keys;
    } else {
        if let Some(_t) = &r.token {
            if pk_any {
                if d.is_scylla() {
                    let cols: Vec<String> = table
                        .partition_key
                        .iter()
                        .zip(&r.pk)
                        .filter(|(_, p)| !p.is_empty())
                        .map(|(&i, _)| {
                            let c = &table.columns[i];
                            format!(
                                "ColumnDefinition{{name={}, type={}, kind=PARTITION_KEY, componentIndex={}, droppedAt=-9223372036854775808}}",
                                c.name,
                                nosql_types::marshal_name(&c.ty, false, d),
                                c.position
                            )
                        })
                        .collect();
                    return Err(Error::invalid(format!(
                        "Columns \"{}\" cannot be restricted by both a normal relation and a token relation",
                        cols.join(", ")
                    )));
                }
                r.key_mode = if pk_all_eq {
                    KeyMode::Keys
                } else {
                    KeyMode::TokenRange
                };
                r.pk_filtered = !pk_all_eq;
            } else {
                r.key_mode = KeyMode::TokenRange;
            }
        } else if pk_all_eq {
            r.key_mode = KeyMode::Keys;
        } else {
            r.key_mode = KeyMode::Scan;
            r.uses_index = has_index;
            if pk_any {
                if !allow_filtering && !for_view && !has_index {
                    if d.is_scylla() && r.pk.iter().flatten().any(Pred::is_slice) && pk_count_all(r)
                    {
                        return Err(Error::invalid(
                            "Only EQ and IN relation are supported on the partition key (unless you use the token() function or ALLOW FILTERING)",
                        ));
                    }
                    return Err(msg::allow_filtering());
                }
                r.pk_filtered = true;
            }
        }
        // Residual predicates on fully restricted keys (ScyllaDB intersections).
        if r.key_mode == KeyMode::Keys && r.pk.iter().any(|p| p.iter().any(|x| !x.is_eq_or_in())) {
            r.pk_filtered = true;
        }
    }
    if !d.is_scylla() && static_only && has_ck {
        if kind != StmtKind::Select {
            return Err(Error::invalid(format!(
                "Invalid restrictions on clustering columns since the {} statement modifies only static columns",
                kind.name()
            )));
        }
        return Err(Error::invalid(
            "Cannot restrict clustering columns when selecting only static columns",
        ));
    }
    r.index_filters_pk = r.uses_index || r.pk_filtered;
    // Clustering columns.
    analyze_clustering(table, r);
    match kind {
        StmtKind::Update => {
            if r.ck.iter().flatten().any(Pred::is_slice)
                || r.multi
                    .iter()
                    .any(|m| matches!(m.kind, MultiKind::Slice { .. }))
            {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => {
                        let rel = r.ck.iter().enumerate().find(|(_, p)| p.iter().any(Pred::is_slice)).map(|(i, _)| table.columns[table.clustering[i]].name.clone()).unwrap_or_default();
                        format!("Invalid operator in where clause ({rel} {})", slice_text(r))
                    }
                    Dialect::Cassandra => "Slice restrictions are not supported on the clustering columns in UPDATE statements".to_string(),
                }));
            }
            if !static_only && r.ck_prefix < table.clustering.len() {
                check_skipped(table, r, false)?;
                // COMPACT STORAGE: a dense table's row may be a clustering prefix.
                if !(crate::schema::compact::is_dense(table) && r.ck_prefix > 0) {
                    let missing: Vec<&str> = (r.ck_prefix..table.clustering.len())
                        .map(|i| table.columns[table.clustering[i]].name.as_str())
                        .collect();
                    return Err(msg::missing_clustering_key(d, &missing));
                }
            }
        }
        StmtKind::Delete => {
            if r.ck_filtered {
                check_skipped(table, r, false)?;
            }
        }
        StmtKind::Select => {
            if r.ck.iter().flatten().any(Pred::is_contains) && !allow_filtering && !has_index {
                return Err(Error::invalid(
                    "Clustering columns can only be restricted with CONTAINS with a secondary index or filtering",
                ));
            }
            if r.ck_filtered && has_ck {
                if has_index {
                    r.uses_index = true;
                } else if !allow_filtering && !for_view {
                    check_skipped(table, r, true)?;
                }
            }
            // Covers indexes on the first clustering column (among others).
            if r.key_mode != KeyMode::Keys && has_ck_index {
                r.uses_index = true;
            }
        }
    }
    r.index_filters_ck = r.uses_index || r.ck_filtered;
    // Non-key columns.
    if !r.others.is_empty() {
        if kind != StmtKind::Select {
            let names: Vec<&str> = r
                .others
                .iter()
                .map(|(i, _)| table.columns[*i].name.as_str())
                .collect();
            return Err(msg::non_pk_in_where(d, &names));
        }
        if has_index {
            r.uses_index = true;
        } else if !allow_filtering && !for_view {
            return Err(msg::allow_filtering());
        }
    }
    if ann {
        r.uses_index = true;
    }
    // `checkNeedsFiltering` with an index (raised by the SELECT after its ORDER BY checks):
    // some restriction no index group serves.
    // ANN queries validate their restrictions themselves (`crate::index::vector`).
    r.index_needs_filtering = r.uses_index
        && !ann
        && !allow_filtering
        && planner.as_ref().is_some_and(|p| {
            p.needs_filtering(table, r, r.index_filters_pk, r.index_filters_ck)
        });
    // `validateSecondaryIndexSelections` (ScyllaDB wants ALLOW FILTERING instead).
    if r.uses_index && r.pk.iter().flatten().any(|p| matches!(p, Pred::In(_))) {
        if !d.is_scylla() {
            return Err(Error::invalid(
                "Select on indexed columns and with IN clause for the PRIMARY KEY are not supported",
            ));
        }
        if !allow_filtering {
            r.index_needs_filtering = true;
        }
    }
    // `checkNeedsFiltering`: a key range with clustering restrictions filters.
    if kind == StmtKind::Select
        && !allow_filtering
        && !for_view
        && !r.uses_index
        && r.key_mode != KeyMode::Keys
        && has_ck
        && r.others.is_empty()
    {
        if d.is_scylla() {
            // Accepted, warned about or refused by `strict_allow_filtering` (the SELECT decides).
            r.implicit_filtering = true;
        } else {
            return Err(msg::allow_filtering());
        }
    }
    Ok(())
}

fn pk_count_all(r: &Restrictions) -> bool {
    r.pk.iter().all(|p| !p.is_empty())
}

fn slice_text(r: &Restrictions) -> String {
    for p in r.ck.iter().flatten() {
        if let Pred::Slice {
            lower,
            inclusive,
            value,
        } = p
        {
            let op = match (lower, inclusive) {
                (true, true) => ">=",
                (true, false) => ">",
                (false, true) => "<=",
                (false, false) => "<",
            };
            let v = match value {
                Term::Const(Some(_)) | Term::Const(None) => "?".to_string(),
                _ => "?".into(),
            };
            let _ = v;
            return format!("{op} {}", const_text(value));
        }
    }
    String::new()
}

fn const_text(t: &Term) -> String {
    match t {
        Term::Bind { .. } => "?".into(),
        Term::Const(Some(b)) if b.len() == 4 => {
            i32::from_be_bytes([b[0], b[1], b[2], b[3]]).to_string()
        }
        _ => "?".into(),
    }
}

/// "PRIMARY KEY column "d" cannot be restricted as preceding column "c" is not restricted".
fn check_skipped(table: &Table, r: &Restrictions, _select: bool) -> Result<()> {
    let mut restricted: Vec<usize> = Vec::new();
    for (i, p) in r.ck.iter().enumerate() {
        if !p.is_empty() {
            restricted.push(i);
        }
    }
    for m in &r.multi {
        restricted.extend(m.positions.iter().copied());
    }
    restricted.sort_unstable();
    restricted.dedup();
    for (i, &pos) in restricted.iter().enumerate() {
        if pos != i {
            return Err(Error::invalid(format!(
                "PRIMARY KEY column \"{}\" cannot be restricted as preceding column \"{}\" is not restricted",
                table.columns[table.clustering[pos]].name, table.columns[table.clustering[i]].name
            )));
        }
    }
    Ok(())
}

/// Decides the clustering prefix storage applies and what remains to filter.
fn analyze_clustering(table: &Table, r: &mut Restrictions) {
    let n = table.clustering.len();
    let mut pos = 0;
    let mut filtered = false;
    let mut consumed_multi = vec![false; r.multi.len()];
    loop {
        if pos >= n {
            break;
        }
        // A multi-column `=` / `IN` starting here extends the prefix.
        if let Some((mi, m)) = r.multi.iter().enumerate().find(|(_, m)| {
            m.positions[0] == pos && matches!(m.kind, MultiKind::Eq(_) | MultiKind::In(_))
        }) && m.positions.iter().all(|p| r.ck[*p].is_empty())
        {
            consumed_multi[mi] = true;
            pos += m.positions.len();
            continue;
        }
        let preds = &r.ck[pos];
        if !preds.is_empty()
            && preds.iter().all(Pred::is_eq_or_in)
            && !r.multi.iter().any(|m| m.positions.contains(&pos))
        {
            pos += 1;
            continue;
        }
        break;
    }
    r.ck_prefix = pos;
    // A slice right after the prefix.
    if pos < n {
        let preds = &r.ck[pos];
        let single_slice = !preds.is_empty() && preds.iter().all(Pred::is_slice);
        let multi_slice = r
            .multi
            .iter()
            .position(|m| m.positions[0] == pos && matches!(m.kind, MultiKind::Slice { .. }));
        if single_slice && multi_slice.is_none() {
            r.ck_slice = true;
        } else if let Some(mi) = multi_slice {
            r.ck_slice = true;
            // Bounds come from the first component; the tuple comparison is exact only when every
            // component sorts the same way, otherwise it is also filtered.
            let m = &r.multi[mi];
            let dirs: Vec<bool> = m
                .positions
                .iter()
                .map(|&p| table.columns[table.clustering[p]].reversed)
                .collect();
            let uniform = dirs.windows(2).all(|w| w[0] == w[1]);
            if uniform && preds.is_empty() {
                consumed_multi[mi] = true;
            }
        }
    }
    let first_free = if r.ck_slice {
        r.ck_prefix + 1
    } else {
        r.ck_prefix
    };
    for (i, p) in r.ck.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        if i >= first_free
            || p.iter().any(|x| !(x.is_eq_or_in() || x.is_slice()))
            || (i == r.ck_prefix && r.ck_slice && !p.iter().all(Pred::is_slice))
        {
            filtered = true;
        }
        // ScyllaDB intersections on prefix columns.
        if i < r.ck_prefix && p.len() > 1 {
            filtered = true;
        }
        if i == r.ck_prefix && r.ck_slice && r.multi.iter().any(|m| m.positions[0] == i) {
            filtered = true;
        }
    }
    if consumed_multi.iter().any(|c| !c) {
        filtered = true;
    }
    r.ck_filtered = filtered;
}

// ---- execution -------------------------------------------------------------------------------

/// Where to look for partitions.
pub enum Partitions {
    /// Partition key component values, one entry per key.
    Keys(Vec<Vec<Bytes>>),
    Range {
        start: KeyBound,
        end: KeyBound,
    },
}

fn eval_value(
    t: &Term,
    cx: &EvalCtx,
    column: &str,
    d: Dialect,
    null_ok: bool,
) -> Result<Option<Bytes>> {
    match t.eval(cx)? {
        Bound::Val(b) => Ok(Some(b)),
        Bound::Null => {
            if null_ok {
                Ok(None)
            } else {
                Err(Error::invalid(format!(
                    "Invalid null value in condition for column {column}"
                )))
            }
        }
        Bound::Unset => Err(unset_error(t, column, d)),
    }
}

fn unset_error(t: &Term, column: &str, d: Dialect) -> Error {
    match (d, t) {
        (Dialect::Scylla, Term::Bind { index, .. }) => {
            Error::invalid(format!("Unexpected unset value for bind variable {index}"))
        }
        _ => msg::unset_routing_key(column),
    }
}

/// The values of an IN (deduplicated later by the caller's comparator). Nulls are reported by
/// Cassandra and skipped by ScyllaDB.
fn eval_in(
    v: &InValues,
    cx: &EvalCtx,
    column: &str,
    elem: &CqlType,
    d: Dialect,
) -> Result<Vec<Bytes>> {
    eval_in_nulls(v, cx, column, elem, d).map(|(v, _)| v)
}

/// The values of an `IN` list, and whether ScyllaDB dropped a null from it (a `NOT IN` with a
/// null matches nothing there, as SQL's does).
fn eval_in_nulls(
    v: &InValues,
    cx: &EvalCtx,
    column: &str,
    elem: &CqlType,
    d: Dialect,
) -> Result<(Vec<Bytes>, bool)> {
    let mut null = false;
    match v {
        InValues::Terms(ts) => {
            let mut out = Vec::with_capacity(ts.len());
            for t in ts {
                match t.eval(cx)? {
                    Bound::Val(b) => out.push(b),
                    Bound::Null => {
                        if !d.is_scylla() {
                            return Err(Error::invalid(format!(
                                "Invalid null value in condition for column {column}"
                            )));
                        }
                        null = true;
                    }
                    Bound::Unset => return Err(unset_error(t, column, d)),
                }
            }
            Ok((out, null))
        }
        InValues::Marker(t) => {
            // ScyllaDB takes null elements in a bound IN list (validated one by one below).
            let bound = match (t, cx.params.get(bind_index(t))) {
                (Term::Bind { .. }, Some(crate::response::Param::Set(b))) if d.is_scylla() => {
                    Bound::Val(b.clone())
                }
                _ => t.eval(cx)?,
            };
            match bound {
                Bound::Val(b) => {
                    let elems = nosql_types::collection_elements(&b)
                        .map_err(|e| Error::invalid(e.message))?;
                    let mut out = Vec::with_capacity(elems.len());
                    for e in elems {
                        match e {
                            Some(x) => {
                                terms::validate_bound(elem, x, column, d)?;
                                out.push(Bytes::copy_from_slice(x));
                            }
                            None => {
                                if !d.is_scylla() {
                                    return Err(Error::invalid(format!(
                                        "Invalid null value in condition for column {column}"
                                    )));
                                }
                                null = true;
                            }
                        }
                    }
                    Ok((out, null))
                }
                Bound::Null => {
                    if d.is_scylla() {
                        Ok((Vec::new(), true))
                    } else {
                        // Multi-column relations pass no column name.
                        Err(Error::invalid(if column.is_empty() {
                            "Invalid null value for IN restriction".to_string()
                        } else {
                            format!("Invalid null value for column {column}")
                        }))
                    }
                }
                Bound::Unset => Err(unset_error(t, column, d)),
            }
        }
    }
}

/// The bind index of a marker term (`usize::MAX` for other terms).
fn bind_index(t: &Term) -> usize {
    match t {
        Term::Bind { index, .. } => *index,
        _ => usize::MAX,
    }
}

/// The values a column restricted by `=` / `IN` (possibly several, intersected) may take.
fn column_values(
    preds: &[Pred],
    cx: &EvalCtx,
    column: &str,
    ty: &CqlType,
    d: Dialect,
    null_ok: bool,
    write: bool,
) -> Result<Option<Vec<Bytes>>> {
    let mut acc: Option<Vec<Bytes>> = None;
    for p in preds {
        let vals = match p {
            Pred::Eq(t) => match eval_value(t, cx, column, d, null_ok)? {
                Some(v) => vec![v],
                None => Vec::new(),
            },
            // A ScyllaDB write takes `IN (x)` as `= x`, and refuses a null `IN ?` list.
            Pred::In(InValues::Terms(ts)) if write && d.is_scylla() && ts.len() == 1 => {
                match eval_value(&ts[0], cx, column, d, false)? {
                    Some(v) => vec![v],
                    None => Vec::new(),
                }
            }
            Pred::In(InValues::Marker(t))
                if write && d.is_scylla() && matches!(t.eval(cx)?, Bound::Null) =>
            {
                return Err(Error::invalid(format!(
                    "Invalid null value in condition for column {column}"
                )));
            }
            Pred::In(v) => eval_in(v, cx, column, ty, d)?,
            _ => continue,
        };
        acc = Some(match acc {
            None => vals,
            Some(prev) => prev
                .into_iter()
                .filter(|a| vals.iter().any(|b| nosql_types::compare(ty, a, b).is_eq()))
                .collect(),
        });
    }
    if let Some(v) = &mut acc {
        v.sort_by(|a, b| nosql_types::compare(ty, a, b));
        v.dedup_by(|a, b| nosql_types::compare(ty, a, b).is_eq());
    }
    Ok(acc)
}

impl Restrictions {
    /// Cassandra can't filter by a multi-column slice: one that doesn't start where the
    /// clustering restrictions before it end (`ClusteringColumnRestrictions.addToRowFilter`,
    /// `MultiColumnRestriction.SliceRestriction.addToRowFilter`).
    fn check_multi_slice_filtering(&self) -> Result<()> {
        if !self
            .multi
            .iter()
            .any(|m| matches!(m.kind, MultiKind::Slice { .. }))
        {
            return Ok(());
        }
        // (first column, last column, slice, multi-column)
        let mut all: Vec<(usize, usize, bool, bool)> = Vec::new();
        for (i, p) in self.ck.iter().enumerate() {
            if !p.is_empty() {
                all.push((i, i, p.iter().all(Pred::is_slice), false));
            }
        }
        for m in &self.multi {
            let first = m.positions.first().copied().unwrap_or(0);
            let last = m.positions.last().copied().unwrap_or(0);
            all.push((first, last, matches!(m.kind, MultiKind::Slice { .. }), true));
        }
        all.sort_by_key(|e| e.0);
        let mut position = 0;
        for (first, last, slice, multi) in all {
            if first != position {
                if multi && slice {
                    return Err(Error::invalid(
                        "Multi-column slice restrictions cannot be used for filtering.",
                    ));
                }
                continue;
            }
            if !slice {
                position = last + 1;
            }
        }
        Ok(())
    }

    /// The partitions to read or write.
    pub fn partitions(
        &self,
        table: &Table,
        cx: &EvalCtx,
        max_keys: Option<u64>,
    ) -> Result<Partitions> {
        let d = cx.dialect;
        match self.key_mode {
            KeyMode::Keys => {
                let mut combos: Vec<Vec<Bytes>> = vec![Vec::new()];
                // ScyllaDB returns nothing for a null key in a SELECT; Cassandra fails.
                let null_ok = d.is_scylla() && self.kind == StmtKind::Select;
                for (pos, preds) in self.pk.iter().enumerate() {
                    let col = &table.columns[table.partition_key[pos]];
                    let write = self.kind != StmtKind::Select;
                    let vals = column_values(preds, cx, &col.name, &col.ty, d, null_ok, write)?
                        .unwrap_or_default();
                    let mut next = Vec::with_capacity(combos.len() * vals.len());
                    for c in &combos {
                        for v in &vals {
                            let mut k = c.clone();
                            k.push(v.clone());
                            next.push(k);
                        }
                    }
                    combos = next;
                    if let Some(max) = max_keys
                        && combos.len() as u64 > max
                    {
                        let total = combos.len();
                        return Err(Error::server(format!(
                            "size of partition-key IN list or partition-key cartesian product of IN list {total} is greater than maximum {max}. Use --max-partition-key-restrictions-per-query configuration option to change default setting."
                        )));
                    }
                }
                Ok(Partitions::Keys(combos))
            }
            KeyMode::TokenRange => {
                let t = self.token.as_ref().cloned().unwrap_or_default();
                let tok = |term: &Term| -> Result<i64> {
                    match term.eval(cx)? {
                        Bound::Val(b) => <[u8; 8]>::try_from(&b[..])
                            .map(i64::from_be_bytes)
                            .map_err(|_| Error::invalid("Invalid token value")),
                        _ => Err(Error::invalid("Invalid null token value")),
                    }
                };
                if let Some(e) = &t.eq {
                    let v = tok(e)?;
                    return Ok(Partitions::Range {
                        start: KeyBound::Token {
                            token: v,
                            after: false,
                        },
                        end: KeyBound::Token {
                            token: v,
                            after: true,
                        },
                    });
                }
                let start = match &t.lo {
                    Some((term, incl)) => {
                        let v = tok(term)?;
                        if v == i64::MIN && !incl {
                            KeyBound::Unbounded
                        } else {
                            KeyBound::Token {
                                token: v,
                                after: !incl,
                            }
                        }
                    }
                    None => KeyBound::Unbounded,
                };
                let end = match &t.hi {
                    Some((term, incl)) => {
                        let v = tok(term)?;
                        // `token(pk) <= MIN` means the whole ring end in Cassandra (min token is
                        // the wrap point); treat it as unbounded.
                        if v == i64::MIN {
                            KeyBound::Unbounded
                        } else {
                            KeyBound::Token {
                                token: v,
                                after: *incl,
                            }
                        }
                    }
                    None => KeyBound::Unbounded,
                };
                // An empty range (start after end) reads nothing.
                if let (
                    KeyBound::Token {
                        token: s,
                        after: sa,
                    },
                    KeyBound::Token {
                        token: e,
                        after: ea,
                    },
                ) = (&start, &end)
                    && (s > e || (s == e && (*sa || !*ea)))
                {
                    return Ok(Partitions::Keys(Vec::new()));
                }
                Ok(Partitions::Range { start, end })
            }
            KeyMode::Scan => Ok(Partitions::Range {
                start: KeyBound::Unbounded,
                end: KeyBound::Unbounded,
            }),
        }
    }

    /// Exact clusterings (all clustering columns restricted by `=` / `IN`), sorted and unique.
    pub fn clusterings(
        &self,
        table: &Table,
        cx: &EvalCtx,
        max: Option<u64>,
    ) -> Result<Vec<Clustering>> {
        let prefixes = self.prefixes(table, cx, max)?;
        Ok(prefixes
            .into_iter()
            .map(|p| Clustering::new(p.iter()))
            .collect())
    }

    /// Value combinations of the clustering prefix storage applies.
    fn prefixes(&self, table: &Table, cx: &EvalCtx, max: Option<u64>) -> Result<Vec<Vec<Bytes>>> {
        let d = cx.dialect;
        let mut combos: Vec<Vec<Bytes>> = vec![Vec::new()];
        let mut pos = 0;
        while pos < self.ck_prefix {
            if let Some(m) = self.multi.iter().find(|m| {
                m.positions[0] == pos && matches!(m.kind, MultiKind::Eq(_) | MultiKind::In(_))
            }) {
                let names: Vec<String> = m
                    .positions
                    .iter()
                    .map(|&p| table.columns[table.clustering[p]].name.clone())
                    .collect();
                let tuples: Vec<Bytes> = match &m.kind {
                    MultiKind::Eq(t) => {
                        vec![eval_value(t, cx, &names.join(", "), d, false)?.unwrap_or_default()]
                    }
                    MultiKind::In(v) => {
                        if let InValues::Marker(t) = v
                            && !d.is_scylla()
                            && matches!(t.eval(cx)?, Bound::Null)
                        {
                            return Err(Error::invalid("Invalid null value for IN restriction"));
                        }
                        let tty = CqlType::Tuple(
                            m.positions
                                .iter()
                                .map(|&p| table.columns[table.clustering[p]].ty.clone())
                                .collect(),
                        );
                        eval_in(v, cx, &names.join(", "), &tty, d)?
                    }
                    MultiKind::Slice { .. } => unreachable!(),
                };
                let mut parts_list = Vec::new();
                for t in &tuples {
                    let fields =
                        nosql_types::tuple_fields(t).map_err(|e| Error::invalid(e.message))?;
                    let mut parts = Vec::with_capacity(m.positions.len());
                    for (i, f) in fields.iter().enumerate().take(m.positions.len()) {
                        match f {
                            Some(b) => parts.push(Bytes::copy_from_slice(b)),
                            None => {
                                return Err(Error::invalid(format!(
                                    "Invalid null value in condition for column {}",
                                    names[i]
                                )));
                            }
                        }
                    }
                    if parts.len() != m.positions.len() {
                        return Err(Error::invalid(format!(
                            "Invalid null value in condition for column {}",
                            names[parts.len()]
                        )));
                    }
                    parts_list.push(parts);
                }
                let mut next = Vec::new();
                for c in &combos {
                    for p in &parts_list {
                        let mut k = c.clone();
                        k.extend(p.iter().cloned());
                        next.push(k);
                    }
                }
                combos = next;
                pos += m.positions.len();
            } else {
                let col = &table.columns[table.clustering[pos]];
                let write = self.kind != StmtKind::Select;
                let vals = column_values(&self.ck[pos], cx, &col.name, &col.ty, d, false, write)?
                    .unwrap_or_default();
                let mut next = Vec::with_capacity(combos.len() * vals.len());
                for c in &combos {
                    for v in &vals {
                        let mut k = c.clone();
                        k.push(v.clone());
                        next.push(k);
                    }
                }
                combos = next;
                pos += 1;
            }
            if let Some(max) = max
                && combos.len() as u64 > max
            {
                return Err(Error::server(format!(
                    "size of clustering-key IN list or clustering-key cartesian product of IN list {} is greater than maximum {max}. Use --max-clustering-key-restrictions-per-query configuration option to change default setting.",
                    combos.len()
                )));
            }
        }
        // Sort and dedup by the table's clustering order.
        let cmp = table.comparator.clone();
        let mut cl: Vec<(Clustering, Vec<Bytes>)> = combos
            .into_iter()
            .map(|c| (Clustering::new(c.iter()), c))
            .collect();
        cl.sort_by(|a, b| nosql_storage::compare::compare_clustering(&*cmp, &a.0, &b.0));
        cl.dedup_by(|a, b| nosql_storage::compare::compare_clustering(&*cmp, &a.0, &b.0).is_eq());
        Ok(cl.into_iter().map(|(_, c)| c).collect())
    }

    /// The clustering filter storage applies (a superset of the matching rows; the residual is
    /// [`Self::filter`]).
    pub fn clustering(
        &self,
        table: &Table,
        cx: &EvalCtx,
        max: Option<u64>,
    ) -> Result<ClusteringFilter> {
        if self.ck_prefix == 0 && !self.ck_slice {
            return Ok(ClusteringFilter::all());
        }
        let prefixes = self.prefixes(table, cx, max)?;
        if self.ck_prefix == table.clustering.len() {
            return Ok(ClusteringFilter::Names(
                prefixes
                    .into_iter()
                    .map(|p| Clustering::new(p.iter()))
                    .collect(),
            ));
        }
        if !self.ck_slice {
            return Ok(ClusteringFilter::Slices(
                prefixes
                    .into_iter()
                    .map(|p| Slice::prefix(Clustering::new(p.iter())))
                    .collect(),
            ));
        }
        let pos = self.ck_prefix;
        let col = &table.columns[table.clustering[pos]];
        let d = cx.dialect;
        // Bounds on the slice column: (value, inclusive) for the lower and upper side.
        let mut lo: Option<(Bytes, bool)> = None;
        let mut hi: Option<(Bytes, bool)> = None;
        let mut tighten = |lower: bool, v: Bytes, incl: bool| {
            let slot = if lower { &mut lo } else { &mut hi };
            let replace = match slot {
                None => true,
                Some((cur, cincl)) => {
                    let o = nosql_types::compare(&col.ty, &v, cur);
                    if lower {
                        o.is_gt() || (o.is_eq() && !incl && *cincl)
                    } else {
                        o.is_lt() || (o.is_eq() && !incl && *cincl)
                    }
                }
            };
            if replace {
                *slot = Some((v, incl));
            }
        };
        let mut empty = false;
        for p in &self.ck[pos] {
            if let Pred::Slice {
                lower,
                inclusive,
                value,
            } = p
            {
                match eval_value(value, cx, &col.name, d, true)? {
                    Some(v) => tighten(*lower, v, *inclusive),
                    None => empty = true,
                }
            }
        }
        if let Some(m) = self
            .multi
            .iter()
            .find(|m| m.positions[0] == pos && matches!(m.kind, MultiKind::Slice { .. }))
        {
            let MultiKind::Slice { lo: mlo, hi: mhi } = &m.kind else {
                unreachable!()
            };
            let first = |t: &Term| -> Result<Option<Bytes>> {
                let Bound::Val(b) = t.eval(cx)? else {
                    return Err(Error::invalid(format!(
                        "Invalid null value in condition for column {}",
                        col.name
                    )));
                };
                let fields =
                    nosql_types::tuple_fields(&b).map_err(|e| Error::invalid(e.message))?;
                Ok(fields
                    .first()
                    .copied()
                    .flatten()
                    .map(Bytes::copy_from_slice))
            };
            // The first component bounds the tuple inclusively (equal first components may still
            // match on later components); the exact comparison is a row filter when needed.
            let exact = m.positions.len() == 1;
            if let Some((t, incl)) = mlo
                && let Some(v) = first(t)?
            {
                tighten(true, v, if exact { *incl } else { true });
            }
            if let Some((t, incl)) = mhi
                && let Some(v) = first(t)?
            {
                tighten(false, v, if exact { *incl } else { true });
            }
            if !exact && self.multi_exact_by_bounds(table, m) {
                // Same direction for every component: whole-tuple bounds are exact.
                return self.tuple_slices(table, &prefixes, m, cx);
            }
        }
        if empty {
            return Ok(ClusteringFilter::Slices(Vec::new()));
        }
        let mut slices = Vec::with_capacity(prefixes.len());
        for p in prefixes {
            let bound_at = |v: &Bytes| -> Clustering {
                let mut parts = p.clone();
                parts.push(v.clone());
                Clustering::new(parts.iter())
            };
            let base = Clustering::new(p.iter());
            // In storage order a reversed column swaps the sides.
            let (logical_lo, logical_hi) = (lo.clone(), hi.clone());
            let (first, last) = if col.reversed {
                (logical_hi, logical_lo)
            } else {
                (logical_lo, logical_hi)
            };
            let start = match first {
                Some((v, incl)) => {
                    if incl {
                        SBound::inclusive_start(bound_at(&v))
                    } else {
                        SBound::exclusive_start(bound_at(&v))
                    }
                }
                None => SBound::inclusive_start(base.clone()),
            };
            let end = match last {
                Some((v, incl)) => {
                    if incl {
                        SBound::inclusive_end(bound_at(&v))
                    } else {
                        SBound::exclusive_end(bound_at(&v))
                    }
                }
                None => SBound::inclusive_end(base.clone()),
            };
            slices.push(Slice::new(start, end));
        }
        Ok(ClusteringFilter::Slices(slices))
    }

    fn multi_exact_by_bounds(&self, table: &Table, m: &Multi) -> bool {
        let dirs: Vec<bool> = m
            .positions
            .iter()
            .map(|&p| table.columns[table.clustering[p]].reversed)
            .collect();
        dirs.windows(2).all(|w| w[0] == w[1]) && self.ck[m.positions[0]].is_empty()
    }

    /// Whole-tuple slices for a multi-column slice whose columns share a direction.
    fn tuple_slices(
        &self,
        table: &Table,
        prefixes: &[Vec<Bytes>],
        m: &Multi,
        cx: &EvalCtx,
    ) -> Result<ClusteringFilter> {
        let MultiKind::Slice { lo, hi } = &m.kind else {
            unreachable!()
        };
        let reversed = table.columns[table.clustering[m.positions[0]]].reversed;
        let tuple = |t: &Term| -> Result<Vec<Bytes>> {
            let Bound::Val(b) = t.eval(cx)? else {
                return Err(Error::invalid(
                    "Invalid null value in condition for columns",
                ));
            };
            let fields = nosql_types::tuple_fields(&b).map_err(|e| Error::invalid(e.message))?;
            let mut out = Vec::new();
            for f in fields {
                match f {
                    Some(x) => out.push(Bytes::copy_from_slice(x)),
                    None => {
                        return Err(Error::invalid(
                            "Invalid null value in condition for columns",
                        ));
                    }
                }
            }
            Ok(out)
        };
        let lo = match lo {
            Some((t, i)) => Some((tuple(t)?, *i)),
            None => None,
        };
        let hi = match hi {
            Some((t, i)) => Some((tuple(t)?, *i)),
            None => None,
        };
        let mut slices = Vec::new();
        for p in prefixes {
            let at = |v: &[Bytes]| {
                let mut parts = p.clone();
                parts.extend(v.iter().cloned());
                Clustering::new(parts.iter())
            };
            let base = Clustering::new(p.iter());
            let (first, last) = if reversed {
                (hi.clone(), lo.clone())
            } else {
                (lo.clone(), hi.clone())
            };
            let start = match first {
                Some((v, true)) => SBound::inclusive_start(at(&v)),
                Some((v, false)) => SBound::exclusive_start(at(&v)),
                None => SBound::inclusive_start(base.clone()),
            };
            let end = match last {
                Some((v, true)) => SBound::inclusive_end(at(&v)),
                Some((v, false)) => SBound::exclusive_end(at(&v)),
                None => SBound::inclusive_end(base.clone()),
            };
            slices.push(Slice::new(start, end));
        }
        Ok(ClusteringFilter::Slices(slices))
    }

    /// Every restriction as a row filter (a materialized view's `WHERE` clause, which applies to
    /// each base row: `View.matchesViewFilter`), token restrictions included.
    pub fn row_filter(&self, table: &Table, cx: &EvalCtx) -> Result<RowFilter> {
        let mut all = self.clone();
        all.pk_filtered = true;
        all.ck_filtered = true;
        // `filter` adds token predicates for key lookups.
        all.key_mode = KeyMode::Keys;
        all.filter(table, cx)
    }

    /// The bound residual filter.
    pub fn filter(&self, table: &Table, cx: &EvalCtx) -> Result<RowFilter> {
        let d = cx.dialect;
        if !d.is_scylla() {
            self.check_multi_slice_filtering()?;
        }
        let mut f = RowFilter::default();
        if self.pk_filtered {
            for (pos, preds) in self.pk.iter().enumerate() {
                let ci = table.partition_key[pos];
                for p in preds {
                    f.preds.push((
                        Target::Column(ci),
                        bind_pred(p, &table.columns[ci].ty, &table.columns[ci].name, cx, d)?,
                    ));
                }
            }
        }
        if self.key_mode == KeyMode::Keys
            && let Some(t) = &self.token
        {
            let tok = |term: &Term| -> Result<Option<Bytes>> { Ok(term.eval(cx)?.into_option()) };
            let mut add = |lower: bool, inclusive: bool, v: Option<Bytes>| {
                if let Some(v) = v {
                    f.preds.push((
                        Target::Token,
                        BoundPred::Slice {
                            lower,
                            inclusive,
                            value: v,
                        },
                    ));
                }
            };
            if let Some(e) = &t.eq {
                let v = tok(e)?;
                add(true, true, v.clone());
                add(false, true, v);
            }
            if let Some((x, i)) = &t.lo {
                add(true, *i, tok(x)?);
            }
            if let Some((x, i)) = &t.hi {
                add(false, *i, tok(x)?);
            }
        }
        if self.ck_filtered {
            for (pos, preds) in self.ck.iter().enumerate() {
                let ci = table.clustering[pos];
                for p in preds {
                    f.preds.push((
                        Target::Column(ci),
                        bind_pred(p, &table.columns[ci].ty, &table.columns[ci].name, cx, d)?,
                    ));
                }
            }
            for m in &self.multi {
                let cols: Vec<usize> = m.positions.iter().map(|&p| table.clustering[p]).collect();
                let types: Vec<CqlType> =
                    cols.iter().map(|&c| table.columns[c].ty.clone()).collect();
                let tuple = |t: &Term| -> Result<Vec<Option<Bytes>>> {
                    let Bound::Val(b) = t.eval(cx)? else {
                        return Err(Error::invalid(
                            "Invalid null value in condition for columns",
                        ));
                    };
                    let fields =
                        nosql_types::tuple_fields(&b).map_err(|e| Error::invalid(e.message))?;
                    Ok(fields
                        .into_iter()
                        .map(|x| x.map(Bytes::copy_from_slice))
                        .collect())
                };
                let bp = match &m.kind {
                    MultiKind::Eq(t) => MultiPred::In(vec![tuple(t)?]),
                    MultiKind::In(v) => {
                        let tty = CqlType::Tuple(types.clone());
                        let vals = eval_in(v, cx, "", &tty, d)?;
                        let mut out = Vec::new();
                        for b in vals {
                            let fields = nosql_types::tuple_fields(&b)
                                .map_err(|e| Error::invalid(e.message))?;
                            out.push(
                                fields
                                    .into_iter()
                                    .map(|x| x.map(Bytes::copy_from_slice))
                                    .collect(),
                            );
                        }
                        MultiPred::In(out)
                    }
                    MultiKind::Slice { lo, hi } => MultiPred::Slice {
                        lo: match lo {
                            Some((t, i)) => Some((tuple(t)?, *i)),
                            None => None,
                        },
                        hi: match hi {
                            Some((t, i)) => Some((tuple(t)?, *i)),
                            None => None,
                        },
                    },
                };
                f.multi.push((cols, types, bp));
            }
        }
        for (ci, preds) in &self.others {
            for p in preds {
                f.preds.push((
                    Target::Column(*ci),
                    bind_pred(p, &table.columns[*ci].ty, &table.columns[*ci].name, cx, d)?,
                ));
            }
        }
        Ok(f)
    }
}

fn bind_pred(p: &Pred, ty: &CqlType, column: &str, cx: &EvalCtx, d: Dialect) -> Result<BoundPred> {
    Ok(match p {
        Pred::Eq(t) => match eval_value(t, cx, column, d, true)? {
            Some(v) => BoundPred::In(vec![v]),
            None => BoundPred::Never,
        },
        Pred::In(v) => BoundPred::In(eval_in(v, cx, column, ty, d)?),
        Pred::NotIn(v) => match eval_in_nulls(v, cx, column, ty, d)? {
            (_, true) => BoundPred::Never,
            (vals, false) => BoundPred::NotIn(vals),
        },
        Pred::Slice {
            lower,
            inclusive,
            value,
        } => match eval_value(value, cx, column, d, true)? {
            Some(v) => BoundPred::Slice {
                lower: *lower,
                inclusive: *inclusive,
                value: v,
            },
            None => BoundPred::Never,
        },
        // A null element, key or value: ScyllaDB matches nothing, Cassandra refuses it.
        Pred::Contains(t) | Pred::ContainsKey(t) => match eval_value(t, cx, column, d, true)? {
            Some(v) if matches!(p, Pred::Contains(_)) => BoundPred::Contains(v),
            Some(v) => BoundPred::ContainsKey(v),
            None if d.is_scylla() => BoundPred::Never,
            None => {
                return Err(Error::invalid(format!(
                    "Unsupported null value for column {column}"
                )));
            }
        },
        Pred::Entry(k, v) => {
            let key = eval_value(k, cx, column, d, true)?;
            let val = eval_value(v, cx, column, d, true)?;
            match (key, val) {
                (Some(key), Some(val)) => BoundPred::Entry(key, val),
                _ if d.is_scylla() => BoundPred::Never,
                (None, _) => {
                    return Err(Error::invalid(format!(
                        "Unsupported null map key for column {column}"
                    )));
                }
                (_, None) => {
                    return Err(Error::invalid(format!(
                        "Unsupported null map value for column {column}"
                    )));
                }
            }
        }
        Pred::Like(t) => match eval_value(t, cx, column, d, true)? {
            Some(v) => BoundPred::Like(String::from_utf8_lossy(&v).into_owned()),
            None => BoundPred::Never,
        },
    })
}

/// What a row predicate looks at.
#[derive(Clone, Debug)]
pub enum Target {
    Column(usize),
    /// The partition token (bigint).
    Token,
}

#[derive(Clone, Debug)]
pub enum BoundPred {
    In(Vec<Bytes>),
    NotIn(Vec<Bytes>),
    Slice {
        lower: bool,
        inclusive: bool,
        value: Bytes,
    },
    Contains(Bytes),
    ContainsKey(Bytes),
    Entry(Bytes, Bytes),
    Like(String),
    /// A comparison with null: never true.
    Never,
    /// A predicate an index evaluates itself (SASI's analyzed matching), set by
    /// [`crate::hooks::IndexScan::adjust_filter`].
    Matcher(std::sync::Arc<dyn ValueMatcher>),
}

/// A predicate on one non-null column value that an index defines.
pub trait ValueMatcher: Send + Sync + std::fmt::Debug {
    fn matches(&self, value: &[u8]) -> bool;
}

#[derive(Clone, Debug)]
pub enum MultiPred {
    In(Vec<Vec<Option<Bytes>>>),
    Slice {
        lo: Option<(Vec<Option<Bytes>>, bool)>,
        hi: Option<(Vec<Option<Bytes>>, bool)>,
    },
}

/// Row-level predicates.
#[derive(Clone, Debug, Default)]
pub struct RowFilter {
    pub preds: Vec<(Target, BoundPred)>,
    pub multi: Vec<(Vec<usize>, Vec<CqlType>, MultiPred)>,
}

impl RowFilter {
    pub fn is_empty(&self) -> bool {
        self.preds.is_empty() && self.multi.is_empty()
    }

    /// Whether a row matches. `value(i)` returns column `i`'s serialised value (`None` = null).
    pub fn matches(
        &self,
        table: &Table,
        token: i64,
        value: &dyn Fn(usize) -> Option<Bytes>,
    ) -> bool {
        for (target, p) in &self.preds {
            let (ty, v) = match target {
                Target::Token => (
                    CqlType::Bigint,
                    Some(Bytes::copy_from_slice(&token.to_be_bytes())),
                ),
                Target::Column(i) => (table.columns[*i].ty.clone(), value(*i)),
            };
            if !eval_pred(p, &ty, v.as_deref()) {
                return false;
            }
        }
        for (cols, types, mp) in &self.multi {
            let vals: Vec<Option<Bytes>> = cols.iter().map(|&c| value(c)).collect();
            if !eval_multi(mp, types, &vals) {
                return false;
            }
        }
        true
    }
}

fn eval_pred(p: &BoundPred, ty: &CqlType, v: Option<&[u8]>) -> bool {
    let cmp_ty = if ty.is_counter() {
        &CqlType::Bigint
    } else {
        ty
    };
    match p {
        BoundPred::Never => false,
        BoundPred::NotIn(vals) => match v {
            None => false,
            Some(x) => !vals
                .iter()
                .any(|y| nosql_types::compare(cmp_ty, x, y).is_eq()),
        },
        _ => {
            let Some(x) = v else { return false };
            match p {
                BoundPred::In(vals) => vals
                    .iter()
                    .any(|y| nosql_types::compare(cmp_ty, x, y).is_eq()),
                BoundPred::Slice {
                    lower,
                    inclusive,
                    value,
                } => {
                    let o = nosql_types::compare(cmp_ty, x, value);
                    if *lower {
                        o.is_gt() || (*inclusive && o.is_eq())
                    } else {
                        o.is_lt() || (*inclusive && o.is_eq())
                    }
                }
                BoundPred::Contains(e) => match ty {
                    CqlType::List { elem, .. } | CqlType::Set { elem, .. } => {
                        nosql_types::collection_elements(x)
                            .map(|es| {
                                es.into_iter()
                                    .flatten()
                                    .any(|y| nosql_types::compare(elem, y, e).is_eq())
                            })
                            .unwrap_or(false)
                    }
                    CqlType::Map { value, .. } => nosql_types::map_entries(x)
                        .map(|es| {
                            es.into_iter()
                                .any(|(_, y)| nosql_types::compare(value, y, e).is_eq())
                        })
                        .unwrap_or(false),
                    _ => false,
                },
                BoundPred::ContainsKey(k) => match ty {
                    CqlType::Map { key, .. } => nosql_types::map_entries(x)
                        .map(|es| {
                            es.into_iter()
                                .any(|(y, _)| nosql_types::compare(key, y, k).is_eq())
                        })
                        .unwrap_or(false),
                    _ => false,
                },
                BoundPred::Entry(k, val) => match ty {
                    // ScyllaDB's `l[i] = v`: the element at position `i`.
                    CqlType::List { elem, .. } => <[u8; 4]>::try_from(&k[..])
                        .ok()
                        .map(i32::from_be_bytes)
                        .and_then(|i| usize::try_from(i).ok())
                        .and_then(|i| {
                            nosql_types::collection_elements(x)
                                .ok()?
                                .get(i)
                                .copied()
                                .flatten()
                        })
                        .is_some_and(|y| nosql_types::compare(elem, y, val).is_eq()),
                    CqlType::Map { key, value, .. } => nosql_types::map_entries(x)
                        .map(|es| {
                            es.into_iter().any(|(y, z)| {
                                nosql_types::compare(key, y, k).is_eq()
                                    && nosql_types::compare(value, z, val).is_eq()
                            })
                        })
                        .unwrap_or(false),
                    _ => false,
                },
                BoundPred::Like(pattern) => like(pattern, &String::from_utf8_lossy(x)),
                BoundPred::Matcher(m) => m.matches(x),
                BoundPred::Never | BoundPred::NotIn(_) => false,
            }
        }
    }
}

/// Tuple comparison with nulls first (`TupleType` order).
fn tuple_cmp(types: &[CqlType], a: &[Option<Bytes>], b: &[Option<Bytes>]) -> std::cmp::Ordering {
    for (i, t) in types.iter().enumerate() {
        let (x, y) = (a.get(i).cloned().flatten(), b.get(i).cloned().flatten());
        let o = match (x, y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => nosql_types::compare(t, &x, &y),
        };
        if o.is_ne() {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

fn eval_multi(p: &MultiPred, types: &[CqlType], vals: &[Option<Bytes>]) -> bool {
    match p {
        MultiPred::In(options) => options.iter().any(|o| tuple_cmp(types, vals, o).is_eq()),
        MultiPred::Slice { lo, hi } => {
            if let Some((b, incl)) = lo {
                let n = b.len().min(types.len());
                let o = tuple_cmp(&types[..n], &vals[..n.min(vals.len())], b);
                if !(o.is_gt() || (*incl && o.is_eq())) {
                    return false;
                }
            }
            if let Some((b, incl)) = hi {
                let n = b.len().min(types.len());
                let o = tuple_cmp(&types[..n], &vals[..n.min(vals.len())], b);
                if !(o.is_lt() || (*incl && o.is_eq())) {
                    return false;
                }
            }
            true
        }
    }
}

/// SQL `LIKE` with `%` (any run) and `_` (one character).
pub fn like(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '%' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like("a%", "abc"));
        assert!(like("%c", "abc"));
        assert!(like("a_c", "abc"));
        assert!(!like("a_c", "abbc"));
        assert!(like("%", ""));
        assert!(!like("a%", "ba"));
        assert!(like("%b%", "abc"));
    }
}
