//! COMPACT STORAGE semantics (Cassandra 5 still runs them; ScyllaDB behind
//! `enable_create_table_with_compact_storage`), ported from Cassandra's `CreateTableStatement`,
//! `UpdateStatement`, `UpdateParameters`, `StatementRestrictions` and `CreateIndexStatement`
//! (Apache-2.0):
//!
//! - a compact table's rows have no row marker: a row exists while its value is set (setting it
//!   to null, or deleting it, removes the row);
//! - a dense table (clustering columns, at most one value column) can be written with a prefix
//!   of its clustering columns (CASSANDRA-7990); those rows read with nulls for the rest;
//! - its value is mandatory (unless the table has none), and the clustering value of a
//!   single-clustering dense table can't be empty;
//! - a static compact table (no clustering) has a hidden clustering column (`column1`): a write
//!   of the key alone misses it;
//! - CREATE TABLE refuses non-frozen collections and UDTs, static columns, several value columns
//!   and none; indexes on the primary key or on a dense table's value are refused.
//!
//! roll's model keeps the CQL view of these tables: a static compact table's columns are
//! regular, the hidden columns are left out (`crate::schema::persist`).

use crate::error::{Error, Result};
use crate::schema::{ColumnKind, Table};

/// A COMPACT STORAGE table (not a view).
pub fn is_compact(t: &Table) -> bool {
    t.view.is_none() && !t.flags.compound && !t.flags.super_
}

/// A compact table with clustering columns.
pub fn is_dense(t: &Table) -> bool {
    is_compact(t) && t.flags.dense
}

/// A compact table without clustering columns.
pub fn is_static_compact(t: &Table) -> bool {
    is_compact(t) && !t.flags.dense
}

/// The value column of a dense table (`None`: it has only key columns).
pub fn value_column(t: &Table) -> Option<usize> {
    is_dense(t)
        .then(|| t.columns.iter().position(|c| !c.is_primary_key()))
        .flatten()
}

/// Whether an INSERT writes a row marker: not into a compact table, except a dense one without
/// a value (whose rows are only their key).
pub fn insert_marks_rows(t: &Table) -> bool {
    !is_compact(t) || (is_dense(t) && value_column(t).is_none())
}

/// A write of a compact table that sets no regular column: the value is mandatory (dense
/// tables), or the hidden clustering column is missing (static compact tables).
pub fn check_sets_value(t: &Table, sets_regular: bool) -> Result<()> {
    if !is_compact(t) || sets_regular {
        return Ok(());
    }
    if is_static_compact(t) {
        let hidden = crate::schema::persist::static_compact_clustering(t).unwrap_or_default();
        return Err(Error::invalid(format!(
            "Some clustering keys are missing: {hidden}"
        )));
    }
    match value_column(t) {
        Some(v) => Err(Error::invalid(format!(
            "Column {} is mandatory for this COMPACT STORAGE table",
            t.columns[v].name
        ))),
        None => Ok(()),
    }
}

/// The clustering prefix of a dense table's INSERT (the values given, in order): `Ok(n)` when
/// the first `n` are given and the rest aren't; gaps fail as Cassandra's restrictions do.
pub fn insert_prefix(t: &Table, given: &[bool]) -> Result<usize> {
    let n = given.iter().take_while(|g| **g).count();
    if let Some(gap) = given.iter().skip(n).position(|g| *g) {
        let pos = n + gap;
        return Err(Error::invalid(format!(
            "PRIMARY KEY column \"{}\" cannot be restricted as preceding column \"{}\" is not restricted",
            t.columns[t.clustering[pos]].name,
            t.columns[t.clustering[n]].name
        )));
    }
    Ok(n)
}

/// `UpdateParameters.newRow`: a single-clustering dense table refuses an empty clustering.
pub fn check_clustering(t: &Table, c: &nosql_storage::Clustering) -> Result<()> {
    if is_dense(t) && t.clustering.len() == 1 && c.get(0).is_none_or(|v| v.is_empty()) {
        return Err(Error::invalid(format!(
            "Invalid empty or null value for column {}",
            t.columns[t.clustering[0]].name
        )));
    }
    Ok(())
}

/// `CreateTableStatement`'s COMPACT STORAGE checks (`columns`: the table's columns as
/// declared, `(name, kind, non-frozen multicell)`).
pub fn check_create(columns: &[(&str, ColumnKind, bool)], has_clustering: bool) -> Result<()> {
    if columns.iter().any(|(_, _, multicell)| *multicell) {
        return Err(Error::invalid(
            "Non-frozen collections and UDTs are not supported with COMPACT STORAGE",
        ));
    }
    if columns.iter().any(|(_, k, _)| *k == ColumnKind::Static) {
        return Err(Error::invalid(
            "Static columns are not supported in COMPACT STORAGE tables",
        ));
    }
    let regular: Vec<&str> = columns
        .iter()
        .filter(|(_, k, _)| *k == ColumnKind::Regular)
        .map(|(n, _, _)| *n)
        .collect();
    if has_clustering && regular.len() > 1 {
        return Err(Error::invalid(format!(
            "COMPACT STORAGE with composite PRIMARY KEY allows no more than one column not part of the PRIMARY KEY (got: {})",
            regular.join(", ")
        )));
    }
    if !has_clustering && regular.is_empty() {
        return Err(Error::invalid(
            "No definition found that is not part of the PRIMARY KEY",
        ));
    }
    Ok(())
}

/// `CreateIndexStatement`: no index on a compact table's primary key, nor on a dense table's
/// value.
pub fn check_index(t: &Table, column: usize) -> Result<()> {
    if !is_compact(t) {
        return Ok(());
    }
    if t.columns[column].is_primary_key() {
        return Err(Error::invalid(
            "Secondary indexes are not supported on PRIMARY KEY columns in COMPACT STORAGE tables",
        ));
    }
    if value_column(t) == Some(column) {
        return Err(Error::invalid(
            "Secondary indexes are not supported on compact value column of COMPACT STORAGE tables",
        ));
    }
    Ok(())
}
