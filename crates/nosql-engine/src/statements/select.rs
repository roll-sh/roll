//! SELECT: preparation (`SelectStatement.RawStatement.prepare`) and execution — partition and
//! slice reads, token-range scans, filtering, DISTINCT, GROUP BY and aggregates, LIMIT / PER
//! PARTITION LIMIT, ORDER BY (with the post-query ordering of IN queries), SELECT JSON, paging.
//!
//! Reads go through storage for stored tables and through an in-memory partition list for
//! generated tables (and the built-in rows of `system_schema`), so every system table supports the
//! full SELECT language. Results are streamed partition chunk by chunk; a page stops reading as
//! soon as it is full.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_cql::ast::{self, Direction, OrderByExpr, Selectable, Statement};
use nosql_storage::compare::{compare_clustering, compare_clustering_bound, slice_contains};
use nosql_storage::read::{
    ClusteringFilter, ColumnFilter, Limits, Partition, PartitionChunk, PartitionRead, RangeRead,
    ReadOptions, Resume,
};
use nosql_storage::sstable::cursor::KeyBound;
use nosql_storage::{Bound as SBound, Clustering, ClusteringComparator, PartitionKey, Row, Slice};
use nosql_types::{CqlType, Dialect};

use super::paging::PagingState;
use super::read_warn::{self, Tracker};
use crate::config::StrictAllowFiltering;
use crate::database::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::messages as msg;
use crate::prepare::restrictions::{self, KeyMode, Partitions, Restrictions, RowFilter, StmtKind};
use crate::prepare::selection::{self, CellMeta, RowData, Selection};
use crate::prepare::terms::{self, Bound, EvalCtx, Term};
use crate::prepare::{PrepareCtx, Receiver};
use crate::response::{ColumnSpec, QueryParams, Response, ResponseBody, Rows};
use crate::schema::{ColumnKind, Schema, Table, TableKind};
use crate::storage;

/// A prepared SELECT.
pub struct SelectPlan {
    pub table: Arc<Table>,
    pub selection: Selection,
    pub restrictions: Restrictions,
    pub distinct: bool,
    pub limit: Option<Term>,
    pub per_partition_limit: Option<Term>,
    /// Read clusterings in reverse storage order.
    pub reversed: bool,
    /// Clustering positions to sort an IN query's rows on (post-query ordering).
    pub post_order: Option<Vec<usize>>,
    /// Partition key bind variables (PREPARED metadata).
    pub pk_indexes: Vec<u16>,
    /// Number of primary key columns (partition key first) rows are grouped on.
    pub group_by: Option<usize>,
    pub bind: Vec<ColumnSpec>,
    /// Columns a row must provide (selection, filters, ordering).
    pub needed: Vec<usize>,
    /// Return a row for a partition with only static content.
    pub static_content_rows: bool,
    pub warnings: Vec<String>,
    /// ScyllaDB `USING TIMEOUT` (validated; reads never time out here).
    pub timeout: Option<Term>,
    /// `ORDER BY col ANN OF vector`.
    pub ann: Option<AnnOrder>,
    /// ScyllaDB's full-text search (`WHERE BM25(...) > 0 ORDER BY BM25(...)`).
    pub bm25: Option<crate::index::fulltext::Bm25Plan>,
    /// The index read when the restrictions use an index (`crate::hooks::IndexPlanner::plan`).
    pub index: Option<Arc<dyn crate::hooks::IndexScan>>,
    /// The index ScyllaDB's trace reports consulting (`IndexPlanner::consulted`).
    pub consulted: Option<String>,
    /// The user functions the statement calls (`EXECUTE` checks, `crate::udf`).
    pub functions: Vec<Arc<dyn crate::functions::Function>>,
    /// ScyllaDB's `USING SERVICE LEVEL name`: must exist when the statement runs.
    pub service_level: Option<String>,
    /// ScyllaDB's `BYPASS CACHE` (the read leaves the row cache as it is).
    pub bypass_cache: bool,
}

/// `ORDER BY column ANN OF vector`: the column (table index) and the query vector.
#[derive(Clone, Debug)]
pub struct AnnOrder {
    pub column: usize,
    pub vector: Term,
}

impl SelectPlan {
    pub fn result_specs(&self) -> Vec<ColumnSpec> {
        self.selection.result_specs(&self.table)
    }
}

fn order_column(table: &Table, ident: &ast::Ident, d: Dialect) -> Result<usize> {
    let name = ident.name();
    table.column_index(&name).ok_or_else(|| match d {
        Dialect::Scylla => Error::invalid(format!("Order by on unknown column {name}")),
        Dialect::Cassandra => msg::undefined_column(d, &table.keyspace, &table.name, &name),
    })
}

fn ann_needs_index(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => {
            "ANN ordering by vector requires the column to be indexed using 'vector_index'"
        }
        Dialect::Cassandra => "ANN ordering by vector requires the column to be indexed",
    })
}

/// ScyllaDB rejects a selector alias used as a column in the WHERE clause.
fn check_where_aliases(table: &Table, s: &ast::Select) -> Result<()> {
    let is_alias = |name: &str| {
        table.column_index(name).is_none()
            && s.selectors
                .iter()
                .any(|sel| sel.alias.as_ref().is_some_and(|a| a.name() == name))
    };
    for rel in &s.where_clause {
        let names: Vec<String> = match rel {
            ast::Relation::Single(r) => vec![r.column.name().to_string()],
            ast::Relation::MultiColumn(r) => {
                r.columns.iter().map(|c| c.name().to_string()).collect()
            }
            _ => Vec::new(),
        };
        if let Some(n) = names.iter().find(|n| is_alias(n)) {
            return Err(Error::invalid(format!(
                "Aliases aren't allowed in the WHERE clause (name: '{n}')"
            )));
        }
    }
    Ok(())
}

/// Unknown column names in a SELECT clause fail before anything else.
fn check_selector_names(table: &Table, s: &Selectable, d: Dialect) -> Result<()> {
    match s {
        Selectable::Column(id) => {
            let name = id.name();
            if table.column(&name).is_none() {
                return Err(msg::undefined_column(
                    d,
                    &table.keyspace,
                    &table.name,
                    &name,
                ));
            }
            Ok(())
        }
        Selectable::Metadata(_, a)
        | Selectable::Field(a, _)
        | Selectable::Element(a, _)
        | Selectable::TypeHint(_, a)
        | Selectable::Negate(a) => check_selector_names(table, a, d),
        Selectable::Cast(c) => check_selector_names(table, &c.arg, d),
        Selectable::Slice(sl) => check_selector_names(table, &sl.receiver, d),
        Selectable::Function(_, args)
        | Selectable::List(args)
        | Selectable::Set(args)
        | Selectable::Tuple(args) => args
            .iter()
            .try_for_each(|a| check_selector_names(table, a, d)),
        Selectable::MapOrUdt(kv) => kv.iter().try_for_each(|(k, v)| {
            check_selector_names(table, k, d).and(check_selector_names(table, v, d))
        }),
        Selectable::Binary(b) => {
            check_selector_names(table, &b.left, d).and(check_selector_names(table, &b.right, d))
        }
        Selectable::Term(_) | Selectable::CountRows => Ok(()),
    }
}

