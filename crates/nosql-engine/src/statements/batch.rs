//! BATCH, as a CQL statement (`BEGIN … APPLY BATCH`) and as the protocol message
//! (`BatchStatement`, Apache-2.0): one timestamp for every statement, counter / logged rules, the
//! unlogged-multi-partition warning and the batch size thresholds (Cassandra), and a single
//! atomic write.

use std::collections::BTreeSet;
use std::sync::Arc;

use nosql_cql::ast::{
    self, BatchKind as AstKind, BatchStatement as AstStatement, Statement, StatementKind,
};
use nosql_storage::manifest::TableId;
use nosql_storage::{ColumnData, PartitionUpdate};
use nosql_types::{CqlType, Dialect};

use super::modify::{self, ModifyPlan};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::messages as msg;
use crate::prepare::terms::{Bound, EvalCtx, Term};
use crate::prepare::{PrepareCtx, Receiver};
use crate::response::{self, ColumnSpec, Param, QueryParams, Response};
use crate::schema::Schema;
use crate::session::SessionState;

/// A prepared batch.
pub struct BatchPlan {
    pub kind: AstKind,
    pub statements: Vec<Arc<ModifyPlan>>,
    pub timestamp: Option<Term>,
    /// ScyllaDB's `USING TIMEOUT` of the batch (its statements' own are ignored).
    pub timeout: Option<Term>,
    pub bind: Vec<ColumnSpec>,
    /// Partition key bind variables of the first statement's table (PREPARED metadata).
    pub pk_indexes: Vec<u16>,
    pub conditional: bool,
}

fn inner_kind(s: &AstStatement<'static>) -> StatementKind<'static> {
    match s {
        AstStatement::Insert(i) => StatementKind::Insert(Box::new(i.clone())),
        AstStatement::Update(u) => StatementKind::Update(Box::new(u.clone())),
        AstStatement::Delete(d) => StatementKind::Delete(Box::new(d.clone())),
    }
}

fn statement_has_timestamp(s: &AstStatement) -> bool {
    match s {
        AstStatement::Insert(i) => i.using.timestamp.is_some(),
        AstStatement::Update(u) => u.using.timestamp.is_some(),
        AstStatement::Delete(d) => d.using.timestamp.is_some(),
    }
}

/// Prepares `BEGIN … APPLY BATCH`.
pub fn prepare(
    db: &Arc<Database>,
    schema: &Arc<Schema>,
    b: &ast::Batch<'static>,
    stmt: &Statement<'static>,
    keyspace: Option<&str>,
) -> Result<BatchPlan> {
    let d = db.dialect();
    if b.using.ttl.is_some() {
        return Err(Error::invalid(
            "Global TTL on the BATCH statement is not supported.",
        ));
    }
    let mut cx = PrepareCtx::new(
        d,
        schema,
        keyspace.unwrap_or(""),
        &db.functions,
        &stmt.bind_markers,
    );
    cx.apply_settings(&db.settings());
    let uf = db.user_functions.read().clone();
    cx.user_functions = uf.as_deref();
    let timestamp = b
        .using
        .timestamp
        .as_ref()
        .map(|t| {
            crate::prepare::terms::prepare(
                t,
                &Receiver::new("", "", "[timestamp]", CqlType::Bigint),
                &mut cx,
            )
        })
        .transpose()?;
    let timeout = match &b.using.timeout {
        Some(t) => {
            crate::schema::ddl::check_timeout(&b.using, d)?;
            Some(crate::prepare::terms::prepare(
                t,
                &Receiver::new("", "", "[timeout]", CqlType::Duration),
                &mut cx,
            )?)
        }
        None => None,
    };
    let mut statements = Vec::with_capacity(b.statements.len());
    for s in &b.statements {
        let kind = inner_kind(s);
        let plan = modify::prepare_in(db, schema, &kind, keyspace, &mut cx)?;
        statements.push(Arc::new(plan));
    }
    let has_ts: Vec<bool> = b.statements.iter().map(statement_has_timestamp).collect();
    validate(db, b.kind, &statements, timestamp.is_some(), &has_ts, d)?;
    let conditional = statements.iter().any(|s| s.conditional);
    let pk_indexes = statements
        .first()
        .map(|s| cx.pk_indexes(&s.table))
        .unwrap_or_default();
    Ok(BatchPlan {
        kind: b.kind,
        statements,
        timestamp,
        timeout,
        bind: cx.bind_specs()?,
        pk_indexes,
        conditional,
    })
}

/// `BatchStatement.validate`.
fn validate(
    _db: &Database,
    kind: AstKind,
    statements: &[Arc<ModifyPlan>],
    batch_ts: bool,
    has_ts: &[bool],
    _d: Dialect,
) -> Result<()> {
    let conditional = statements.iter().any(|s| s.conditional);
    if batch_ts {
        if conditional {
            return Err(Error::invalid(
                "Cannot provide custom timestamp for conditional BATCH",
            ));
        }
        if kind == AstKind::Counter {
            return Err(Error::invalid(
                "Cannot provide custom timestamp for counter BATCH",
            ));
        }
    }
    let mut counters = false;
    let mut non_counters = false;
    for (s, ts) in statements.iter().zip(has_ts) {
        if batch_ts && *ts {
            return Err(Error::invalid(
                "Timestamp must be set either on BATCH or individual statements",
            ));
        }
        if s.table.is_counter() {
            counters = true;
        } else {
            non_counters = true;
        }
    }
    match kind {
        AstKind::Counter if non_counters => {
            return Err(Error::invalid(
                "Cannot include non-counter statement in a counter batch",
            ));
        }
        AstKind::Logged if counters => {
            return Err(Error::invalid(
                "Cannot include a counter statement in a logged batch",
            ));
        }
        AstKind::Unlogged if counters && non_counters => {
            return Err(Error::invalid(
                "Counter and non-counter mutations cannot exist in the same batch",
            ));
        }
        _ => {}
    }
    crate::lwt::validate_batch(_d, statements)
}

/// Executes a prepared CQL batch.
/// Boxed so the future is checked `Send` here, with named lifetimes (rust-lang/rust#110338).
pub fn execute<'a>(
    db: &'a Arc<Database>,
    plan: &'a BatchPlan,
    params: &'a QueryParams,
    stmt: &'a Statement<'static>,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_inner(db, plan, params, stmt))
}

