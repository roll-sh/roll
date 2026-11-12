//! Request tracing: `system_traces.sessions` / `events` (and ScyllaDB's `sessions_time_idx`,
//! `node_slow_log`, `node_slow_log_time_idx`), written as each reference writes them.
//!
//! A request is traced when the client sets the frame's TRACING flag ([`crate::Session::set_tracing`]),
//! or at random with the probability `nodetool settraceprobability` sets
//! ([`Database::set_trace_probability`]); ScyllaDB's slow query log
//! ([`Database::set_slow_query_log`], the REST API's `/storage_service/slow_query`) records requests
//! slower than its threshold. The session row (with `duration`) and the events are written before
//! the response returns, so drivers polling `sessions` until `duration` is set
//! (`ResultSet.get_query_trace`) always find a complete trace. Rows expire after the references'
//! TTL (one day; the slow log's TTL is configurable).
//!
//! Ported from Cassandra's `Tracing`, `TraceStateImpl` and `TraceKeyspace` (rows, parameters,
//! request names, the `QUERY` command); ScyllaDB's rows (`username`, `request_size`,
//! `response_size`, `scylla_span_id`, the `table` and `param[n]` parameters, `shard 0/sl:default`
//! threads) follow its documentation (`docs/dev/tracing.md`) and live captures. Events are the
//! request's stages with their real elapsed times and the activity texts each reference prints
//! for them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};
use parking_lot::Mutex;

use crate::database::Database;
use crate::error::{Error, Result};
use crate::response::{Batch, Param, QueryParams, Response};
use crate::session::SessionState;
use crate::statements::{self, Plan, Prepared};

/// The references' trace TTL (`tracetype_query_ttl`, ScyllaDB's one day).
pub const TRACE_TTL: u32 = 86_400;

/// ScyllaDB's slow query log settings (`/storage_service/slow_query`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlowQueryLog {
    pub enable: bool,
    /// Seconds the slow log rows live.
    pub ttl: u32,
    /// Microseconds above which a request is logged.
    pub threshold: u64,
    /// Record the session and slow log rows only, no events.
    pub fast: bool,
}

impl Default for SlowQueryLog {
    fn default() -> Self {
        SlowQueryLog {
            enable: false,
            ttl: TRACE_TTL,
            threshold: 500_000,
            fast: false,
        }
    }
}

/// Tracing state of a database.
#[derive(Default)]
pub struct State {
    probability: AtomicU64,
    /// `slow.enable`, read on every request without the lock.
    slow_enabled: AtomicBool,
    slow: Mutex<SlowQueryLog>,
    prepared: Mutex<HashMap<String, Arc<Prepared>>>,
    spans: AtomicU64,
}

impl Database {
    /// `nodetool settraceprobability`: the share of requests traced without the client asking.
    pub fn set_trace_probability(&self, p: f64) -> Result<()> {
        if !(0.0..=1.0).contains(&p) || p.is_nan() {
            return Err(Error::invalid("Trace probability must be between 0 and 1"));
        }
        self.tracing
            .probability
            .store(p.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    /// `nodetool gettraceprobability`.
    pub fn trace_probability(&self) -> f64 {
        f64::from_bits(self.tracing.probability.load(Ordering::Relaxed))
    }

    /// ScyllaDB's slow query logging (`POST /storage_service/slow_query`).
    pub fn set_slow_query_log(&self, s: SlowQueryLog) {
        let mut slow = self.tracing.slow.lock();
        *slow = s;
        self.tracing.slow_enabled.store(s.enable, Ordering::Relaxed);
    }

    pub fn slow_query_log(&self) -> SlowQueryLog {
        *self.tracing.slow.lock()
    }
}

/// What is being traced.
pub enum Request<'a> {
    Query {
        text: &'a str,
        params: &'a QueryParams,
    },
    Execute {
        prepared: &'a Prepared,
        params: &'a QueryParams,
    },
    Batch {
        batch: &'a Batch,
    },
    Prepare {
        text: &'a str,
    },
}

/// Where an event happens (Cassandra's thread pools).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Request,
    Read,
    Mutation,
}