/// Prepares a SELECT.
pub fn prepare(
    db: &Arc<Database>,
    schema: &Arc<Schema>,
    s: &ast::Select,
    stmt: &Statement<'static>,
    keyspace: Option<&str>,
) -> Result<SelectPlan> {
    let d = db.dialect();
    let mut table = super::resolve_table(db, schema, &s.table, keyspace)?;
    if s.mutation_fragments {
        crate::mutation_fragments::check_order_by(!s.order_by.is_empty())?;
        table = Arc::new(crate::mutation_fragments::derived_table(&table));
    }
    for sel in &s.selectors {
        check_selector_names(&table, &sel.expr, d)?;
    }
    if !s.using.is_empty() {
        crate::schema::ddl::check_timeout(&s.using, d)?;
    }
    // ORDER BY columns resolve before the WHERE clause is analysed.
    let mut orderings: Vec<(usize, bool)> = Vec::new();
    let mut cx = PrepareCtx::new(
        d,
        schema,
        &table.keyspace,
        &db.functions,
        &stmt.bind_markers,
    );
    cx.apply_settings(&db.settings());
    let uf = db.user_functions.read().clone();
    cx.user_functions = uf.as_deref();
    if d.is_scylla() {
        crate::index::fulltext::check_selectors(&s.selectors, &cx)?;
    }
    let planner = if table.kind == TableKind::Regular {
        db.hooks().index
    } else {
        None
    };
    let mut ann: Option<AnnOrder> = None;
    // ScyllaDB's full-text search (`crate::index::fulltext`).
    let mut bm25_order: Option<crate::index::fulltext::Call> = None;
    for o in &s.order_by {
        match &o.expr {
            OrderByExpr::Column(id) => {
                let c = order_column(&table, id, d)?;
                if ann.is_some() || bm25_order.is_some() {
                    return Err(other_ordering_with_ann(d));
                }
                orderings.push((c, o.direction == Some(Direction::Desc)));
            }
            OrderByExpr::Ann(id, term) => {
                // The vector is checked against the column before the index lookup.
                let ci = match (d, table.column_index(&id.name())) {
                    (Dialect::Scylla, None) => {
                        return Err(Error::invalid(format!(
                            "Undefined column name {}",
                            id.name()
                        )));
                    }
                    _ => order_column(&table, id, d)?,
                };
                let col = &table.columns[ci];
                if d.is_scylla()
                    && !matches!(&col.ty, CqlType::Vector { elem, .. } if **elem == CqlType::Float)
                {
                    return Err(Error::invalid(
                        "ANN ordering is only supported on float vector indexes",
                    ));
                }
                let vector = terms::prepare(
                    term,
                    &Receiver::new(
                        &table.keyspace,
                        &table.name,
                        col.name.clone(),
                        col.ty.clone(),
                    ),
                    &mut cx,
                )?;
                if ann.is_some() {
                    return Err(Error::invalid("Cannot specify more than one ANN ordering"));
                }
                if bm25_order.is_some() {
                    return Err(Error::invalid(
                        "Cannot specify more than one similarity ordering",
                    ));
                }
                if !orderings.is_empty() {
                    return Err(other_ordering_with_ann(d));
                }
                if o.direction == Some(Direction::Desc) {
                    return Err(Error::invalid("Descending ANN ordering is not supported"));
                }
                if planner.is_none() {
                    return Err(ann_needs_index(d));
                }
                ann = Some(AnnOrder { column: ci, vector });
            }
            OrderByExpr::Function(f, args) if d.is_scylla() => {
                if !crate::index::fulltext::resolves_to_bm25(f, &cx)? {
                    return Err(crate::index::fulltext::other_order_function(f, &cx));
                }
                if ann.is_some() || bm25_order.is_some() || !orderings.is_empty() {
                    return Err(Error::invalid(
                        "bm25 ordering does not support any other ordering",
                    ));
                }
                bm25_order = Some(crate::index::fulltext::call(&table, args, &mut cx)?);
            }
            OrderByExpr::Function(..) => return Err(ann_needs_index(d)),
        }
    }
    cx.indexes = planner.clone();
    cx.ann = ann.as_ref().map(|a| a.column);
    let static_only_select = !s.selectors.is_empty() && selects_only_static(&table, &s.selectors);
    if d.is_scylla() {
        check_where_aliases(&table, s)?;
    }
    // A full-text search takes its `BM25()` restriction and no other: it is checked whole
    // (`fulltext::plan`) instead of analysed.
    let (mut bm25_filters, other_relations) = if d.is_scylla() {
        let (f, rest) = crate::index::fulltext::split_where(&table, &s.where_clause, &mut cx)?;
        (f, rest.len())
    } else {
        (Vec::new(), s.where_clause.len())
    };
    let fulltext = !bm25_filters.is_empty() || bm25_order.is_some();
    let mut restrictions = restrictions::analyze(
        &table,
        StmtKind::Select,
        if fulltext { &[] } else { &s.where_clause },
        s.allow_filtering,
        static_only_select,
        false,
        &mut cx,
    )?;
    if restrictions.implicit_filtering {
        match db.settings().strict_allow_filtering {
            StrictAllowFiltering::True => return Err(msg::allow_filtering()),
            StrictAllowFiltering::Warn => restrictions.warnings.push(
                "This query should use ALLOW FILTERING and will be rejected in future versions."
                    .into(),
            ),
            StrictAllowFiltering::False => {}
        }
    }
    let selection = selection::prepare(&table, &s.selectors, s.json, &mut cx)?;
    if s.distinct {
        if s.per_partition_limit.is_some() {
            return Err(Error::invalid(
                "PER PARTITION LIMIT is not allowed with SELECT DISTINCT queries",
            ));
        }
        validate_distinct(&table, &selection, &restrictions, d)?;
    }
    // GROUP BY
    let group_by = if s.group_by.is_empty() {
        None
    } else {
        Some(prepare_group_by(
            &table,
            &s.group_by,
            &restrictions,
            s.distinct,
            d,
        )?)
    };
    let bm25 = crate::index::fulltext::plan(crate::index::fulltext::Query {
        table: &table,
        filter: bm25_filters.pop(),
        order: bm25_order,
        ann: ann.is_some(),
        aggregate: selection.is_aggregate() || group_by.is_some(),
        limit: s.limit.as_ref(),
        per_partition_limit: s.per_partition_limit.is_some(),
        others: other_relations,
    })?;
    if group_by.is_none() && selection.is_aggregate() && s.per_partition_limit.is_some() {
        return Err(Error::invalid(
            "PER PARTITION LIMIT is not allowed with aggregate queries.",
        ));
    }
    // ORDER BY
    let mut reversed = false;
    let mut post_order = None;
    if !orderings.is_empty() {
        if restrictions.uses_index {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "ORDER BY with 2ndary indexes is not supported.",
                Dialect::Cassandra => {
                    "ORDER BY with 2ndary indexes is not supported, except for ANN queries."
                }
            }));
        }
        if restrictions.key_mode != KeyMode::Keys {
            return Err(Error::invalid(
                "ORDER BY is only supported when the partition key is restricted by an EQ or an IN.",
            ));
        }
        reversed = ordering_reversal(&table, &orderings, &restrictions, d)?;
        let is_in = restrictions
            .pk
            .iter()
            .any(|p| p.iter().any(|x| matches!(x, restrictions::Pred::In(_))));
        if is_in {
            post_order = Some(
                orderings
                    .iter()
                    .map(|(c, _)| table.columns[*c].position as usize)
                    .collect(),
            );
        }
    }
    if restrictions.index_needs_filtering {
        return Err(msg::allow_filtering());
    }
    // The index read (or the vector search).
    let index = match &planner {
        Some(p) if ann.is_some() || restrictions.uses_index => {
            p.plan(db, &table, &restrictions, ann.as_ref())?
        }
        _ => None,
    };
    let limit = s
        .limit
        .as_ref()
        .map(|t| {
            terms::prepare(
                t,
                &Receiver::new(&table.keyspace, &table.name, "[limit]", CqlType::Int),
                &mut cx,
            )
        })
        .transpose()?;
    let per_partition_limit = s
        .per_partition_limit
        .as_ref()
        .map(|t| {
            terms::prepare(
                t,
                &Receiver::new(
                    &table.keyspace,
                    &table.name,
                    if d.is_scylla() {
                        "[limit]"
                    } else {
                        "[per_partition_limit]"
                    },
                    CqlType::Int,
                ),
                &mut cx,
            )
        })
        .transpose()?;
    let timeout = s
        .using
        .timeout
        .as_ref()
        .map(|t| {
            terms::prepare(
                t,
                &Receiver::new(&table.keyspace, &table.name, "[timeout]", CqlType::Duration),
                &mut cx,
            )
        })
        .transpose()?;
    let bind = cx.bind_specs()?;
    let pk_indexes = cx.pk_indexes(&table);
    let mut needed: Vec<usize> = selection.columns.clone();
    needed.extend(table.partition_key.iter().copied());
    needed.extend(table.clustering.iter().copied());
    for (ci, _) in &restrictions.others {
        needed.push(*ci);
    }
    if let Some(a) = &ann {
        needed.push(a.column);
    }
    needed.sort_unstable();
    needed.dedup();
    let static_content_rows = !restrictions.ck.iter().any(|p| !p.is_empty())
        && restrictions.multi.is_empty()
        && !restrictions
            .others
            .iter()
            .any(|(c, _)| table.columns[*c].kind == ColumnKind::Regular);
    let warnings = restrictions.warnings.clone();
    let consulted = match &planner {
        Some(p) if restrictions.uses_index && ann.is_none() => p.consulted(&table, &restrictions),
        _ => None,
    };
    Ok(SelectPlan {
        table,
        selection,
        restrictions,
        distinct: s.distinct,
        limit,
        per_partition_limit,
        reversed,
        post_order,
        group_by,
        bind,
        pk_indexes,
        needed,
        static_content_rows,
        warnings,
        timeout,
        ann,
        bm25,
        index,
        consulted,
        functions: std::mem::take(&mut cx.used),
        service_level: s.using.service_level.as_ref().map(|n| n.name().into_owned()),
        bypass_cache: s.bypass_cache,
    })
}

fn other_ordering_with_ann(d: Dialect) -> Error {
    Error::invalid(match d {
        Dialect::Scylla => "Similarity ordering does not support any other ordering",
        Dialect::Cassandra => "ANN ordering does not support any other ordering",
    })
}

fn selects_only_static(table: &Table, selectors: &[ast::Selector]) -> bool {
    fn cols(s: &Selectable, out: &mut Vec<String>) {
        match s {
            Selectable::Column(id) => out.push(id.name().to_string()),
            Selectable::Metadata(_, a)
            | Selectable::Field(a, _)
            | Selectable::Element(a, _)
            | Selectable::TypeHint(_, a)
            | Selectable::Negate(a) => cols(a, out),
            Selectable::Cast(c) => cols(&c.arg, out),
            Selectable::Slice(sl) => cols(&sl.receiver, out),
            Selectable::Function(_, args)
            | Selectable::List(args)
            | Selectable::Set(args)
            | Selectable::Tuple(args) => args.iter().for_each(|a| cols(a, out)),
            Selectable::MapOrUdt(kv) => kv.iter().for_each(|(k, v)| {
                cols(k, out);
                cols(v, out)
            }),
            Selectable::Binary(b) => {
                cols(&b.left, out);
                cols(&b.right, out)
            }
            _ => {}
        }
    }
    if !table.has_statics() {
        return false;
    }
    let mut names = Vec::new();
    for s in selectors {
        cols(&s.expr, &mut names);
    }
    let kinds: Vec<ColumnKind> = names
        .iter()
        .filter_map(|n| table.column(n))
        .map(|c| c.kind)
        .collect();
    kinds.contains(&ColumnKind::Static)
        && kinds
            .iter()
            .all(|k| matches!(k, ColumnKind::Static | ColumnKind::PartitionKey))
}

fn validate_distinct(table: &Table, sel: &Selection, r: &Restrictions, d: Dialect) -> Result<()> {
    let ck_restricted = r.ck.iter().any(|p| !p.is_empty()) || !r.multi.is_empty();
    let non_static = r.others.iter().any(|(c, _)| !table.columns[*c].is_static());
    if ck_restricted || (d.is_scylla() && !r.others.is_empty()) || (!d.is_scylla() && non_static) {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                "SELECT DISTINCT with WHERE clause only supports restriction by partition key."
            }
            Dialect::Cassandra => {
                "SELECT DISTINCT with WHERE clause only supports restriction by partition key and/or static columns."
            }
        }));
    }
    let cols: Vec<usize> = if sel.wildcard {
        (0..table.columns.len()).collect()
    } else {
        sel.columns.clone()
    };
    for &c in &cols {
        let col = &table.columns[c];
        if col.kind != ColumnKind::PartitionKey && col.kind != ColumnKind::Static {
            return Err(Error::invalid(format!(
                "SELECT DISTINCT queries must only request partition key columns and/or static columns (not {})",
                col.name
            )));
        }
    }
    if r.key_mode != KeyMode::Keys {
        for &p in &table.partition_key {
            if !cols.contains(&p) {
                return Err(Error::invalid(format!(
                    "SELECT DISTINCT queries must request all the partition key columns (missing {})",
                    table.columns[p].name
                )));
            }
        }
    }
    Ok(())
}

fn prepare_group_by(
    table: &Table,
    groups: &[Selectable],
    r: &Restrictions,
    distinct: bool,
    d: Dialect,
) -> Result<usize> {
    let pk: Vec<usize> = table
        .partition_key
        .iter()
        .chain(table.clustering.iter())
        .copied()
        .collect();
    let mut next = 0usize;
    for g in groups {
        let Selectable::Column(id) = g else {
            return Err(Error::invalid(
                "Only column names and monotonic scalar functions are supported in the GROUP BY clause.",
            ));
        };
        let name = id.name();
        let Some(ci) = table.column_index(&name) else {
            return Err(match d {
                Dialect::Scylla => Error::invalid(format!("Group by unknown column {name}")),
                Dialect::Cassandra => msg::undefined_column(d, &table.keyspace, &table.name, &name),
            });
        };
        let col = &table.columns[ci];
        if !col.is_primary_key() {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!("Group by non-primary-key column {name}"),
                Dialect::Cassandra => format!(
                    "Group by is currently only supported on the columns of the PRIMARY KEY, got {name}"
                ),
            }));
        }
        loop {
            let Some(&pkc) = pk.get(next) else {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => format!("Group by column {name} is out of order"),
                    Dialect::Cassandra => "Group by currently only support groups of columns following their declared order in the PRIMARY KEY".into(),
                }));
            };
            next += 1;
            if pkc == ci {
                break;
            }
            let restricted_eq = match table.columns[pkc].kind {
                ColumnKind::PartitionKey => r.pk[table.columns[pkc].position as usize]
                    .iter()
                    .any(|p| matches!(p, restrictions::Pred::Eq(_))),
                _ => r.ck[table.columns[pkc].position as usize]
                    .iter()
                    .any(|p| matches!(p, restrictions::Pred::Eq(_))),
            };
            if !restricted_eq {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => format!("Group by column {name} is out of order"),
                    Dialect::Cassandra => "Group by currently only support groups of columns following their declared order in the PRIMARY KEY".into(),
                }));
            }
        }
    }
    if next < table.partition_key.len() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => "GROUP BY must include the entire partition key",
            Dialect::Cassandra => "Group by is not supported on only a part of the partition key",
        }));
    }
    if distinct && next > table.partition_key.len() {
        return Err(Error::invalid(
            "Grouping on clustering columns is not allowed for SELECT DISTINCT queries",
        ));
    }
    Ok(next)
}