async fn execute_inner(
    db: &Arc<Database>,
    plan: &BatchPlan,
    params: &QueryParams,
    stmt: &Statement<'static>,
) -> Result<Response> {
    let d = db.dialect();
    let cx = EvalCtx::new(&params.values, d);
    let mut ts = None;
    if let Some(t) = &plan.timestamp {
        match t.eval(&cx)? {
            Bound::Val(b) => ts = <[u8; 8]>::try_from(&b[..]).ok().map(i64::from_be_bytes),
            Bound::Null => return Err(Error::invalid("Invalid null value of timestamp")),
            Bound::Unset => {}
        }
    }
    if let Some(ts) = ts {
        modify::check_future_timestamp(db, ts)?;
    }
    let timeout = match &plan.timeout {
        Some(t) => crate::schema::ddl::check_timeout_value(t, &cx)?,
        None => None,
    };
    let items: Vec<(Arc<ModifyPlan>, Vec<Param>)> = plan
        .statements
        .iter()
        .map(|s| (s.clone(), params.values.clone()))
        .collect();
    run(db, plan.kind, &items, ts, timeout, params, Some(stmt)).await
}

/// Shared by both batch forms: builds every statement's updates with one timestamp, checks the
/// thresholds, applies atomically.
async fn run(
    db: &Arc<Database>,
    kind: AstKind,
    items: &[(Arc<ModifyPlan>, Vec<Param>)],
    batch_ts: Option<i64>,
    timeout: Option<nosql_types::Duration>,
    params: &QueryParams,
    stmt: Option<&Statement<'static>>,
) -> Result<Response> {
    let d = db.dialect();
    let guard_warnings = crate::guardrails::write_cl::check(db, params.consistency)?;
    if params.consistency.is_serial() && !items.iter().any(|(p, _)| p.conditional) && d.is_scylla()
    {
        return Err(crate::lwt::serial_write_error(
            items.iter().any(|(p, _)| p.table.is_counter()),
        ));
    }
    if items.iter().any(|(p, _)| p.conditional) {
        let Some(h) = db.hooks().conditional else {
            return Err(Error::invalid("Conditional updates are not supported yet"));
        };
        let members = items.iter().map(|(p, v)| (p.as_ref(), &v[..])).collect();
        let mut r = h
            .execute(
                db,
                crate::hooks::ConditionalRequest {
                    table: &items[0].0.table,
                    statement: stmt,
                    params,
                    members,
                    batch: true,
                },
            )
            .await?;
        let mut w = guard_warnings;
        w.append(&mut r.warnings);
        r.warnings = w;
        return Ok(r);
    }
    let timestamp = batch_ts
        .or(params.timestamp)
        .unwrap_or_else(|| db.timestamp());
    let mut all: Vec<(TableId, PartitionUpdate, Arc<crate::schema::Table>)> = Vec::new();
    for (plan, values) in items {
        let cx = EvalCtx::new(values, d);
        let a = modify::attrs(db, plan, params, &cx, Some(timestamp))?;
        let id = plan.table.table_id();
        let ups = if plan.table.is_counter() {
            let pending: Vec<PartitionUpdate> = all
                .iter()
                .filter(|(i, ..)| *i == id)
                .map(|(_, u, _)| u.clone())
                .collect();
            let ups = modify::updates_after(db, plan, &cx, &a, &pending).await?;
            for (_, earlier, _) in all.iter_mut().filter(|(i, ..)| *i == id) {
                for u in &ups {
                    modify::supersede_counters(earlier, u);
                }
            }
            ups
        } else {
            modify::updates(db, plan, &cx, &a).await?
        };
        for u in ups {
            all.push((id, u, plan.table.clone()));
        }
    }
    let mut warnings = Vec::new();
    // ScyllaDB fails a multi-partition batch over `batch_size_fail_threshold_in_kb` (it sends
    // no size warning). Its size is the mutations' memory footprint, which this approximates
    // with each update's data size plus a fixed per-mutation overhead.
    if d.is_scylla() {
        let partitions: BTreeSet<(TableId, Vec<u8>)> =
            all.iter().map(|(id, u, _)| (*id, u.key.key.to_vec())).collect();
        if partitions.len() > 1 {
            let size: u64 = all
                .iter()
                .map(|(_, u, _)| data_size(u) + SCYLLA_MUTATION_OVERHEAD)
                .sum();
            if size > db.settings().batch_size_fail_threshold {
                return Err(Error::invalid("Batch too large"));
            }
        }
    }
    if !d.is_scylla() {
        // Mutations are per (keyspace, partition key).
        let mutations: BTreeSet<(String, Vec<u8>)> = all
            .iter()
            .map(|(_, u, t)| (t.keyspace.clone(), u.key.key.to_vec()))
            .collect();
        if mutations.len() > 1 {
            let size: u64 = all.iter().map(|(_, u, _)| data_size(u)).sum();
            let warn = db.settings().batch_size_warn_threshold;
            if size > warn {
                if size > db.settings().batch_size_fail_threshold {
                    return Err(Error::invalid("Batch too large"));
                }
                let tables: BTreeSet<String> = all
                    .iter()
                    .map(|(_, _, t)| format!("{}.{}", t.keyspace, t.name))
                    .collect();
                let names: Vec<String> = tables.into_iter().collect();
                warnings.push(format!("Batch for [{}] is of size {size}, exceeding specified threshold of {warn} by {}.", names.join(", "), size - warn));
            }
        }
        if kind != AstKind::Logged && mutations.len() > 1 {
            let keys: BTreeSet<Vec<u8>> = all
                .iter()
                .map(|(id, u, _)| [id.0.to_be_bytes().to_vec(), u.key.key.to_vec()].concat())
                .collect();
            let limit = db
                .settings()
                .unlogged_batch_across_partitions_warn_threshold;
            if keys.len() as u64 > limit {
                let tables: BTreeSet<String> = all
                    .iter()
                    .map(|(_, _, t)| format!("{}.{}", t.keyspace, t.name))
                    .collect();
                let plural = if tables.len() == 1 { "" } else { "s" };
                let names: Vec<String> = tables.into_iter().collect();
                warnings.push(format!(
                    "Unlogged batch covering {} partitions detected against table{plural} [{}]. You should use a logged batch for atomicity, or asynchronous writes for performance.",
                    keys.len(),
                    names.join(", ")
                ));
            }
        }
    }
    // ScyllaDB's virtual tables take each statement's updates.
    if all.iter().any(|(_, _, t)| t.is_virtual()) {
        for (_, u, t) in &all {
            crate::guardrails::config_table::apply(
                db,
                t,
                std::slice::from_ref(u),
                params.consistency,
                false,
            )?;
        }
        return Ok(Response::void().with_warnings(guard_warnings));
    }
    let mut warnings = [guard_warnings, warnings].concat();
    let checked: Vec<(&crate::schema::Table, &PartitionUpdate)> =
        all.iter().map(|(_, u, t)| (&**t, u)).collect();
    let write = crate::guardrails::large_data::Write {
        consistency: params.consistency,
        write_type: match kind {
            AstKind::Logged => "BATCH",
            AstKind::Unlogged => "UNLOGGED_BATCH",
            AstKind::Counter => "COUNTER",
        },
        replica: true,
    };
    warnings.extend(crate::guardrails::large_data::check(db, &checked, write).await?);
    if crate::schema::ddl::no_time(timeout.as_ref()) {
        return Err(batch_timed_out(kind, &all, params.consistency));
    }
    // ScyllaDB's per-partition rate limit: not for counters, nor for a logged batch that goes
    // through the batch log (more than one mutation).
    let mutations: BTreeSet<(TableId, &[u8])> =
        all.iter().map(|(id, u, _)| (*id, &u.key.key[..])).collect();
    if kind != AstKind::Counter && (kind == AstKind::Unlogged || mutations.len() == 1) {
        let mut tables: Vec<&Arc<crate::schema::Table>> = Vec::new();
        for (_, _, t) in &all {
            if !t.is_counter() && !tables.iter().any(|x| x.table_id() == t.table_id()) {
                tables.push(t);
            }
        }
        for t in tables {
            let tokens: Vec<i64> = all
                .iter()
                .filter(|(id, ..)| *id == t.table_id())
                .map(|(_, u, _)| u.key.token)
                .collect();
            crate::rate_limit::check(db, t, &tokens, crate::rate_limit::Op::Write)?;
        }
    }
    // Derived data, then one atomic write.
    let (table, write_type) = batch_timeout_target(kind, &all);
    let writes = all.into_iter().map(|(_, u, t)| (t, u)).collect();
    modify::apply_writes(db, writes)
        .await
        .map_err(|e| modify::timed_out_at(db, e, &table, params.consistency, write_type))?;
    let _ = msg::no_keyspace;
    Ok(Response::void().with_warnings(warnings))
}