struct Ev {
    elapsed: i64,
    activity: String,
    stage: Stage,
}

/// A request being traced.
pub struct Trace {
    pub id: [u8; 16],
    /// The client asked for it (the response carries the id).
    requested: bool,
    /// Only kept when slower than the slow log threshold.
    slow_only: bool,
    fast: bool,
    started: i64,
    instant: Instant,
    request: String,
    parameters: BTreeMap<String, String>,
    tables: BTreeSet<String>,
    command_text: String,
    client: Option<IpAddr>,
    user: String,
    request_size: i32,
    events: Vec<Ev>,
    d: Dialect,
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Starts tracing a request if the client asked for it, the trace probability picks it, or
/// ScyllaDB's slow query log is on.
pub fn begin(
    db: &Database,
    session: &SessionState,
    requested: bool,
    req: Request<'_>,
) -> Option<Trace> {
    let d = db.dialect();
    let p = db.trace_probability();
    let sampled = p > 0.0 && rand::random::<f64>() < p;
    let slow_only = !(requested || sampled);
    if slow_only && !(d.is_scylla() && db.tracing.slow_enabled.load(Ordering::Relaxed)) {
        return None;
    }
    let slow = db.slow_query_log();
    let mut parameters = BTreeMap::new();
    let consistency = |params: &QueryParams, out: &mut BTreeMap<String, String>| {
        out.insert(
            "consistency_level".into(),
            params.consistency.name().to_string(),
        );
        out.insert(
            "serial_consistency_level".into(),
            params
                .serial_consistency
                .map(|c| c.name())
                .unwrap_or("SERIAL")
                .to_string(),
        );
        if let Some(n) = params.page_size.filter(|n| *n > 0) {
            out.insert("page_size".into(), n.to_string());
        }
        if d.is_scylla()
            && let Some(ts) = params.timestamp
        {
            out.insert("user_timestamp".into(), ts.to_string());
        }
    };
    let (request, text, size) = match &req {
        Request::Query { text, params } => {
            consistency(params, &mut parameters);
            parameters.insert("query".into(), text.to_string());
            let size = text.len() + 4 + params.values.iter().map(param_len).sum::<usize>() + 16;
            ("Execute CQL3 query".to_string(), text.to_string(), size)
        }
        Request::Execute { prepared, params } => {
            consistency(params, &mut parameters);
            parameters.insert("query".into(), prepared.text.clone());
            for (i, (spec, v)) in prepared.bind.iter().zip(&params.values).enumerate() {
                let value = match v {
                    Param::Set(b) if d.is_scylla() => scylla_param(&spec.ty, b),
                    Param::Set(b) => nosql_types::to_cql_literal(&spec.ty, b, d)
                        .unwrap_or_else(|_| format!("0x{}", hex(b))),
                    Param::Null => "null".into(),
                    Param::Unset => "<unset>".into(),
                };
                match d {
                    Dialect::Cassandra => {
                        let mut value = value;
                        if value.len() > 1000 {
                            value.truncate(1000);
                            value.push_str("...'");
                        }
                        parameters.insert(format!("bound_var_{i}_{}", spec.name), value);
                    }
                    Dialect::Scylla => {
                        parameters.insert(format!("param[{i}]"), value);
                    }
                }
            }
            let request = match d {
                Dialect::Scylla => format!("Execute CQL3 prepared query [{}]", hex(&prepared.id)),
                Dialect::Cassandra => "Execute CQL3 prepared query".to_string(),
            };
            let size = 18 + params.values.iter().map(param_len).sum::<usize>() + 16;
            (request, prepared.text.clone(), size)
        }
        Request::Batch { batch } => {
            consistency(&batch.params, &mut parameters);
            parameters.remove("page_size");
            let texts: Vec<String> = batch
                .statements
                .iter()
                .map(|s| match s {
                    crate::response::BatchStatement::Query { text, .. } => text.clone(),
                    crate::response::BatchStatement::Prepared { id, .. } => db
                        .get_prepared(id)
                        .map(|p| p.text.clone())
                        .unwrap_or_default(),
                })
                .collect();
            if d.is_scylla() {
                for (i, t) in texts.iter().enumerate() {
                    parameters.insert(format!("query[{i}]"), t.clone());
                }
            }
            let size = texts.iter().map(|t| t.len() + 8).sum::<usize>() + 16;
            (
                "Execute batch of CQL3 queries".to_string(),
                texts.join("; "),
                size,
            )
        }
        Request::Prepare { text } => {
            parameters.insert("query".into(), text.to_string());
            (
                "Preparing CQL3 query".to_string(),
                text.to_string(),
                text.len() + 4,
            )
        }
    };
    let fast = slow_only && slow.fast;
    Some(Trace {
        id: crate::uuid::timeuuid(),
        requested,
        slow_only,
        fast,
        started: now_micros(),
        instant: Instant::now(),
        request,
        parameters,
        tables: BTreeSet::new(),
        command_text: text,
        client: session.client.address.map(|a| a.ip()),
        user: session
            .user
            .clone()
            .unwrap_or_else(|| "anonymous".to_string()),
        request_size: size.min(i32::MAX as usize) as i32,
        events: Vec::new(),
        d,
    })
}

/// Starts tracing a request of another API (Alternator), when the trace probability picks it
/// or ScyllaDB's slow query log is on: `request` is the session's `request` (`Alternator
/// PutItem`), `command` the slow log's command, `tables` the tables it uses.
#[allow(clippy::too_many_arguments)]
pub fn begin_request(
    db: &Database,
    request: String,
    parameters: BTreeMap<String, String>,
    command: String,
    tables: BTreeSet<String>,
    client: Option<IpAddr>,
    user: Option<&str>,
    request_size: usize,
) -> Option<Trace> {
    let d = db.dialect();
    let p = db.trace_probability();
    let sampled = p > 0.0 && rand::random::<f64>() < p;
    let slow_only = !sampled;
    if slow_only && !(d.is_scylla() && db.tracing.slow_enabled.load(Ordering::Relaxed)) {
        return None;
    }
    Some(Trace {
        id: crate::uuid::timeuuid(),
        requested: false,
        slow_only,
        fast: slow_only && db.slow_query_log().fast,
        started: now_micros(),
        instant: Instant::now(),
        request,
        parameters,
        tables,
        command_text: command,
        client,
        user: user.unwrap_or("anonymous").to_string(),
        request_size: request_size.min(i32::MAX as usize) as i32,
        events: Vec::new(),
        d,
    })
}

fn param_len(p: &Param) -> usize {
    match p {
        Param::Set(b) => 4 + b.len(),
        _ => 4,
    }
}

/// ScyllaDB's rendering of a bound value in `param[n]` (probed with every type): list and
/// vector elements joined by ", ", set elements by "; ", map entries as `{k : v}` joined by
/// ", ", tuple and UDT fields joined by ":" with `@` for null, blobs as bare hex, scalars as
/// `CAST(x AS text)`. Values it can't take apart print as hex.
fn scylla_param(ty: &CqlType, b: &[u8]) -> String {
    let join = |elem: &CqlType, sep: &str| -> Option<String> {
        let es = nosql_types::collection_elements(b).ok()?;
        Some(
            es.iter()
                .map(|e| e.map_or_else(|| "null".to_string(), |e| scylla_param(elem, e)))
                .collect::<Vec<_>>()
                .join(sep),
        )
    };
    let text = match ty {
        CqlType::Blob => Some(hex(b)),
        CqlType::List { elem, .. } => join(elem, ", "),
        CqlType::Set { elem, .. } => join(elem, "; "),
        CqlType::Vector { elem, dim } => {
            nosql_types::vector_elements(elem, *dim, b).ok().map(|es| {
                es.iter()
                    .map(|e| scylla_param(elem, e))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        }
        CqlType::Map { key, value, .. } => nosql_types::map_entries(b).ok().map(|es| {
            es.iter()
                .map(|(k, v)| format!("{{{} : {}}}", scylla_param(key, k), scylla_param(value, v)))
                .collect::<Vec<_>>()
                .join(", ")
        }),
        CqlType::Tuple(_) | CqlType::Udt(..) => {
            let types: Vec<&CqlType> = match ty {
                CqlType::Tuple(ts) => ts.iter().collect(),
                CqlType::Udt(def, _) => def.fields.iter().map(|(_, t)| t).collect(),
                _ => Vec::new(),
            };
            nosql_types::tuple_fields(b).ok().map(|fs| {
                fs.iter()
                    .enumerate()
                    .map(|(i, f)| match (f, types.get(i)) {
                        (Some(f), Some(t)) => scylla_param(t, f),
                        _ => "@".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(":")
            })
        }
        _ => nosql_types::to_text(ty, b, Dialect::Scylla)
            .ok()
            .map(|t| t.unwrap_or_else(|| "null".into())),
    };
    text.unwrap_or_else(|| hex(b))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Trace {
    /// Records an event now.
    pub fn event(&mut self, stage: Stage, activity: impl Into<String>) {
        if self.fast {
            return;
        }
        let elapsed = self.instant.elapsed().as_micros() as i64;
        self.events.push(Ev {
            elapsed,
            activity: activity.into(),
            stage,
        });
    }

    pub fn dialect(&self) -> Dialect {
        self.d
    }

    /// The request text was parsed (QUERY and PREPARE).
    pub fn parsing(&mut self, text: &str) {
        match self.d {
            Dialect::Scylla => self.event(Stage::Request, "Parsing a statement"),
            Dialect::Cassandra => self.event(Stage::Request, format!("Parsing {text}")),
        }
    }

    /// The statement was prepared; `bound` for EXECUTE.
    pub fn prepared(&mut self, bound: bool) {
        let user = self.user.clone();
        match self.d {
            Dialect::Scylla if bound => {
                self.event(Stage::Request, "Checking bounds");
                self.event(Stage::Request, "Processing a statement");
            }
            Dialect::Scylla => self.event(
                Stage::Request,
                format!("Processing a statement for authenticated user: {user}"),
            ),
            Dialect::Cassandra if bound => {}
            Dialect::Cassandra => self.event(Stage::Request, "Preparing statement"),
        }
    }

    /// The events of executing `p` (before execution: what it will do).
    pub fn executing(&mut self, db: &Database, p: &Prepared) {
        let d = self.d;
        match &p.plan {
            Plan::Select(plan) => {
                let t = &plan.table;
                self.table_parameter(&t.keyspace, &t.name);
                let single =
                    plan.restrictions.key_mode == crate::prepare::restrictions::KeyMode::Keys;
                match d {
                    Dialect::Scylla => {
                        if let Some(name) = &plan.consulted {
                            let what = if t.clustering.is_empty() {
                                "a single slice of keys"
                            } else {
                                "a list of rows containing keys"
                            };
                            self.event(
                                Stage::Request,
                                format!("Consulting index {name} for {what}"),
                            );
                        }
                        self.event(
                            Stage::Read,
                            format!("Executing read query (reversed {})", plan.reversed),
                        );
                        self.event(Stage::Read, "read_data: querying locally");
                        self.event(
                            Stage::Read,
                            if single {
                                "Start querying singular range"
                            } else {
                                "Start querying token range (-inf, +inf)"
                            },
                        );
                    }
                    Dialect::Cassandra if single => {
                        self.event(
                            Stage::Read,
                            format!("Executing single-partition query on {}", t.name),
                        );
                        self.event(Stage::Read, "Acquiring sstable references");
                        self.event(Stage::Read, "Merging memtable contents");
                    }
                    Dialect::Cassandra => {
                        let ranges = db.config().num_tokens + 1;
                        self.event(Stage::Request, "Computing ranges to query");
                        self.event(
                            Stage::Request,
                            format!(
                                "Submitting range requests on {ranges} ranges with a concurrency of 1 (0.0 rows per range expected)"
                            ),
                        );
                        self.event(Stage::Request, "Submitted 1 concurrent range requests");
                        self.event(
                            Stage::Read,
                            "Executing seq scan across 0 sstables for (min(-9223372036854775808), min(-9223372036854775808)]",
                        );
                    }
                }
            }
            Plan::Modify(plan) => {
                let t = &plan.table;
                self.table_parameter(&t.keyspace, &t.name);
                self.mutation(db, &t.name, false);
                self.view_updates(db, t);
            }
            Plan::Batch(plan) => {
                for s in &plan.statements {
                    self.tables
                        .insert(format!("{}.{}", s.table.keyspace, s.table.name));
                }
                let name = plan
                    .statements
                    .first()
                    .map(|s| s.table.name.clone())
                    .unwrap_or_default();
                if d.is_scylla() {
                    self.batch_endpoints(db);
                    self.event(Stage::Request, "Created a batch context");
                }
                self.mutation(db, &name, true);
            }
            _ => {}
        }
    }

    /// A SELECT continuing a paged read: ScyllaDB picks up the reader the previous page saved
    /// (its querier cache); roll resumes exactly where that page ended, so always reuses it.
    pub fn resuming(&mut self, p: &Prepared, paged: bool) {
        if paged && self.d.is_scylla() && matches!(p.plan, Plan::Select(_)) {
            self.event(Stage::Read, "Reusing querier");
        }
    }

    /// ScyllaDB names the table of a single-table statement in the parameters.
    fn table_parameter(&mut self, keyspace: &str, table: &str) {
        let name = format!("{keyspace}.{table}");
        if self.d.is_scylla() {
            self.parameters.insert("table".into(), name.clone());
        }
        self.tables.insert(name);
    }

    /// ScyllaDB's `batch_endpoints` parameter: the replicas of the batchlog.
    fn batch_endpoints(&mut self, db: &Database) {
        self.parameters.insert(
            "batch_endpoints".into(),
            format!("/{}", crate::uuid::format(&db.config().host_id())),
        );
    }

    /// The events of a protocol BATCH (before execution).
    pub fn batch(&mut self, db: &Database, b: &Batch) {
        let mut first = None;
        for s in &b.statements {
            if let crate::response::BatchStatement::Prepared { id, .. } = s
                && let Some(p) = db.get_prepared(id)
                && let Plan::Modify(plan) = &p.plan
            {
                let t = &plan.table;
                self.tables.insert(format!("{}.{}", t.keyspace, t.name));
                first.get_or_insert_with(|| t.name.clone());
            }
        }
        if self.d.is_scylla() {
            self.batch_endpoints(db);
            self.event(Stage::Request, "Created a batch context");
        }
        self.mutation(db, first.as_deref().unwrap_or("batches"), true);
    }

    fn mutation(&mut self, db: &Database, table: &str, batch: bool) {
        let host = crate::uuid::format(&db.config().host_id());
        match self.d {
            Dialect::Scylla => {
                self.event(
                    Stage::Mutation,
                    format!("Creating write handler with live: [{host}] dead: []"),
                );
                if batch {
                    self.event(Stage::Mutation, "Sending a batchlog write mutation");
                }
                self.event(Stage::Mutation, "Executing a mutation locally");
            }
            Dialect::Cassandra => {
                self.event(
                    Stage::Request,
                    if batch {
                        "Determining replicas for atomic batch"
                    } else {
                        "Determining replicas for mutation"
                    },
                );
                self.event(Stage::Mutation, "Appending to commitlog");
                self.event(Stage::Mutation, format!("Adding to {table} memtable"));
            }
        }
    }

    /// ScyllaDB's view update events of a write to a table with views: the views with
    /// `synchronous_updates` are named, then the propagation.
    fn view_updates(&mut self, db: &Database, t: &crate::schema::Table) {
        if !self.d.is_scylla() {
            return;
        }
        let schema = db.schema();
        let Some(ks) = schema.keyspace(&t.keyspace) else {
            return;
        };
        let mut any = false;
        for v in crate::views::views_of(ks, t) {
            any = true;
            if crate::views::synchronous(v) {
                self.event(
                    Stage::Mutation,
                    format!(
                        "Forcing {}.{} view update to be synchronous (synchronous_updates property was set)",
                        v.keyspace, v.name
                    ),
                );
            }
        }
        if any {
            self.event(
                Stage::Mutation,
                format!(
                    "View updates for {}.{} were generated and propagated",
                    t.keyspace, t.name
                ),
            );
        }
    }

    /// The statement ran and returned `r`.
    pub fn executed(&mut self, db: &Database, r: &Result<Response>) {
        let host = crate::uuid::format(&db.config().host_id());
        let rows = r.as_ref().ok().and_then(|r| r.rows()).map(|r| r.rows.len());
        match self.d {
            Dialect::Scylla => {
                if let Some(n) = rows {
                    self.event(
                        Stage::Read,
                        format!(
                            "Page stats: {n} partition(s) ({n} live, 0 dead), 0 static row(s) (0 live, 0 dead), {n} clustering row(s) ({n} live, 0 dead), 0 range tombstone(s) and {n} cell(s) ({n} live, 0 dead)"
                        ),
                    );
                    self.event(Stage::Read, "Querying is done");
                } else if !self.tables.is_empty() {
                    self.event(Stage::Mutation, format!("Got a response from /{host}"));
                    self.event(Stage::Mutation, "Mutation successfully completed");
                }
                self.event(Stage::Request, "Done processing - preparing a result");
            }
            Dialect::Cassandra => {
                if let Some(n) = rows
                    && !self.tables.is_empty()
                {
                    self.event(
                        Stage::Read,
                        format!("Read {n} live rows and 0 tombstone cells"),
                    );
                }
            }
        }
    }

    fn thread(&self, stage: Stage) -> &'static str {
        match self.d {
            Dialect::Scylla => "shard 0/sl:default",
            Dialect::Cassandra => match stage {
                Stage::Request => "Native-Transport-Requests-1",
                Stage::Read => "ReadStage-1",
                Stage::Mutation => "MutationStage-1",
            },
        }
    }
}

/// Ends a trace: writes its rows and puts the id on the response when the client asked.
pub async fn finish(
    db: &Arc<Database>,
    trace: Option<Trace>,
    mut r: Result<Response>,
) -> Result<Response> {
    if trace.is_none() {
        return r;
    }
    let size = response_size(r.as_ref().ok().and_then(|r| r.rows()));
    let id = end(db, trace, size).await;
    match &mut r {
        Ok(resp) => resp.tracing_id = id,
        Err(e) => e.tracing_id = id,
    }
    r
}

/// The size of a RESULT body carrying `rows` (ScyllaDB's `response_size`).
pub fn response_size(rows: Option<&crate::response::Rows>) -> usize {
    rows.map(|rows| {
        rows.rows
            .iter()
            .flatten()
            .map(|c| 4 + c.as_ref().map_or(0, |b| b.len()))
            .sum::<usize>()
            + 12
    })
    .unwrap_or(4)
}

/// Ends a trace (`response_size`: [`response_size`] of the result); the id to send back when
/// the client asked for tracing.
pub async fn end(
    db: &Arc<Database>,
    trace: Option<Trace>,
    response_size: usize,
) -> Option<[u8; 16]> {
    let t = trace?;
    let duration = t.instant.elapsed().as_micros().min(i32::MAX as u128) as i32;
    let slow = db.slow_query_log();
    let is_slow = t.d.is_scylla() && slow.enable && duration as u64 >= slow.threshold;
    if t.slow_only && !is_slow {
        return None;
    }
    // Trace writes never fail the request (the references write them asynchronously).
    let size = response_size.min(i32::MAX as usize) as i32;
    if let Err(e) = write(db, &t, duration, size, is_slow, slow.ttl).await {
        tracing::warn!(db = %db.config().id, error = %e.message, "writing a trace failed");
    }
    t.requested.then_some(t.id)
}

async fn write(
    db: &Arc<Database>,
    t: &Trace,
    duration: i32,
    response_size: i32,
    slow: bool,
    slow_ttl: u32,
) -> Result<()> {
    let cfg = db.config();
    let coordinator = cfg.address;
    let client = t.client.unwrap_or(coordinator);
    let id = Param::new(Bytes::copy_from_slice(&t.id));
    let params = Param::new(nosql_types::pack_map(
        t.parameters
            .iter()
            .map(|(k, v)| (k.as_bytes(), v.as_bytes())),
    ));
    let started_ms = t.started / 1000;
    let mut writes: Vec<(String, Vec<Param>)> = Vec::new();
    match t.d {
        Dialect::Cassandra => {
            writes.push((
                format!(
                    "INSERT INTO system_traces.sessions (session_id, client, command, coordinator, coordinator_port, duration, parameters, request, started_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) USING TTL {TRACE_TTL}"
                ),
                vec![
                    id.clone(),
                    p_inet(&client),
                    p_text("QUERY"),
                    p_inet(&coordinator),
                    p_int(7000),
                    p_int(duration),
                    params,
                    p_text(&t.request),
                    p_bigint(started_ms),
                ],
            ));
            for (e, eid) in t.events.iter().zip(event_ids(t)) {
                writes.push((
                    format!(
                        "INSERT INTO system_traces.events (session_id, event_id, activity, source, source_elapsed, source_port, thread) VALUES (?, ?, ?, ?, ?, ?, ?) USING TTL {TRACE_TTL}"
                    ),
                    vec![
                        id.clone(),
                        p_uuid(&eid),
                        p_text(&e.activity),
                        p_inet(&coordinator),
                        p_int(e.elapsed.min(i32::MAX as i64) as i32),
                        p_int(7000),
                        p_text(t.thread(e.stage)),
                    ],
                ));
            }
        }
        Dialect::Scylla => {
            let ttl = if t.slow_only { slow_ttl } else { TRACE_TTL };
            writes.push((
                format!(
                    "INSERT INTO system_traces.sessions (session_id, client, command, coordinator, duration, parameters, request, request_size, response_size, started_at, username) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) USING TTL {ttl}"
                ),
                vec![
                    id.clone(),
                    p_inet(&client),
                    p_text("QUERY"),
                    p_inet(&coordinator),
                    p_int(duration),
                    params.clone(),
                    p_text(&t.request),
                    p_int(t.request_size),
                    p_int(response_size),
                    p_bigint(started_ms),
                    p_text(&t.user),
                ],
            ));
            let minute = started_ms - started_ms.rem_euclid(60_000);
            writes.push((
                format!(
                    "INSERT INTO system_traces.sessions_time_idx (minute, started_at, session_id) VALUES (?, ?, ?) USING TTL {ttl}"
                ),
                vec![p_bigint(minute), p_bigint(started_ms), id.clone()],
            ));
            let span = db.tracing.spans.fetch_add(1, Ordering::Relaxed) as i64
                + (t.started & 0x0000_ffff_ffff_ffff);
            for (e, eid) in t.events.iter().zip(event_ids(t)) {
                writes.push((
                    format!(
                        "INSERT INTO system_traces.events (session_id, event_id, activity, scylla_parent_id, scylla_span_id, source, source_elapsed, thread) VALUES (?, ?, ?, ?, ?, ?, ?, ?) USING TTL {ttl}"
                    ),
                    vec![
                        id.clone(),
                        p_uuid(&eid),
                        p_text(&e.activity),
                        p_bigint(0),
                        p_bigint(span),
                        p_inet(&coordinator),
                        p_int(e.elapsed.min(i32::MAX as i64) as i32),
                        p_text(t.thread(e.stage)),
                    ],
                ));
            }
            if slow {
                let start_time = event_id(t.started);
                writes.push((
                    format!(
                        "INSERT INTO system_traces.node_slow_log (start_time, node_ip, shard, command, date, duration, parameters, session_id, source_ip, table_names, username) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) USING TTL {slow_ttl}"
                    ),
                    vec![
                        p_uuid(&start_time),
                        p_inet(&coordinator),
                        p_int(0),
                        p_text(&t.command_text),
                        p_bigint(started_ms),
                        p_int(duration),
                        params,
                        id.clone(),
                        p_inet(&client),
                        Param::new(nosql_types::pack_collection(
                            t.tables.iter().map(|s| s.as_bytes()),
                        )),
                        p_text(&t.user),
                    ],
                ));
                writes.push((
                    format!(
                        "INSERT INTO system_traces.node_slow_log_time_idx (minute, started_at, session_id, start_time, node_ip, shard) VALUES (?, ?, ?, ?, ?, ?) USING TTL {slow_ttl}"
                    ),
                    vec![
                        p_bigint(minute),
                        p_bigint(started_ms),
                        id.clone(),
                        p_uuid(&start_time),
                        p_inet(&coordinator),
                        p_int(0),
                    ],
                ));
            }
        }
    }
    for (cql, values) in writes {
        internal(db, &cql, values).await?;
    }
    Ok(())
}

/// The events' ids: timeuuids at their times, strictly increasing so the events read back in
/// the order they happened.
fn event_ids(t: &Trace) -> Vec<[u8; 16]> {
    let mut last = 0i64;
    t.events
        .iter()
        .map(|e| {
            let ts = ((t.started + e.elapsed) * 10).max(last + 1);
            last = ts;
            timeuuid_at(ts)
        })
        .collect()
}

/// A timeuuid at `micros`.
fn event_id(micros: i64) -> [u8; 16] {
    timeuuid_at(micros * 10)
}

/// A timeuuid at `ts` (100ns intervals since the Unix epoch).
fn timeuuid_at(ts: i64) -> [u8; 16] {
    let mut u = crate::uuid::timeuuid();
    // 100ns intervals since 1582-10-15.
    let ts = (ts + 0x01B2_1DD2_1381_4000) as u64;
    let low = (ts & 0xffff_ffff) as u32;
    let mid = ((ts >> 32) & 0xffff) as u16;
    let hi = (((ts >> 48) & 0x0fff) as u16) | 0x1000;
    u[0..4].copy_from_slice(&low.to_be_bytes());
    u[4..6].copy_from_slice(&mid.to_be_bytes());
    u[6..8].copy_from_slice(&hi.to_be_bytes());
    u
}

/// Executes an internal write (no authorization, not traced; `crate::audit` writes this way
/// too).
pub(crate) async fn internal(db: &Arc<Database>, cql: &str, values: Vec<Param>) -> Result<()> {
    let p = {
        let schema = db.schema();
        let cached = db.tracing.prepared.lock().get(cql).cloned();
        match cached {
            Some(p) if p.is_valid(&schema) => p,
            _ => {
                let stmt = crate::session::parse(cql, db.dialect())?;
                let p = Arc::new(statements::prepare(db, &schema, stmt, cql, None)?);
                let mut cache = db.tracing.prepared.lock();
                if cache.len() > 64 {
                    cache.clear();
                }
                cache.insert(cql.to_string(), p.clone());
                p
            }
        }
    };
    let params = QueryParams::with_values(values);
    match &p.plan {
        Plan::Modify(plan) => statements::modify::execute(db, plan, &params, &p.stmt)
            .await
            .map(|_| ()),
        _ => Err(Error::server(format!("internal tracing statement {cql}"))),
    }
}

pub(crate) fn p_text(s: &str) -> Param {
    Param::new(Bytes::copy_from_slice(s.as_bytes()))
}

pub(crate) fn p_int(v: i32) -> Param {
    Param::new(Bytes::copy_from_slice(&v.to_be_bytes()))
}

pub(crate) fn p_bigint(v: i64) -> Param {
    Param::new(Bytes::copy_from_slice(&v.to_be_bytes()))
}

pub(crate) fn p_uuid(u: &[u8; 16]) -> Param {
    Param::new(Bytes::copy_from_slice(u))
}

pub(crate) fn p_inet(a: &IpAddr) -> Param {
    match crate::system::val::inet(a) {
        Some(b) => Param::Set(b),
        None => Param::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_are_v1_at_the_time() {
        let u = event_id(1_700_000_000_000_000);
        assert_eq!(crate::uuid::version(&u), 1);
        let a = event_id(1_700_000_000_000_000);
        let b = event_id(1_700_000_000_000_100);
        assert_eq!(
            nosql_types::compare_timeuuid(&a, &b),
            std::cmp::Ordering::Less
        );
    }
}