/// `isReversed`: whether ORDER BY reads clusterings backwards.
fn ordering_reversal(
    table: &Table,
    orderings: &[(usize, bool)],
    r: &Restrictions,
    d: Dialect,
) -> Result<bool> {
    let mut reversed_map: Vec<Option<bool>> = vec![None; table.clustering.len()];
    let mut i = 0usize;
    let mut seen: Vec<usize> = Vec::new();
    for &(c, desc) in orderings {
        let col = &table.columns[c];
        if col.kind != ColumnKind::Clustering {
            return Err(Error::invalid(format!(
                "Order by is currently only supported on the clustered columns of the PRIMARY KEY, got {}",
                col.name
            )));
        }
        let pos = col.position as usize;
        if d.is_scylla() && seen.contains(&pos) {
            return Err(Error::invalid(
                "Order by currently only supports the ordering of columns following their declared order in the PRIMARY KEY",
            ));
        }
        seen.push(pos);
        while i < pos {
            // An IN of one value is an equality here.
            let single = |v: &restrictions::InValues| matches!(v, restrictions::InValues::Terms(t) if t.len() == 1);
            let eq = r.ck[i].iter().any(|p| match p {
                restrictions::Pred::Eq(_) => true,
                restrictions::Pred::In(v) => single(v),
                _ => false,
            }) || r.multi.iter().any(|m| {
                m.positions.contains(&i)
                    && match &m.kind {
                        restrictions::MultiKind::Eq(_) => true,
                        restrictions::MultiKind::In(v) => single(v),
                        _ => false,
                    }
            });
            if !eq {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => format!("Unsupported order by relation - column {} doesn't have an ordering or EQ relation.", table.columns[table.clustering[i]].name),
                    Dialect::Cassandra => "Order by currently only supports the ordering of columns following their declared order in the PRIMARY KEY".into(),
                }));
            }
            i += 1;
        }
        i = pos + 1;
        reversed_map[pos] = Some(desc != col.reversed);
    }
    let mut result: Option<bool> = None;
    for (pos, b) in reversed_map.iter().enumerate() {
        let Some(b) = b else { continue };
        match result {
            None => result = Some(*b),
            Some(prev) if prev != *b => {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => format!(
                        "Unsupported order by relation - only reversing all columns is supported, but column {} has opposite ordering",
                        table.columns[table.clustering[pos]].name
                    ),
                    Dialect::Cassandra => "Unsupported order by relation".into(),
                }));
            }
            _ => {}
        }
    }
    Ok(result.unwrap_or(false))
}

// ---- execution -------------------------------------------------------------------------------

/// Evaluates a LIMIT term.
fn eval_limit(t: &Option<Term>, cx: &EvalCtx, per_partition: bool) -> Result<Option<u64>> {
    let Some(t) = t else { return Ok(None) };
    match t.eval(cx)? {
        Bound::Unset => Ok(None),
        Bound::Null => Err(Error::invalid(if per_partition {
            "Invalid null value of per partition limit"
        } else {
            "Invalid null value of limit"
        })),
        Bound::Val(b) => {
            let v = <[u8; 4]>::try_from(&b[..])
                .map(i32::from_be_bytes)
                .map_err(|_| Error::invalid("Invalid limit value"))?;
            if v <= 0 {
                return Err(msg::limit_not_positive(cx.dialect, per_partition));
            }
            Ok(Some(v as u64))
        }
    }
}

/// Partitions one at a time, from storage or memory.
enum Source {
    Keys {
        table: Arc<nosql_storage::table::Table>,
        reads: VecDeque<(u64, PartitionRead)>,
        current: Option<nosql_storage::read::ReadStream>,
        index: u64,
        /// Cassandra's tombstone warning: one read command per partition.
        track: Option<Box<Tracker>>,
        /// Rows the next partition read may return ([`Source::set_room`]).
        room: u64,
    },
    Range(nosql_storage::read::ReadStream, Option<Box<Tracker>>),
    /// Chunks with the index of their key in an `IN` list (0 for scans).
    Memory(VecDeque<(u64, PartitionChunk)>, u64),
    /// Base partitions found through an index, in result order (with where an index read
    /// paged by entries resumes).
    Index(
        futures::stream::BoxStream<'static, Result<PartitionChunk>>,
        Option<Arc<parking_lot::Mutex<Option<Bytes>>>>,
    ),
}

impl Source {
    /// The key-list position of the partition last returned.
    fn key_index(&self) -> u64 {
        match self {
            Source::Keys { index, .. } => *index,
            Source::Memory(_, i) => *i,
            Source::Range(..) | Source::Index(..) => 0,
        }
    }

    /// The rows the next partition read of an `IN` / `=` query may return: its storage limit
    /// (when that is exact) and its paged `LIMIT` (`MultiPartitionPager`).
    fn set_room(&mut self, n: u64) {
        if let Source::Keys { room, .. } = self {
            *room = n;
        }
    }

    fn tracker(&mut self) -> Option<&mut Tracker> {
        match self {
            Source::Keys { track, .. } | Source::Range(_, track) => track.as_deref_mut(),
            _ => None,
        }
    }

    /// The command reading stopped after row `row` of the last chunk.
    fn stop_at(&mut self, row: usize) {
        if let Some(t) = self.tracker() {
            t.stop_at(row);
        }
    }

    /// Whether the partition being read is the last of an `IN` / `=` query.
    fn last_key(&self) -> bool {
        matches!(self, Source::Keys { reads, .. } if reads.is_empty())
    }

    /// The command reading skipped the partition it opened last after its static row.
    fn stop_after_static(&mut self) {
        if let Some(t) = self.tracker() {
            t.stop_after_static();
        }
    }

    /// The command reading stopped before the partition it opened last.
    fn stop_at_partition_start(&mut self) {
        if let Some(t) = self.tracker() {
            t.stop_at_partition_start();
        }
    }

    /// Closes the open command; the tombstone warnings of the execution.
    fn warnings(&mut self) -> Vec<String> {
        self.tracker().map(|t| t.finish()).unwrap_or_default()
    }

    /// Where the read ended at ScyllaDB's `query_tombstone_page_limit` (once `next` ended).
    fn page_cut(&self) -> Option<nosql_storage::read::PageCut> {
        match self {
            Source::Keys {
                current: Some(s), ..
            }
            | Source::Range(s, _) => s.metrics.page_cut(),
            _ => None,
        }
    }

    async fn next(&mut self) -> Result<Option<PartitionChunk>> {
        match self {
            Source::Keys {
                table,
                reads,
                current,
                index,
                track,
                room,
            } => loop {
                if let Some(s) = current {
                    match s.next().await {
                        Some(c) => return c.map(Some).map_err(read_error),
                        // A read cut at the tombstone page limit ends the page.
                        None if s.metrics.page_cut().is_some() => return Ok(None),
                        None => {
                            if let Some(t) = track {
                                t.end();
                            }
                            *current = None
                        }
                    }
                }
                match reads.pop_front() {
                    Some((i, mut r)) => {
                        *index = i;
                        let Some(t) = track else {
                            *current = Some(table.read(r));
                            continue;
                        };
                        if t.query.storage_limited {
                            r.limits.rows = *room;
                        }
                        let command = read_warn::Command {
                            place: read_warn::Place::Key(r.key.clone()),
                            clustering: r.clustering.clone(),
                            reversed: r.reversed,
                            room: *room,
                            groups_done: None,
                        };
                        let s = table.read(r);
                        t.start(&s.metrics, command);
                        *current = Some(s);
                    }
                    None => return Ok(None),
                }
            },
            Source::Range(s, track) => match s.next().await {
                Some(c) => c.map(Some).map_err(read_error),
                None => {
                    if let Some(t) = track {
                        t.end();
                    }
                    Ok(None)
                }
            },
            Source::Index(s, _) => s.next().await.transpose(),
            Source::Memory(q, last) => match q.pop_front() {
                Some((i, c)) => {
                    *last = i;
                    Ok(Some(c))
                }
                None => Ok(None),
            },
        }
    }
}

fn read_error(e: anyhow::Error) -> Error {
    if let Some(t) = e.downcast_ref::<nosql_storage::read::TombstoneOverwhelming>() {
        let _ = t;
        return Error {
            code: ErrorCode::ReadFailure,
            message: "Operation failed - received 0 responses and 1 failures: READ_TOO_MANY_TOMBSTONES from /127.0.0.1:7000".into(),
            detail: crate::error::ErrorDetail::ReadFailure {
                consistency: crate::error::Consistency::One,
                received: 0,
                block_for: 1,
                num_failures: 1,
                reasons: vec![(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1)],
                data_present: false,
            },
            warnings: Vec::new(),
            tracing_id: None,
        };
    }
    Error::internal(e)
}

/// A read failure reports the request's consistency (Cassandra's `ReadFailureException` from
/// the coordinator's replica plan).
fn at_consistency(mut e: Error, cl: crate::error::Consistency) -> Error {
    if let crate::error::ErrorDetail::ReadFailure { consistency, .. } = &mut e.detail {
        *consistency = cl;
    }
    e
}

/// Restricts a clustering filter to what comes after `after` in query order.
fn filter_after(
    cmp: &dyn ClusteringComparator,
    f: &ClusteringFilter,
    after: &Clustering,
    reversed: bool,
) -> ClusteringFilter {
    match f {
        ClusteringFilter::Names(names) => ClusteringFilter::Names(
            names
                .iter()
                .filter(|c| {
                    let o = compare_clustering(cmp, c, after);
                    if reversed { o.is_lt() } else { o.is_gt() }
                })
                .cloned()
                .collect(),
        ),
        ClusteringFilter::Slices(slices) => {
            let mut out = Vec::new();
            for s in slices {
                if reversed {
                    // Keep [start, min(end, before(after))].
                    let cut = SBound::exclusive_end(after.clone());
                    if compare_clustering_bound(cmp, after, &s.start).is_lt() {
                        continue;
                    }
                    let end = if compare_clustering_bound(cmp, after, &s.end).is_lt() {
                        cut
                    } else {
                        s.end.clone()
                    };
                    out.push(Slice::new(s.start.clone(), end));
                } else {
                    let cut = SBound::exclusive_start(after.clone());
                    if compare_clustering_bound(cmp, after, &s.end).is_gt() {
                        continue;
                    }
                    let start = if compare_clustering_bound(cmp, after, &s.start).is_gt() {
                        cut
                    } else {
                        s.start.clone()
                    };
                    out.push(Slice::new(start, s.end.clone()));
                }
            }
            ClusteringFilter::Slices(out)
        }
    }
}