/// The timeout of a batch that had no time, as ScyllaDB reports it: a single-partition batch
/// is a plain write; a logged one fails writing the batch log.
fn batch_timed_out(
    kind: AstKind,
    all: &[(TableId, PartitionUpdate, Arc<crate::schema::Table>)],
    cl: crate::error::Consistency,
) -> Error {
    let (table, write_type) = batch_timeout_target(kind, all);
    crate::schema::ddl::write_timed_out(&table, cl, write_type)
}

/// The table and write type a batch's write timeout names.
fn batch_timeout_target(
    kind: AstKind,
    all: &[(TableId, PartitionUpdate, Arc<crate::schema::Table>)],
) -> (String, &'static str) {
    let partitions: BTreeSet<(TableId, &[u8])> =
        all.iter().map(|(id, u, _)| (*id, &u.key.key[..])).collect();
    let table = all
        .first()
        .map(|(_, _, t)| format!("{}.{}", t.keyspace, t.name))
        .unwrap_or_default();
    let counter = all.first().is_some_and(|(_, _, t)| t.is_counter());
    match kind {
        _ if counter => (table, "COUNTER"),
        _ if partitions.len() <= 1 => (table, "SIMPLE"),
        AstKind::Logged => ("system.batchlog_v2".to_string(), "BATCH_LOG"),
        _ => (table, "UNLOGGED_BATCH"),
    }
}

