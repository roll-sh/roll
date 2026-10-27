//! Lightweight transactions: `INSERT … IF NOT EXISTS`, `UPDATE` / `DELETE … IF EXISTS | IF
//! <conditions>`, conditional batches and reads at `SERIAL` / `LOCAL_SERIAL`.
//!
//! A database has a single writer, so Paxos needs no round trips: a conditional request takes
//! the partition's lock, reads the rows its conditions name, evaluates them and, when they hold,
//! applies every update at once — linearisable per partition, like a Paxos round with one
//! replica. What clients can observe still follows the references:
//!
//! - the write timestamp is the ballot's (`Commit.Proposal.of` rewrites every timestamp, also a
//!   member's `USING TIMESTAMP`; Cassandra's ballots are milliseconds × 1000, ScyllaDB's
//!   microseconds), and Cassandra moves partition and range deletions to `ballot - 1`;
//! - `SERIAL` / `LOCAL_SERIAL` are checked as serial and commit consistencies;
//! - the result: Cassandra (`ModificationStatement.buildCasResultSet`) returns `[applied]` alone
//!   on success and the rows it read on failure; ScyllaDB (`cas_request`, as documented in
//!   `docs/kb/lwt-differences.rst`) always returns the old row, one row per batch statement,
//!   with columns fixed at PREPARE;
//! - ScyllaDB extensions: `SET c = c ± v`, `LIKE` and `NOT IN` in conditions, subscripted sets,
//!   mixing `IF EXISTS` / `IF NOT EXISTS` / conditions on one row in a batch.
//!
//! Modules: [`prepare`] types `IF` clauses (called by `statements::modify`), [`eval`] evaluates
//! them, [`arith`] is `c = c ± v`, and `exec` runs requests. Conditional statements reach this
//! module through [`crate::hooks::Conditional`] ([`install`]).

pub mod arith;
pub mod eval;
mod exec;
pub mod prepare;

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use nosql_types::{CqlType, Dialect};

use crate::config::DbConfig;
use crate::error::{Consistency, Error, Result};
use crate::hooks::Hooks;
use crate::messages as msg;
use crate::prepare::terms::EvalCtx;
use crate::response::{ColumnSpec, QueryParams, ResultMetadata};
use crate::statements::modify::{Conditions, ModifyPlan};

/// Installs the conditional-statement hook.
pub fn install(hooks: &mut Hooks, _cfg: &DbConfig) {
    hooks.conditional = Some(Arc::new(Lwt::default()));
}

/// The [`crate::hooks::Conditional`] implementation of one database.
#[derive(Default)]
pub struct Lwt {
    /// The last Cassandra ballot (milliseconds × 1000, strictly increasing).
    last_ballot: AtomicI64,
}

/// `ConsistencyLevel.validateForWrite` / `validateCounterForWrite` at a serial consistency.
pub fn serial_write_error(counter: bool) -> Error {
    Error::invalid(if counter {
        "Counter operations are inherently non-serializable"
    } else {
        "You must use conditional updates for serializable writes"
    })
}

/// `validateForCas` (the serial consistency) then `validateForCasCommit` (the consistency).
pub fn check_consistency(params: &QueryParams) -> Result<()> {
    let serial = params.serial_consistency.unwrap_or(Consistency::Serial);
    if !serial.is_serial() {
        return Err(Error::invalid(
            "Invalid consistency for conditional update. Must be one of SERIAL or LOCAL_SERIAL",
        ));
    }
    if params.consistency.is_serial() {
        return Err(Error::invalid(format!(
            "{} is not supported as conditional update commit consistency. Use ANY if you mean \"make sure it is accepted but I don't care how many replicas commit it for non-SERIAL reads\"",
            params.consistency.name()
        )));
    }
    Ok(())
}

/// `BatchStatement.validate`: a conditional batch writes one table.
pub fn validate_batch(d: Dialect, statements: &[Arc<ModifyPlan>]) -> Result<()> {
    if !statements.iter().any(|s| s.conditional) {
        return Ok(());
    }
    let first = &statements[0].table;
    if statements
        .iter()
        .any(|s| s.table.keyspace != first.keyspace || s.table.name != first.name)
    {
        return Err(msg::lwt_batch_tables(d));
    }
    Ok(())
}

/// A ScyllaDB `INSERT … JSON … IF NOT EXISTS`, which ScyllaDB runs as a plain insert.
pub fn is_scylla_plain_json(d: Dialect, plan: &ModifyPlan) -> bool {
    d.is_scylla() && plan.json.is_some() && matches!(plan.conditions, Conditions::NotExists)
}

/// The columns ScyllaDB returns after `[applied]` (its PREPARED result metadata): every column
/// when a statement has `IF [NOT] EXISTS`, otherwise the conditioned columns (plus the primary
/// key in a batch), in schema order.
pub fn scylla_columns(members: &[&ModifyPlan], batch: bool) -> Vec<usize> {
    let Some(first) = members.first() else {
        return Vec::new();
    };
    let t = &first.table;
    let conditional = members.iter().filter(|m| m.conditional);
    let mut set = vec![false; t.columns.len()];
    for m in conditional {
        match &m.conditions {
            Conditions::NotExists | Conditions::Exists => return (0..t.columns.len()).collect(),
            Conditions::List(cs) => {
                for c in cs {
                    set[c.column] = true;
                }
            }
            Conditions::None => {}
        }
    }
    if batch {
        for &c in t.partition_key.iter().chain(&t.clustering) {
            set[c] = true;
        }
    }
    (0..t.columns.len()).filter(|&c| set[c]).collect()
}

/// The PREPARED result metadata of a conditional statement or batch (ScyllaDB only).
pub fn prepared_result(d: Dialect, members: &[&ModifyPlan], batch: bool) -> Option<ResultMetadata> {
    if !d.is_scylla() || !members.iter().any(|m| m.conditional) {
        return None;
    }
    let t = &members[0].table;
    let mut columns = vec![applied_spec(t)];
    columns.extend(
        scylla_columns(members, batch)
            .into_iter()
            .map(|c| column_spec(t, c)),
    );
    Some(ResultMetadata { columns })
}

pub(crate) fn applied_spec(t: &crate::schema::Table) -> ColumnSpec {
    ColumnSpec::new(&t.keyspace, &t.name, "[applied]", CqlType::Boolean)
}

pub(crate) fn column_spec(t: &crate::schema::Table, c: usize) -> ColumnSpec {
    ColumnSpec::new(
        &t.keyspace,
        &t.name,
        &t.columns[c].name,
        t.columns[c].ty.clone(),
    )
}

/// ScyllaDB reads at `SERIAL` / `LOCAL_SERIAL` one partition only (Cassandra 5's Paxos v2 reads
/// ranges and several partitions too).
pub fn check_serial_read(
    d: Dialect,
    plan: &crate::statements::select::SelectPlan,
    params: &QueryParams,
    cx: &EvalCtx,
) -> Result<()> {
    use crate::prepare::restrictions::{KeyMode, Partitions};
    if !params.consistency.is_serial() || !d.is_scylla() {
        return Ok(());
    }
    let one_partition = plan.restrictions.key_mode == KeyMode::Keys
        && match plan.restrictions.partitions(&plan.table, cx, None)? {
            Partitions::Keys(mut ks) => {
                ks.sort();
                ks.dedup();
                ks.len() <= 1
            }
            Partitions::Range { .. } => false,
        };
    if one_partition {
        return Ok(());
    }
    Err(Error::invalid(
        "SERIAL/LOCAL_SERIAL consistency may only be requested for one partition at a time",
    ))
}