/// The rows of a `MUTATION_FRAGMENTS()` read ([`crate::mutation_fragments`]): generated for the
/// restricted partitions, filtered by the clustering restrictions; a page resumes after the
/// exact last row.
async fn mutation_fragments_source(
    db: &Arc<Database>,
    t: &Table,
    parts: &Partitions,
    clustering: &ClusteringFilter,
    state: Option<&PagingState>,
    now: i64,
) -> Result<Source> {
    let d = db.dialect();
    let schema = db.schema();
    let base = schema
        .table_by_id(&t.id)
        .ok_or_else(|| msg::unconfigured_table(d, &t.name))?;
    let mut keys: Vec<PartitionKey> = Vec::new();
    if let Partitions::Keys(ks) = parts {
        for k in ks {
            keys.push(storage::partition_key(k, d, false)?);
        }
    }
    let virtual_parts = match base.kind {
        TableKind::Regular => None,
        _ => Some(virtual_partitions(db, base)?),
    };
    // Slices of the base's clustering (cut at their bounds, as ScyllaDB slices the base).
    let clip = crate::mutation_fragments::base_slices(t, base, clustering);
    let generated = crate::mutation_fragments::partitions(
        db,
        t,
        base,
        parts,
        &keys,
        virtual_parts,
        clip.as_deref(),
        now,
    )
    .await?;
    let cmp = t.comparator.clone();
    let mut chunks: VecDeque<(u64, PartitionChunk)> = VecDeque::new();
    let mut resuming = state.is_some();
    for (key, rows) in generated {
        let pos = match parts {
            Partitions::Keys(_) => keys.iter().position(|k| *k == key).unwrap_or(0) as u64,
            Partitions::Range { start, end } => {
                if !start.admits_from(&key) || !end.admits_to(&key) {
                    continue;
                }
                0
            }
        };
        let mut rows: Vec<Row> = rows
            .into_iter()
            .filter(|r| clip.is_some() || filter_contains(&*cmp, clustering, &r.clustering))
            .collect();
        if let Some(s) = state
            && resuming
        {
            let here = match parts {
                Partitions::Keys(_) => pos == s.key_index,
                Partitions::Range { .. } => key.key == s.key,
            };
            if !here {
                continue;
            }
            resuming = false;
            match &s.clustering {
                None => continue,
                Some(c) => match rows.iter().position(|r| &r.clustering == c) {
                    Some(i) => {
                        rows.drain(..=i);
                    }
                    None => continue,
                },
            }
        }
        chunks.push_back((
            pos,
            PartitionChunk {
                key,
                partition_deletion: nosql_storage::DeletionTime::LIVE,
                static_row: None,
                rows,
                markers: Vec::new(),
                first: true,
                last: true,
            },
        ));
    }
    Ok(Source::Memory(chunks, 0))
}

fn filter_contains(cmp: &dyn ClusteringComparator, f: &ClusteringFilter, c: &Clustering) -> bool {
    match f {
        ClusteringFilter::Names(n) => n.iter().any(|x| compare_clustering(cmp, x, c).is_eq()),
        ClusteringFilter::Slices(s) => s
            .iter()
            .any(|sl| slice_contains(cmp, &sl.start, &sl.end, c)),
    }
}

/// Rows of a generated table (or the built-in part of a `system_schema` table) as partitions.
fn virtual_partitions(db: &Database, t: &Table) -> Result<Vec<Partition>> {
    if t.kind == TableKind::SchemaTable {
        return Ok(db.builtin_schema_rows(&t.name).to_vec());
    }
    let rows = db.virtual_rows(t)?;
    let mut parts: Vec<Partition> = Vec::new();
    let mut keys: Vec<Vec<Bytes>> = Vec::new();
    for vr in rows {
        let mut values: Vec<Option<Bytes>> = vec![None; t.columns.len()];
        for (name, v) in vr {
            if let Some(i) = t.column_index(name) {
                values[i] = v;
            }
        }
        let pk: Vec<Bytes> = t
            .partition_key
            .iter()
            .map(|&i| values[i].clone().unwrap_or_default())
            .collect();
        let key = storage::partition_key(&pk, db.dialect(), false)?;
        let clustering = Clustering::new(
            t.clustering
                .iter()
                .map(|&i| values[i].clone().unwrap_or_default()),
        );
        let mut row = Row::new(clustering).with_liveness(nosql_storage::LivenessInfo::live(0));
        let mut stat = Row::new(Clustering::EMPTY);
        for (i, c) in t.columns.iter().enumerate() {
            if c.is_primary_key() {
                continue;
            }
            if let Some(v) = &values[i] {
                let cell = nosql_storage::Cell::live(0, v.clone());
                if c.is_static() {
                    stat = stat.with_cell(c.id, cell);
                } else {
                    row = row.with_cell(c.id, cell);
                }
            }
        }
        row.columns.sort_by_key(|c| c.column());
        match parts.iter_mut().find(|p| p.key == key) {
            Some(p) => p.rows.push(row),
            None => {
                keys.push(pk);
                parts.push(Partition {
                    key,
                    partition_deletion: nosql_storage::DeletionTime::LIVE,
                    static_row: if stat.columns.is_empty() {
                        None
                    } else {
                        Some(stat)
                    },
                    rows: vec![row],
                    markers: Vec::new(),
                })
            }
        }
    }
    let cmp = t.comparator.clone();
    for p in &mut parts {
        p.rows
            .sort_by(|a, b| compare_clustering(&*cmp, &a.clustering, &b.clustering));
    }
    if local_order(db, t) {
        let mut both: Vec<(Vec<Bytes>, Partition)> = keys.into_iter().zip(parts).collect();
        both.sort_by(|a, b| compare_keys(t, &a.0, &b.0));
        return Ok(both.into_iter().map(|(_, p)| p).collect());
    }
    parts.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(parts)
}

/// Cassandra's virtual tables use a `LocalPartitioner`: partitions in key order, not token order.
fn local_order(db: &Database, t: &Table) -> bool {
    db.dialect() == nosql_types::Dialect::Cassandra
        && crate::system::catalog::is_virtual_keyspace(db.dialect(), &t.keyspace)
}

/// The partition key components in key order (for sorting IN keys by value).
fn compare_keys(t: &Table, a: &[Bytes], b: &[Bytes]) -> std::cmp::Ordering {
    for (i, &ci) in t.partition_key.iter().enumerate() {
        let o = nosql_types::compare(&t.columns[ci].ty, &a[i], &b[i]);
        if o.is_ne() {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

struct Out {
    rows: Vec<Vec<Option<Bytes>>>,
    /// Sort keys for post-query ordering: (clustering values, partition index, position).
    sort: Vec<(Vec<Option<Bytes>>, usize, usize)>,
    /// Cassandra's tombstone warnings.
    warnings: Vec<String>,
}

/// Executes a prepared SELECT.
/// Boxed so the future is checked `Send` here, with named lifetimes (rust-lang/rust#110338).
pub fn execute<'a>(
    db: &'a Arc<Database>,
    plan: &'a SelectPlan,
    params: &'a QueryParams,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_inner(db, plan, params, None))
}

/// [`execute`] with the column masks of the reading role ([`crate::masking::read_masks`]).
pub fn execute_masked<'a>(
    db: &'a Arc<Database>,
    plan: &'a SelectPlan,
    params: &'a QueryParams,
    masks: Option<Arc<crate::masking::ReadMasks>>,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_inner(db, plan, params, masks))
}

/// Table columns the restrictions of `plan` constrain (`StatementRestrictions.isRestricted`).
pub fn restricted_columns(plan: &SelectPlan) -> Vec<usize> {
    let t = &plan.table;
    let r = &plan.restrictions;
    let mut out = Vec::new();
    for (pos, preds) in r.pk.iter().enumerate() {
        if !preds.is_empty() || r.token.is_some() {
            out.extend(t.partition_key.get(pos).copied());
        }
    }
    for (pos, preds) in r.ck.iter().enumerate() {
        if !preds.is_empty() {
            out.extend(t.clustering.get(pos).copied());
        }
    }
    for m in &r.multi {
        out.extend(m.positions.iter().filter_map(|p| t.clustering.get(*p).copied()));
    }
    out.extend(r.others.iter().map(|(c, _)| *c));
    out.sort_unstable();
    out.dedup();
    out
}