/// What a ScyllaDB mutation weighs beyond its data in the batch size check.
const SCYLLA_MUTATION_OVERHEAD: u64 = 213;

/// `PartitionUpdate.dataSize()` (Cassandra 5.0): rows (clustering, liveness, deletion, cells),
/// the static row (always counted) and the partition deletion.
pub fn data_size(u: &PartitionUpdate) -> u64 {
    fn row_size(r: &nosql_storage::Row) -> u64 {
        let clustering: u64 = r.clustering.components().map(|c| c.len() as u64).sum();
        let liveness = if r.liveness.is_expiring() { 20 } else { 8 };
        let mut size = clustering + liveness + 12;
        for c in &r.columns {
            match c {
                ColumnData::Simple { cell, .. } => size += cell_size(cell),
                ColumnData::Complex { cells, .. } => {
                    size += 12;
                    for cell in cells {
                        size += cell_size(cell);
                    }
                }
            }
        }
        size
    }
    fn cell_size(c: &nosql_storage::Cell) -> u64 {
        20 + c.value.len() as u64 + c.path.as_ref().map(|p| p.len() as u64).unwrap_or(0)
    }
    let mut size: u64 = u.rows.iter().map(row_size).sum();
    size += match &u.static_row {
        Some(s) => row_size(s),
        None => 20,
    };
    size += 8;
    for rt in &u.range_tombstones {
        let b =
            |x: &nosql_storage::Bound| x.prefix.components().map(|c| c.len() as u64).sum::<u64>();
        size += b(&rt.start) + b(&rt.end) + 12;
    }
    if !u.range_tombstones.is_empty() {
        size += 4;
    }
    size
}