async fn execute_inner(
    db: &Arc<Database>,
    plan: &SelectPlan,
    params: &QueryParams,
    masks: Option<Arc<crate::masking::ReadMasks>>,
) -> Result<Response> {
    let d = db.dialect();
    let mut cx = EvalCtx::new(&params.values, d);
    cx.now = params.now_in_seconds.map(i64::from);
    cx.masks = masks;
    if let Some(to) = &plan.timeout {
        crate::schema::ddl::check_timeout_value(to, &cx)?;
    }
    if let Some(sl) = &plan.service_level
        && sl != "default"
        && !crate::auth::snapshot(db).service_levels.contains_key(sl)
    {
        return Err(Error::invalid(format!("Service level {sl} doesn't exist")));
    }
    if params.consistency == crate::error::Consistency::Any {
        return Err(Error::invalid(
            "ANY ConsistencyLevel is only supported for writes",
        ));
    }
    if params.consistency == crate::error::Consistency::EachQuorum && d.is_scylla() {
        return Err(Error::invalid(
            "EACH_QUORUM ConsistencyLevel is only supported for writes",
        ));
    }
    if params.consistency.is_serial()
        && d.is_scylla()
        && plan.restrictions.key_mode != KeyMode::Keys
        && plan.ann.is_none()
    {
        return Err(Error::invalid(
            "SERIAL/LOCAL_SERIAL consistency may only be requested for one partition at a time",
        ));
    }
    if params.consistency.is_serial() {
        crate::describe::paxos::touch(db, &plan.table);
    }
    // Vector search answers before ScyllaDB's SERIAL partition check (`crate::index::vector`).
    if plan.ann.is_none() {
        crate::lwt::check_serial_read(d, plan, params, &cx)?;
    }
    if !d.is_scylla() {
        plan.selection.check_unset_arguments(&cx)?;
    }
    let user_limit = eval_limit(&plan.limit, &cx, false)?;
    let ppl = eval_limit(&plan.per_partition_limit, &cx, true)?;
    let page_size = params.page_size.filter(|p| *p > 0).map(|p| p as u64);
    let aggregate = plan.selection.is_aggregate() || plan.group_by.is_some();
    let mut warnings = plan.warnings.clone();
    if aggregate && !d.is_scylla() {
        let pk_restricted = plan.restrictions.pk.iter().any(|p| !p.is_empty());
        if !pk_restricted {
            warnings.push("Aggregation query used without partition key".into());
        } else if plan
            .restrictions
            .pk
            .iter()
            .any(|p| p.iter().any(|x| matches!(x, restrictions::Pred::In(_))))
        {
            warnings
                .push("Aggregation query used on multiple partition keys (IN restriction)".into());
        }
    }
    let post_order = plan.post_order.is_some();
    if post_order && page_size.is_some_and(|ps| user_limit.is_none_or(|l| l > ps)) && !aggregate {
        return Err(msg::order_by_in_paged());
    }
    // Top-K (ANN) queries are computed whole and returned in one page.
    let computed = plan.index.as_ref().is_some_and(|i| i.computed());
    if plan.ann.is_some() {
        warnings.extend(crate::index::vector::check_top_k(
            d, plan, params, &cx, user_limit, aggregate,
        )?);
    }
    if let Some(b) = &plan.bm25 {
        crate::index::fulltext::execute(b, &cx, user_limit)?;
    }
    rate_limit_reads(db, plan, &cx)?;
    let state = match &params.paging_state {
        Some(raw) if !raw.is_empty() => Some(PagingState::decode(raw)?),
        _ => None,
    };
    // A computed result (aggregates, groups, post-ordered rows) is produced whole and paged over.
    if aggregate || post_order || computed {
        let returned = match (&state, plan.group_by) {
            (Some(s), Some(_)) => s.returned,
            _ => 0,
        };
        let all = collect(db, plan, &cx, user_limit, ppl, page_size, returned)
            .await
            .map_err(|e| at_consistency(e, params.consistency))?;
        warnings.extend(all.warnings);
        let mut rows = all.rows;
        let mut paging = None;
        // ScyllaDB returns all groups in one page; Cassandra's group pager ends a full page with a
        // paging state (the next page may be empty).
        let grouped = plan.group_by.is_some();
        if (!aggregate || grouped) && !computed && !(grouped && d.is_scylla()) {
            let skip = state.as_ref().map(|s| s.returned).unwrap_or(0) as usize;
            rows = rows.into_iter().skip(skip).collect();
            if let Some(ps) = page_size
                && (rows.len() as u64 > ps || (grouped && rows.len() as u64 == ps))
            {
                rows.truncate(ps as usize);
                paging = Some(
                    PagingState {
                        returned: (skip + rows.len()) as u64,
                        ..Default::default()
                    }
                    .encode(),
                );
            }
        }
        return finish(plan, rows, paging, warnings, d);
    }
    let (rows, paging) = page(db, plan, &cx, user_limit, ppl, page_size, state)
        .await
        .map_err(|e| at_consistency(e, params.consistency))?;
    warnings.extend(paging.1);
    finish(plan, rows, paging.0, warnings, d)
}

/// ScyllaDB's per-partition rate limit on reads that name their partitions (each page is a
/// read; scans aren't limited).
fn rate_limit_reads(db: &Database, plan: &SelectPlan, cx: &EvalCtx<'_>) -> Result<()> {
    use crate::rate_limit::{Op, check, limit};
    let t = &plan.table;
    if !cx.dialect.is_scylla() || limit(t, Op::Read).is_none() || plan.ann.is_some() {
        return Ok(());
    }
    if plan.index.is_some() && plan.restrictions.key_mode != KeyMode::Keys {
        return Ok(());
    }
    let Ok(Partitions::Keys(keys)) = plan.restrictions.partitions(t, cx, None) else {
        return Ok(());
    };
    let mut tokens = Vec::with_capacity(keys.len());
    for k in &keys {
        tokens.push(storage::partition_key(k, cx.dialect, false)?.token);
    }
    check(db, t, &tokens, Op::Read)
}

fn finish(
    plan: &SelectPlan,
    rows: Vec<Vec<Option<Bytes>>>,
    paging: Option<Bytes>,
    warnings: Vec<String>,
    d: Dialect,
) -> Result<Response> {
    let rows = if plan.selection.json {
        rows.into_iter()
            .map(|r| plan.selection.to_json(&r, d).map(|j| vec![Some(j)]))
            .collect::<Result<Vec<_>>>()?
    } else {
        rows
    };
    let body = ResponseBody::Rows(Rows {
        metadata: crate::response::ResultMetadata {
            columns: plan.result_specs(),
        },
        rows,
        paging_state: paging,
    });
    Ok(Response::new(body).with_warnings(warnings))
}

/// The partitions and reads of the query (resuming after `state`). With `warn` (Cassandra's
/// tombstone warning), storage reads record their row positions and, when that is exact, stop
/// at `room` rows.
#[allow(clippy::too_many_arguments)]
async fn open_source(
    db: &Arc<Database>,
    plan: &SelectPlan,
    cx: &EvalCtx<'_>,
    state: Option<&PagingState>,
    per_partition: Option<u64>,
    page: Option<u64>,
    warn: Option<read_warn::Query>,
    room: u64,
) -> Result<Source> {
    let t = &plan.table;
    let d = cx.dialect;
    if let Some(ix) = &plan.index
        && (plan.ann.is_some() || plan.restrictions.key_mode != KeyMode::Keys)
    {
        // ScyllaDB's `scylla_index_query_latencies` (per page read through the index).
        if d.is_scylla()
            && let Some(name) = db
                .hooks()
                .index
                .and_then(|p| p.consulted(t, &plan.restrictions))
        {
            db.guardrail_metrics().count_index_query(&t.keyspace, &name, 0);
        }
        let r = ix.open(db, plan, cx, state, page)?;
        return Ok(Source::Index(r.chunks, r.next));
    }
    let max_keys = if d.is_scylla() {
        Some(db.settings().max_partition_key_restrictions_per_query)
    } else {
        None
    };
    let max_ck = if d.is_scylla() {
        Some(db.settings().max_clustering_key_restrictions_per_query)
    } else {
        None
    };
    let parts = plan.restrictions.partitions(t, cx, max_keys)?;
    let clustering = if plan.distinct {
        ClusteringFilter::all()
    } else {
        plan.restrictions.clustering(t, cx, max_ck)?
    };
    // ScyllaDB refuses an oversized clustering key in reads too.
    if d.is_scylla()
        && let ClusteringFilter::Names(names) = &clustering
    {
        for c in names {
            storage::check_clustering_length(c, d)?;
        }
    }
    let now = cx.now.unwrap_or_else(|| db.now_secs());
    let columns = ColumnFilter::all();
    let mut limits = Limits::default();
    if plan.distinct {
        limits.per_partition = 1;
    } else if let Some(p) = per_partition
        && plan.restrictions.filter(t, cx)?.is_empty()
        && plan.restrictions.others.is_empty()
    {
        limits.per_partition = p;
    }
    let options = ReadOptions {
        now,
        tombstone_failure_threshold: if d.is_scylla() {
            None
        } else {
            Some(db.settings().tombstone_failure_threshold)
        },
        raw: false,
        // ScyllaDB cuts a page after `query_tombstone_page_limit` tombstones (paged reads only).
        tombstone_page_limit: (d.is_scylla() && page.is_some())
            .then(|| db.settings().scylla.query_tombstone_page_limit)
            .filter(|l| *l > 0),
    };
    let cmp = t.comparator.clone();
    if crate::mutation_fragments::is_derived(t) {
        return mutation_fragments_source(db, t, &parts, &clustering, state, now).await;
    }
    if t.kind != TableKind::Regular {
        // Generated rows (plus stored rows for system_schema), filtered in memory.
        let mut all = virtual_partitions(db, t)?;
        if t.kind == TableKind::SchemaTable {
            let st = db.storage_table(t)?;
            let stored = st
                .scan(RangeRead::all(now))
                .partitions()
                .await
                .map_err(read_error)?;
            all.extend(stored);
            all.sort_by(|a, b| a.key.cmp(&b.key));
        }
        let wanted: Option<Vec<PartitionKey>> = match &parts {
            Partitions::Keys(keys) => {
                let mut ks: Vec<PartitionKey> = Vec::new();
                for k in keys {
                    ks.push(storage::partition_key(k, d, false)?);
                }
                Some(ks)
            }
            Partitions::Range { .. } => None,
        };
        let mut chunks: VecDeque<(u64, PartitionChunk)> = VecDeque::new();
        let mut key_order: Vec<(usize, PartitionChunk)> = Vec::new();
        let local = local_order(db, t);
        let mut resumed = false;
        for p in all {
            let pos = match &wanted {
                Some(ks) => match ks.iter().position(|k| k == &p.key) {
                    Some(i) => i,
                    None => continue,
                },
                None => {
                    if let Partitions::Range { start, end } = &parts
                        && (!start.admits_from(&p.key) || !end.admits_to(&p.key))
                    {
                        continue;
                    }
                    0
                }
            };
            if let Some(s) = state
                && wanted.is_none()
            {
                let resume_key =
                    PartitionKey::new(crate::storage::token_of(&s.key, d), s.key.clone());
                if local {
                    // Key order: everything up to the last partition returned was sent.
                    if !resumed {
                        if p.key != resume_key {
                            continue;
                        }
                        resumed = true;
                        if s.clustering.is_none() {
                            continue;
                        }
                    }
                } else if p.key < resume_key || (p.key == resume_key && s.clustering.is_none()) {
                    continue;
                }
            }
            let mut f = clustering.clone();
            if let Some(s) = state
                && s.key == p.key.key
                && let Some(c) = &s.clustering
            {
                f = filter_after(&*cmp, &f, c, plan.reversed);
            }
            let mut rows: Vec<Row> = p
                .rows
                .into_iter()
                .filter(|r| filter_contains(&*cmp, &f, &r.clustering))
                .collect();
            if plan.reversed {
                rows.reverse();
            }
            key_order.push((
                pos,
                PartitionChunk {
                    key: p.key,
                    partition_deletion: p.partition_deletion,
                    static_row: p.static_row,
                    rows,
                    markers: Vec::new(),
                    first: true,
                    last: true,
                },
            ));
        }
        if wanted.is_some() {
            key_order.sort_by_key(|(i, _)| *i);
            if let Some(s) = state {
                let first = if s.clustering.is_some() {
                    s.key_index
                } else {
                    s.key_index + 1
                };
                key_order.retain(|(i, _)| *i as u64 >= first);
            }
        }
        chunks.extend(key_order.into_iter().map(|(i, c)| (i as u64, c)));
        return Ok(Source::Memory(chunks, 0));
    }
    let st = db.storage_table(t)?;
    if !plan.bypass_cache {
        crate::mutation_fragments::note_read(db, t, &parts);
    }
    match parts {
        Partitions::Keys(mut keys) => {
            keys.sort_by(|a, b| compare_keys(t, a, b));
            keys.dedup_by(|a, b| compare_keys(t, a, b).is_eq());
            let start = state
                .map(|s| {
                    if s.clustering.is_some() {
                        s.key_index as usize
                    } else {
                        s.key_index as usize + 1
                    }
                })
                .unwrap_or(0);
            let mut reads = VecDeque::new();
            for (i, k) in keys.iter().enumerate().skip(start) {
                let key = storage::partition_key(k, d, false)?;
                // Cassandra intersects `token(...)` restrictions with the keys: a key outside
                // them isn't read.
                if !d.is_scylla() && !token_admits(&plan.restrictions, cx, key.token) {
                    continue;
                }
                // Only a view row can have an empty partition key.
                if key.key.is_empty() && d.is_scylla() && !t.is_view() {
                    continue;
                }
                let mut f = clustering.clone();
                let mut l = limits;
                if let Some(s) = state
                    && i as u64 == s.key_index
                    && s.clustering.is_some()
                {
                    if let Some(c) = &s.clustering {
                        f = filter_after(&*cmp, &f, c, plan.reversed);
                    }
                    if l.per_partition != u64::MAX {
                        l.per_partition = l.per_partition.saturating_sub(s.in_partition);
                    }
                }
                reads.push_back((
                    i as u64,
                    PartitionRead {
                        key,
                        clustering: f,
                        reversed: plan.reversed,
                        columns: columns.clone(),
                        limits: l,
                        options,
                    },
                ));
            }
            if warn.as_ref().is_some_and(|w| w.storage_limited) {
                for (_, r) in reads.iter_mut() {
                    r.limits.count_static_only = plan.static_content_rows || plan.distinct;
                }
            }
            Ok(Source::Keys {
                table: st,
                reads,
                current: None,
                index: 0,
                track: warn.map(|w| Box::new(Tracker::new(w))),
                room,
            })
        }
        Partitions::Range { start, end } => {
            let resume = state.map(|s| Resume {
                key: PartitionKey::new(crate::storage::token_of(&s.key, d), s.key.clone()),
                after: s.clustering.clone(),
                returned_in_partition: s.in_partition,
            });
            let mut track = warn.map(|w| Box::new(Tracker::new(w)));
            let mut limits = limits;
            let mut command = None;
            if let Some(t) = &track {
                if t.query.storage_limited {
                    limits.rows = room;
                    limits.count_static_only = plan.static_content_rows || plan.distinct;
                }
                // A later page starts at the last key (`PartitionRangeQueryPager.makeKeyBounds`):
                // included when the page ended inside it with rows left for it.
                let start = match (state, &resume) {
                    (Some(s), Some(r)) => KeyBound::Key {
                        key: r.key.clone(),
                        inclusive: s.clustering.is_some()
                            && per_partition.is_none_or(|p| s.in_partition < p),
                    },
                    _ => start.clone(),
                };
                command = Some(read_warn::Command {
                    place: read_warn::Place::Range { start, end: end.clone() },
                    clustering: clustering.clone(),
                    reversed: plan.reversed,
                    room,
                    groups_done: None,
                });
            }
            let r = RangeRead {
                start,
                end,
                resume,
                clustering,
                reversed: plan.reversed,
                columns,
                limits,
                options,
            };
            let s = st.scan(r);
            if let (Some(t), Some(c)) = (track.as_deref_mut(), command) {
                t.start(&s.metrics, c);
            }
            Ok(Source::Range(s, track))
        }
    }
}

/// Whether `token` satisfies the `token(...)` restrictions of an `=` / `IN` query.
fn token_admits(r: &Restrictions, cx: &EvalCtx<'_>, token: i64) -> bool {
    let Some(tp) = &r.token else { return true };
    let v = |t: &Term| match t.eval(cx) {
        Ok(Bound::Val(b)) => <[u8; 8]>::try_from(&b[..]).ok().map(i64::from_be_bytes),
        _ => None,
    };
    if let Some(x) = tp.eq.as_ref().and_then(v)
        && token != x
    {
        return false;
    }
    if let Some((t, incl)) = &tp.lo
        && let Some(x) = v(t)
        && (if *incl { token < x } else { token <= x })
    {
        return false;
    }
    if let Some((t, incl)) = &tp.hi
        && let Some(x) = v(t)
        && (if *incl { token > x } else { token >= x })
    {
        return false;
    }
    true
}

/// Builds the row view of a stored row.
#[allow(clippy::too_many_arguments)]
fn row_values(
    t: &Table,
    plan: &SelectPlan,
    key: &[Option<Bytes>],
    row: Option<&Row>,
    static_row: Option<&Row>,
    now_s: i64,
    values: &mut Vec<Option<Bytes>>,
    meta: &mut Vec<CellMeta>,
) {
    values.clear();
    values.resize(t.columns.len(), None);
    let want_meta = !plan.selection.meta_columns.is_empty();
    if want_meta {
        meta.clear();
        meta.resize(t.columns.len(), CellMeta::default());
    }
    for (pos, &ci) in t.partition_key.iter().enumerate() {
        values[ci] = key.get(pos).cloned().flatten();
    }
    if let Some(r) = row {
        for (pos, &ci) in t.clustering.iter().enumerate() {
            values[ci] = r.clustering.get(pos);
        }
    }
    for &ci in &plan.needed {
        let col = &t.columns[ci];
        if col.is_primary_key() {
            continue;
        }
        let src = if col.is_static() { static_row } else { row };
        let Some(src) = src else { continue };
        let m = if want_meta && plan.selection.meta_columns.contains(&ci) {
            Some(&mut meta[ci])
        } else {
            None
        };
        values[ci] = storage::column_value(t, col, src, now_s, m);
    }
}

/// The partition currently being read.
struct Current {
    key: PartitionKey,
    key_vals: Vec<Option<Bytes>>,
    static_row: Option<Row>,
    had_rows: bool,
    in_partition: u64,
    key_index: u64,
}

/// Reads one page.
#[allow(clippy::type_complexity)]
async fn page(
    db: &Arc<Database>,
    plan: &SelectPlan,
    cx: &EvalCtx<'_>,
    user_limit: Option<u64>,
    ppl: Option<u64>,
    page_size: Option<u64>,
    state: Option<PagingState>,
) -> Result<(Vec<Vec<Option<Bytes>>>, (Option<Bytes>, Vec<String>))> {
    let t = &plan.table;
    let mut filter = plan.restrictions.filter(t, cx)?;
    if let Some(ix) = &plan.index {
        ix.adjust_filter(&mut filter)?;
    }
    let mut remaining = match &state {
        Some(s) => s.remaining,
        None => user_limit.unwrap_or(u64::MAX),
    };
    // Cassandra pages a read whose limit exceeds the page (`SelectStatement.execute`); each
    // command may return what is left of the page.
    let paged = page_size.filter(|ps| user_limit.is_none_or(|l| l > *ps));
    let page_room = paged.map_or(remaining, |ps| remaining.min(ps));
    let ppl_text = if plan.distinct { Some(1) } else { ppl };
    let limits = match paged {
        Some(_) => read_warn::LimitMode::Paged { per_partition: ppl_text },
        None => read_warn::LimitMode::Fixed { rows: user_limit, per_partition: ppl_text },
    };
    let warn = read_warn::Query::new(db.settings().tombstone_warn_threshold, plan, cx, limits);
    let mut src = open_source(db, plan, cx, state.as_ref(), ppl, page_size, warn, page_room).await?;
    let keys_mode = matches!(src, Source::Keys { .. });
    // The row filter's static columns, checked once per partition (`RowFilter.CQLFilter`).
    let static_filter = RowFilter {
        preds: filter
            .preds
            .iter()
            .filter(|(target, _)| matches!(target, restrictions::Target::Column(ci) if t.columns[*ci].is_static()))
            .cloned()
            .collect(),
        multi: Vec::new(),
    };
    // An index read paged by index entries (ScyllaDB) decides where pages end; PER PARTITION
    // LIMIT doesn't apply to it.
    let entry_next = match &src {
        Source::Index(_, next) => next.clone(),
        _ => None,
    };
    let entry_paged = entry_next.is_some();
    let per_partition = if entry_paged {
        u64::MAX
    } else {
        ppl.unwrap_or(u64::MAX)
    };
    let mut last: Option<(Bytes, Option<Clustering>)> = state
        .as_ref()
        .map(|s| (s.key.clone(), s.clustering.clone()));
    let now = cx.now.unwrap_or_else(|| db.now_secs());
    let mut out: Vec<Vec<Option<Bytes>>> = Vec::new();
    let mut values = Vec::new();
    let mut meta = Vec::new();
    let mut cur: Option<Current> = None;
    let resume_in_partition = state
        .as_ref()
        .filter(|s| s.clustering.is_some())
        .map(|s| (s.key.clone(), s.in_partition));
    // ScyllaDB also ends a page by its size.
    let byte_limit = if cx.dialect.is_scylla() {
        crate::limits::SCYLLA_PAGE_BYTES
    } else {
        u64::MAX
    };
    let mut bytes = 0u64;
    let full = |n: usize, bytes: u64| {
        !entry_paged && page_size.is_some_and(|ps| n as u64 >= ps || bytes >= byte_limit)
    };
    let size = |row: &[Option<Bytes>]| -> u64 {
        row.iter()
            .map(|v| v.as_ref().map_or(0, |b| b.len() as u64))
            .sum()
    };
    let encode = |c: &Current, clustering: Option<Clustering>, remaining: u64| -> Bytes {
        PagingState {
            key: c.key.key.clone(),
            in_partition: if clustering.is_some() {
                c.in_partition
            } else {
                0
            },
            clustering,
            remaining,
            key_index: if plan.restrictions.key_mode == KeyMode::Keys {
                c.key_index
            } else {
                0
            },
            returned: 0,
            index: None,
        }
        .encode()
    };
    // The paging state of a read cut at the tombstone page limit after a whole partition.
    let mut cut_state: Option<PagingState> = None;
    loop {
        src.set_room(page_room.saturating_sub(out.len() as u64));
        let chunk = src.next().await?;
        if chunk.is_none()
            && let Some(cut) = src.page_cut()
        {
            let in_partition = cur
                .as_ref()
                .filter(|c| c.key == cut.key)
                .map_or(0, |c| c.in_partition);
            let after = cut_position(t, &cut, plan.reversed);
            let st = PagingState {
                key: cut.key.key.clone(),
                in_partition: if after.is_some() { in_partition } else { 0 },
                clustering: after.clone(),
                remaining,
                key_index: if plan.restrictions.key_mode == KeyMode::Keys {
                    src.key_index()
                } else {
                    0
                },
                returned: 0,
                index: None,
            };
            // Cut inside a partition: it isn't over (no static-only row).
            if after.is_some() || remaining == 0 {
                return Ok((out, ((remaining > 0).then(|| st.encode()), src.warnings())));
            }
            cut_state = Some(st);
        }
        let same = matches!((&chunk, &cur), (Some(ch), Some(c)) if ch.key == c.key);
        if !same {
            // Close the previous partition: a static-only partition is one row.
            if let Some(c) = cur.take()
                && !c.had_rows
                && (plan.static_content_rows || plan.distinct)
                && let Some(s) = &c.static_row
                && storage::row_is_live(t, s, now)
            {
                row_values(
                    t,
                    plan,
                    &c.key_vals,
                    None,
                    Some(s),
                    now,
                    &mut values,
                    &mut meta,
                );
                if filter.is_empty() || filter.matches(t, c.key.token, &|i| values[i].clone()) {
                    let rd = RowData {
                        values: &values,
                        meta: &meta,
                    };
                    out.push(plan.selection.row(&rd, cx, now)?);
                    bytes += out.last().map_or(0, |r| size(r));
                    last = Some((c.key.key.clone(), None));
                    remaining = remaining.saturating_sub(1);
                    if remaining == 0 {
                        src.stop_at_partition_start();
                        return Ok((out, (None, src.warnings())));
                    }
                    if full(out.len(), bytes) {
                        src.stop_at_partition_start();
                        return Ok((out, (Some(encode(&c, None, remaining)), src.warnings())));
                    }
                }
            }
            let Some(ch) = &chunk else { break };
            let in_partition = match &resume_in_partition {
                Some((k, n)) if *k == ch.key.key => *n,
                _ => 0,
            };
            if keys_mode && !static_filter.is_empty() && src.tracker().is_some() {
                let kv = storage::key_components(t, &ch.key.key);
                row_values(t, plan, &kv, None, ch.static_row.as_ref(), now, &mut values, &mut meta);
                if !static_filter.matches(t, ch.key.token, &|i| values[i].clone()) {
                    src.stop_after_static();
                }
            }
            cur = Some(Current {
                key: ch.key.clone(),
                key_vals: storage::key_components(t, &ch.key.key),
                static_row: ch.static_row.clone(),
                // Resumed after some of its rows: not a static-only partition.
                had_rows: matches!(&resume_in_partition, Some((k, _)) if *k == ch.key.key),
                in_partition,
                key_index: src.key_index(),
            });
        }
        let Some(ch) = chunk else { break };
        let Some(c) = cur.as_mut() else { break };
        if ch.static_row.is_some() {
            c.static_row = ch.static_row.clone();
        }
        for (ri, r) in ch.rows.iter().enumerate() {
            if c.in_partition >= per_partition || (plan.distinct && c.had_rows) {
                break;
            }
            if !t.dropped.is_empty() && !storage::row_is_live(t, r, now) {
                continue;
            }
            row_values(
                t,
                plan,
                &c.key_vals,
                Some(r),
                c.static_row.as_ref(),
                now,
                &mut values,
                &mut meta,
            );
            if !filter.is_empty() && !filter.matches(t, ch.key.token, &|i| values[i].clone()) {
                continue;
            }
            c.had_rows = true;
            let rd = RowData {
                values: &values,
                meta: &meta,
            };
            out.push(plan.selection.row(&rd, cx, now)?);
            bytes += out.last().map_or(0, |r| size(r));
            last = Some((ch.key.key.clone(), Some(r.clustering.clone())));
            c.in_partition += 1;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                src.stop_at(ri);
                return Ok((out, (None, src.warnings())));
            }
            // Cassandra's pager knows the last partition is done once its limit is reached
            // (`SinglePartitionPager.isExhausted`).
            if full(out.len(), bytes)
                && !cx.dialect.is_scylla()
                && keys_mode
                && src.last_key()
                && (c.in_partition >= per_partition || plan.distinct)
            {
                src.stop_at(ri);
                return Ok((out, (None, src.warnings())));
            }
            if full(out.len(), bytes) {
                let clustering = if plan.distinct {
                    None
                } else {
                    Some(r.clustering.clone())
                };
                let st = encode(c, clustering, remaining);
                src.stop_at(ri);
                return Ok((out, (Some(st), src.warnings())));
            }
            // The partition's command ends at its limit (`DataLimits` stops it there).
            if keys_mode && (c.in_partition >= per_partition || plan.distinct) {
                src.stop_at(ri);
            }
        }
    }
    if let Some(st) = cut_state {
        return Ok((out, (Some(st.encode()), src.warnings())));
    }
    // An entry-paged index read ended its page: resume at its position.
    if let Some(next) = entry_next
        && let Some(pos) = next.lock().take()
    {
        let (key, clustering) = last.unwrap_or_default();
        let st = PagingState {
            key,
            clustering,
            remaining,
            index: Some(pos),
            ..Default::default()
        };
        return Ok((out, (Some(st.encode()), src.warnings())));
    }
    Ok((out, (None, src.warnings())))
}