/// Executes a protocol BATCH message.
/// Boxed so the future is checked `Send` here, with named lifetimes (rust-lang/rust#110338).
pub fn execute_protocol_batch<'a>(
    db: &'a Arc<Database>,
    session: &'a mut SessionState,
    b: response::Batch,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_protocol_batch_inner(db, session, b))
}

async fn execute_protocol_batch_inner(
    db: &Arc<Database>,
    session: &mut SessionState,
    b: response::Batch,
) -> Result<Response> {
    let d = db.dialect();
    let kind = match b.kind {
        response::BatchKind::Logged => AstKind::Logged,
        response::BatchKind::Unlogged => AstKind::Unlogged,
        response::BatchKind::Counter => AstKind::Counter,
    };
    let mut items: Vec<(Arc<ModifyPlan>, Vec<Param>)> = Vec::with_capacity(b.statements.len());
    let mut has_ts = Vec::with_capacity(b.statements.len());
    let schema = db.schema();
    let ks = b
        .params
        .keyspace
        .clone()
        .or_else(|| session.keyspace.clone());
    for s in &b.statements {
        let (prepared, values) = match s {
            response::BatchStatement::Query { text, values } => {
                let stmt = crate::session::parse(text, d)?;
                if !matches!(
                    stmt.kind,
                    StatementKind::Insert(_) | StatementKind::Update(_) | StatementKind::Delete(_)
                ) {
                    return Err(Error::invalid(
                        "Invalid statement in batch: only UPDATE, INSERT and DELETE statements are allowed.",
                    ));
                }
                let p = super::prepare(db, &schema, stmt, text, ks.as_deref())?;
                (Arc::new(p), values.clone())
            }
            response::BatchStatement::Prepared { id, values } => {
                let Some(p) = db.get_prepared(id) else {
                    return Err(msg::unprepared(d, id));
                };
                if !p.is_valid(&schema) {
                    return Err(msg::unprepared(d, id));
                }
                (p, values.clone())
            }
        };
        let super::Plan::Modify(plan) = &prepared.plan else {
            return Err(Error::invalid(
                "Invalid statement in batch: only UPDATE, INSERT and DELETE statements are allowed.",
            ));
        };
        if values.len() != prepared.bind.len() {
            return Err(Error::invalid(format!(
                "There were {} markers(?) in CQL but {} bound variables",
                prepared.bind.len(),
                values.len()
            )));
        }
        super::authorize_modify(db, session, plan)?;
        has_ts.push(plan.timestamp.is_some());
        items.push((plan.clone(), values));
    }
    let plans: Vec<Arc<ModifyPlan>> = items.iter().map(|(p, _)| p.clone()).collect();
    validate(db, kind, &plans, false, &has_ts, d)?;
    run(db, kind, &items, None, None, &b.params, None).await
}