/// Where the next page resumes after a read cut at the tombstone page limit: after the cut's
/// position, or, for a cut just before a clustering whose last component is an integer, after
/// the value next to it in query order (the exact resume point); otherwise after the last
/// position before it.
fn cut_position(
    t: &Table,
    cut: &nosql_storage::read::PageCut,
    reversed: bool,
) -> Option<Clustering> {
    let Some(before) = &cut.before else {
        return cut.after.clone();
    };
    let exact = (|| {
        let parts: Vec<&[u8]> = before.components().collect();
        if parts.len() != t.clustering.len() {
            return None;
        }
        let col = t.clustering_columns().last()?;
        // One step back in query order, in storage order.
        let back: i64 = if reversed != col.reversed { 1 } else { -1 };
        let last = parts.last()?;
        let stepped: Vec<u8> = match (&col.ty, last.len()) {
            (CqlType::Int, 4) => i32::from_be_bytes((*last).try_into().ok()?)
                .checked_add(back as i32)?
                .to_be_bytes()
                .to_vec(),
            (CqlType::Bigint | CqlType::Timestamp | CqlType::Time, 8) => {
                i64::from_be_bytes((*last).try_into().ok()?)
                    .checked_add(back)?
                    .to_be_bytes()
                    .to_vec()
            }
            (CqlType::Smallint, 2) => i16::from_be_bytes((*last).try_into().ok()?)
                .checked_add(back as i16)?
                .to_be_bytes()
                .to_vec(),
            (CqlType::Tinyint, 1) => i8::from_be_bytes((*last).try_into().ok()?)
                .checked_add(back as i8)?
                .to_be_bytes()
                .to_vec(),
            (CqlType::Date, 4) => u32::from_be_bytes((*last).try_into().ok()?)
                .checked_add_signed(back as i32)?
                .to_be_bytes()
                .to_vec(),
            _ => return None,
        };
        let mut c: Vec<&[u8]> = parts[..parts.len() - 1].to_vec();
        c.push(&stepped);
        Some(Clustering::new(c))
    })();
    exact.or_else(|| cut.after.clone())
}

/// Collects the whole (bounded) result: aggregates, groups or post-ordered rows.
// `flush_group!` resets the group state even after the last group.
#[allow(unused_assignments)]
#[allow(clippy::too_many_arguments)]
async fn collect(
    db: &Arc<Database>,
    plan: &SelectPlan,
    cx: &EvalCtx<'_>,
    user_limit: Option<u64>,
    ppl: Option<u64>,
    page_size: Option<u64>,
    returned_groups: u64,
) -> Result<Out> {
    let t = &plan.table;
    let d = cx.dialect;
    let mut filter: RowFilter = plan.restrictions.filter(t, cx)?;
    if let Some(ix) = &plan.index {
        ix.adjust_filter(&mut filter)?;
    }
    // With GROUP BY, PER PARTITION LIMIT counts groups (`DataLimits.groupByLimits`), not rows.
    let group_ppl = match plan.group_by {
        Some(_) => ppl.unwrap_or(u64::MAX),
        None => u64::MAX,
    };
    let ppl = if plan.group_by.is_some() { None } else { ppl };
    // Cassandra's commands: aggregates are paged internally by the page size (one unpaged
    // command without one), GROUP BY by the page size or 10000 rows and stops at the page's
    // group limit, post-ordered IN reads have no row limit.
    let aggregate = plan.selection.is_aggregate() || plan.group_by.is_some();
    let (limits, sub_page, page_groups) = if plan.group_by.is_some() {
        let rows = page_size.unwrap_or(10_000);
        let left = user_limit.map(|l| l.saturating_sub(returned_groups));
        let groups = match page_size {
            Some(ps) => Some(left.map_or(ps, |l| l.min(ps))),
            None => left,
        };
        let per_partition = (group_ppl != u64::MAX).then_some(group_ppl);
        (read_warn::LimitMode::Group { groups, per_partition, rows }, Some(rows), groups)
    } else if aggregate {
        match page_size {
            Some(ps) => (read_warn::LimitMode::Paged { per_partition: None }, Some(ps), None),
            None => (read_warn::LimitMode::Empty, None, None),
        }
    } else {
        (read_warn::LimitMode::Fixed { rows: None, per_partition: ppl }, None, None)
    };
    // Aggregates read every row: storage isn't limited.
    let warn = read_warn::Query::new(db.settings().tombstone_warn_threshold, plan, cx, limits).map(
        |mut w| {
            w.storage_limited = false;
            w
        },
    );
    // A later GROUP BY page counts from the last row of the groups earlier pages returned, and
    // ends after its groups (`page_end`).
    let page_end = page_groups.map(|g| returned_groups + g);
    let mut page_started = returned_groups == 0;
    let mut remembered: Option<(PartitionKey, Clustering)> = None;
    let sub_room = sub_page.unwrap_or(u64::MAX);
    let mut src = open_source(db, plan, cx, None, ppl, None, warn, sub_room).await?;
    let keys_mode = matches!(src, Source::Keys { .. });
    let t_cmp = t.comparator.clone();
    // Rows counted in the current internal page, groups started.
    let mut sub_counted = 0u64;
    let mut groups_started = 0u64;
    let now = cx.now.unwrap_or_else(|| db.now_secs());
    let per_partition = ppl.unwrap_or(u64::MAX);
    let mut partition_groups = 0u64;
    let mut out = Out {
        rows: Vec::new(),
        sort: Vec::new(),
        warnings: Vec::new(),
    };
    let mut values = Vec::new();
    let mut meta = Vec::new();
    let mut groups_left = user_limit.unwrap_or(u64::MAX);
    let mut group_key: Option<Vec<Option<Bytes>>> = None;
    let mut aggs = plan.selection.new_group();
    let mut first_row: Option<(Vec<Option<Bytes>>, Vec<CellMeta>)> = None;
    let mut any_row = false;
    let mut current: Option<PartitionKey> = None;
    let mut current_static: Option<Row> = None;
    let mut key_vals: Vec<Option<Bytes>> = Vec::new();
    let mut in_partition = 0u64;
    let mut partition_index = 0usize;
    let mut position = 0usize;
    let mut partition_had_rows = false;
    let mut rows_left = user_limit.unwrap_or(u64::MAX);
    let group_cols: Vec<usize> = match plan.group_by {
        Some(n) => t
            .partition_key
            .iter()
            .chain(t.clustering.iter())
            .take(n)
            .copied()
            .collect(),
        None => Vec::new(),
    };
    macro_rules! flush_group {
        () => {
            if let Some((fv, fm)) = first_row.take() {
                let rd = RowData {
                    values: &fv,
                    meta: &fm,
                };
                out.rows
                    .push(plan.selection.finish(&aggs, Some(&rd), cx, now)?);
                aggs = plan.selection.new_group();
                groups_left = groups_left.saturating_sub(1);
            }
        };
    }
    // Handles one row (as values) for every mode.
    macro_rules! handle_row {
        ($token:expr) => {{
            if !filter.is_empty() && !filter.matches(t, $token, &|i| values[i].clone()) {
                false
            } else {
                if aggregate {
                    if plan.group_by.is_some() {
                        let k: Vec<Option<Bytes>> =
                            group_cols.iter().map(|&c| values[c].clone()).collect();
                        let mut skip = false;
                        if group_key.as_ref() != Some(&k) {
                            if partition_groups >= group_ppl {
                                skip = true;
                            } else {
                                flush_group!();
                                group_key = Some(k);
                                partition_groups += 1;
                                groups_started += 1;
                            }
                        }
                        if skip || groups_left == 0 {
                            false
                        } else {
                            if first_row.is_none() {
                                first_row = Some((values.clone(), meta.clone()));
                            }
                            let rd = RowData {
                                values: &values,
                                meta: &meta,
                            };
                            plan.selection.accumulate(&mut aggs, &rd, cx, now)?;
                            true
                        }
                    } else {
                        if first_row.is_none() {
                            first_row = Some((values.clone(), meta.clone()));
                        }
                        any_row = true;
                        let rd = RowData {
                            values: &values,
                            meta: &meta,
                        };
                        plan.selection.accumulate(&mut aggs, &rd, cx, now)?;
                        true
                    }
                } else {
                    let rd = RowData {
                        values: &values,
                        meta: &meta,
                    };
                    out.rows.push(plan.selection.row(&rd, cx, now)?);
                    if let Some(cols) = &plan.post_order {
                        let k: Vec<Option<Bytes>> = cols
                            .iter()
                            .map(|&p| values[t.clustering[p]].clone())
                            .collect();
                        out.sort.push((k, partition_index, position));
                    }
                    position += 1;
                    rows_left = rows_left.saturating_sub(1);
                    true
                }
            }
        }};
    }
    loop {
        src.set_room(sub_room.saturating_sub(sub_counted));
        let chunk = src.next().await?;
        let new_partition = match &chunk {
            Some(c) => current.as_ref() != Some(&c.key),
            None => true,
        };
        if new_partition {
            if current.take().is_some()
                && !partition_had_rows
                && (plan.static_content_rows || plan.distinct)
                && let Some(s) = current_static.take()
                && storage::row_is_live(t, &s, now)
            {
                row_values(
                    t,
                    plan,
                    &key_vals,
                    None,
                    Some(&s),
                    now,
                    &mut values,
                    &mut meta,
                );
                handle_row!(0);
            }
            let Some(c) = &chunk else { break };
            // The page's last group ended with the partition before.
            if plan.group_by.is_some()
                && page_started
                && page_end.is_some_and(|e| groups_started >= e)
                && let Some(t) = src.tracker()
            {
                t.halt_at_partition_start();
            }
            current = Some(c.key.clone());
            current_static = c.static_row.clone();
            key_vals = storage::key_components(t, &c.key.key);
            in_partition = 0;
            partition_groups = 0;
            partition_had_rows = false;
            partition_index += 1;
            // Post-ordered rows are limited after the sort.
            if (!aggregate && rows_left == 0 && plan.post_order.is_none())
                || (plan.group_by.is_some() && groups_left == 0)
            {
                break;
            }
        }
        let Some(c) = chunk else { break };
        for (ri, r) in c.rows.iter().enumerate() {
            if in_partition >= per_partition {
                break;
            }
            if !t.dropped.is_empty() && !storage::row_is_live(t, r, now) {
                continue;
            }
            row_values(
                t,
                plan,
                &key_vals,
                Some(r),
                current_static.as_ref(),
                now,
                &mut values,
                &mut meta,
            );
            // The first row of a group past the partition's group limit.
            let skipped_groups = plan.group_by.is_some()
                && partition_groups >= group_ppl
                && group_key.as_ref().is_some_and(|g| {
                    group_cols.iter().zip(g.iter()).any(|(&ci, v)| values[ci] != *v)
                });
            let counted = handle_row!(c.key.token);
            if counted {
                in_partition += 1;
                partition_had_rows = true;
            }
            if let Some(t) = src.tracker() {
                if plan.group_by.is_some() && groups_started <= returned_groups {
                    // A group an earlier page returned.
                    if counted {
                        t.remember(ri);
                        remembered = Some((c.key.clone(), r.clustering.clone()));
                    }
                } else {
                    if !page_started {
                        page_started = true;
                        sub_counted = 0;
                        if let Some(cmd) = t.command() {
                            let next = match (&remembered, t.remembered_here()) {
                                (Some((k, after)), true) => read_warn::Command {
                                    place: match &cmd.place {
                                        read_warn::Place::Key(k) => read_warn::Place::Key(k.clone()),
                                        read_warn::Place::Range { end, .. } => read_warn::Place::Range {
                                            start: KeyBound::Key { key: k.clone(), inclusive: true },
                                            end: end.clone(),
                                        },
                                    },
                                    clustering: match &cmd.place {
                                        read_warn::Place::Key(_) => {
                                            filter_after(&*t_cmp, &cmd.clustering, after, plan.reversed)
                                        }
                                        read_warn::Place::Range { .. } => cmd.clustering.clone(),
                                    },
                                    ..cmd.clone()
                                },
                                _ => cmd.clone(),
                            };
                            t.restart(next);
                        }
                    }
                    // GROUP BY stops at the first row past the page's groups, and a partition
                    // at the first row past its groups.
                    if page_end.is_some_and(|e| groups_started > e) {
                        t.halt_at(ri);
                    } else if keys_mode
                        && skipped_groups
                        && t.command().is_some_and(|cmd| matches!(&cmd.place, read_warn::Place::Key(k) if *k == c.key))
                    {
                        t.stop_at(ri);
                    } else if counted && sub_page.is_some() {
                        sub_counted += 1;
                        if Some(sub_counted) == sub_page
                            && let Some(cmd) = t.command()
                        {
                            // The next internal page resumes after this row.
                            let next = read_warn::Command {
                                place: match &cmd.place {
                                    read_warn::Place::Key(k) => read_warn::Place::Key(k.clone()),
                                    read_warn::Place::Range { end, .. } => read_warn::Place::Range {
                                        start: KeyBound::Key { key: c.key.clone(), inclusive: true },
                                        end: end.clone(),
                                    },
                                },
                                clustering: match &cmd.place {
                                    read_warn::Place::Key(_) => {
                                        filter_after(&*t_cmp, &cmd.clustering, &r.clustering, plan.reversed)
                                    }
                                    read_warn::Place::Range { .. } => cmd.clustering.clone(),
                                },
                                reversed: cmd.reversed,
                                room: sub_room,
                                groups_done: plan
                                    .group_by
                                    .map(|_| groups_started.saturating_sub(1 + returned_groups)),
                            };
                            t.split_at(ri, next);
                            sub_counted = 0;
                        }
                    }
                }
            }
            if !aggregate && rows_left == 0 && plan.post_order.is_none() {
                break;
            }
        }
    }
    out.warnings = src.warnings();
    if aggregate {
        if plan.group_by.is_some() {
            if groups_left > 0 {
                flush_group!();
            }
        } else {
            let first = first_row.take();
            let rd = first.as_ref().map(|(v, m)| RowData { values: v, meta: m });
            out.rows
                .push(plan.selection.finish(&aggs, rd.as_ref(), cx, now)?);
            let _ = any_row;
        }
        return Ok(out);
    }
    // Post-query ordering of IN queries.
    if plan.post_order.is_some() {
        let cmp = t.comparator.clone();
        let positions = plan.post_order.clone().unwrap_or_default();
        let types: Vec<(CqlType, bool)> = positions
            .iter()
            .map(|&p| {
                (
                    t.columns[t.clustering[p]].ty.clone(),
                    t.columns[t.clustering[p]].reversed,
                )
            })
            .collect();
        let _ = cmp;
        let key_cmp = |a: &Vec<Option<Bytes>>, b: &Vec<Option<Bytes>>| -> std::cmp::Ordering {
            for (i, (ty, rev)) in types.iter().enumerate() {
                let o = match (&a[i], &b[i]) {
                    (Some(x), Some(y)) => nosql_types::compare_with_order(ty, *rev, x, y),
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, _) => std::cmp::Ordering::Less,
                    (_, None) => std::cmp::Ordering::Greater,
                };
                if o.is_ne() {
                    return o;
                }
            }
            std::cmp::Ordering::Equal
        };
        let mut idx: Vec<usize> = (0..out.rows.len()).collect();
        match d {
            Dialect::Cassandra => {
                idx.sort_by(|&a, &b| {
                    let o = key_cmp(&out.sort[a].0, &out.sort[b].0);
                    if plan.reversed { o.reverse() } else { o }
                });
            }
            Dialect::Scylla => {
                // ScyllaDB orders ascending (rows of a partition in clustering order) and reverses.
                idx.sort_by(|&a, &b| {
                    key_cmp(&out.sort[a].0, &out.sort[b].0)
                        .then_with(|| out.sort[a].1.cmp(&out.sort[b].1))
                        .then_with(|| {
                            let o = out.sort[a].2.cmp(&out.sort[b].2);
                            if plan.reversed { o.reverse() } else { o }
                        })
                });
                if plan.reversed {
                    idx.reverse();
                }
            }
        }
        let mut rows: Vec<Option<Vec<Option<Bytes>>>> = out.rows.into_iter().map(Some).collect();
        out.rows = idx.into_iter().filter_map(|i| rows[i].take()).collect();
        if let Some(l) = user_limit {
            out.rows.truncate(l as usize);
        }
    }
    Ok(out)
}

/// Rows of a statement as text, for tests.
pub fn _ensure_send() {}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_storage::BytesComparator;

    #[test]
    fn filter_after_cuts_slices_and_names() {
        let c = |s: &str| Clustering::new([s.as_bytes()]);
        let all = ClusteringFilter::all();
        let f = filter_after(&BytesComparator, &all, &c("b"), false);
        assert!(!filter_contains(&BytesComparator, &f, &c("a")));
        assert!(!filter_contains(&BytesComparator, &f, &c("b")));
        assert!(filter_contains(&BytesComparator, &f, &c("c")));
        let f = filter_after(&BytesComparator, &all, &c("b"), true);
        assert!(filter_contains(&BytesComparator, &f, &c("a")));
        assert!(!filter_contains(&BytesComparator, &f, &c("b")));
        let names = ClusteringFilter::Names(vec![c("a"), c("b"), c("c")]);
        match filter_after(&BytesComparator, &names, &c("a"), false) {
            ClusteringFilter::Names(n) => assert_eq!(n, vec![c("b"), c("c")]),
            _ => unreachable!(),
        }
    }
}
