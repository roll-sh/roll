//! Running a conditional statement or batch (`ModificationStatement.makeCasRequest`,
//! `BatchStatement.makeCasRequest`, `CQL3CasRequest`, `ModificationStatement.casInternal` and
//! `buildCasResultSet`; Apache-2.0), with ScyllaDB's result shape.
//!
//! 1. Every member's partition key and row are evaluated and checked (one partition, no `IN`,
//!    Cassandra's rules against mixing `IF EXISTS`, `IF NOT EXISTS` and conditions on a row),
//!    and its conditions bound.
//! 2. Under the partition lock, the rows the conditions name are read (Cassandra: only the
//!    columns `CQL3CasRequest.columnsToRead` selects; the first live row when only the static
//!    row has conditions).
//! 3. The conditions are evaluated; when they hold every member's updates are built (list
//!    operations and `c = c ± v` see the state read under the lock) at the ballot timestamp and
//!    applied atomically with the write observers.
//! 4. The result: see [`cassandra_result`] and [`scylla_result`].

use std::sync::Arc;
use std::sync::atomic::Ordering as AO;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use nosql_storage::read::{ClusteringFilter, Limits, PartitionRead};
use nosql_storage::{Clustering, PartitionKey, PartitionUpdate, Row};
use nosql_types::Dialect;

use super::eval::{self, BoundCondition, View};
use super::{Lwt, prepare};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{Conditional, ConditionalRequest};
use crate::messages as msg;
use crate::prepare::terms::EvalCtx;
use crate::response::{Response, ResponseBody, ResultMetadata, Rows};
use crate::schema::{ColumnKind, Table};
use crate::statements::modify::{self, Conditions, Del, ModKind, ModifyPlan, Targets};
use crate::storage;

impl Conditional for Lwt {
    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        request: ConditionalRequest<'a>,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(run(self, db, request))
    }
}

/// What a statement writes.
#[derive(Clone, Debug)]
enum Target {
    /// One row (the empty clustering of a table without clustering columns).
    Row(Clustering),
    /// The static row only.
    Static,
    /// The whole partition (a non-conditional `DELETE` of a partition).
    Partition,
    /// Slices or several rows (non-conditional statements only).
    Other,
}

struct Member<'a> {
    plan: &'a ModifyPlan,
    cx: EvalCtx<'a>,
    target: Target,
    conds: Vec<BoundCondition<'a>>,
}

impl Member<'_> {
    fn exists_kind(&self) -> bool {
        self.plan.conditional
            && matches!(
                self.plan.conditions,
                Conditions::Exists | Conditions::NotExists
            )
    }

    /// Conditions only on static columns (ScyllaDB then checks the static row).
    fn only_static_conditions(&self, t: &Table) -> bool {
        matches!(&self.plan.conditions, Conditions::List(cs)
            if !cs.is_empty() && cs.iter().all(|c| t.columns[c.column].is_static()))
    }
}

/// The partition as read for the conditions.
struct Current {
    static_row: Option<Row>,
    /// Rows read by name, in clustering order.
    rows: Vec<Row>,
    /// The first live row of the partition, when read.
    first: Option<Row>,
}

impl Lwt {
    /// The ballot's timestamp: ScyllaDB's in microseconds; Cassandra's
    /// `ClientState.getTimestampForPaxos`, milliseconds × 1000 made unique.
    fn ballot(&self, db: &Database) -> i64 {
        if db.dialect().is_scylla() {
            return db.timestamp();
        }
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut prev = self.last_ballot.load(AO::Relaxed);
        loop {
            let floor = prev.max(db.clock.last());
            let next = (ms * 1000).max(floor + 1);
            match self
                .last_ballot
                .compare_exchange_weak(prev, next, AO::Relaxed, AO::Relaxed)
            {
                Ok(_) => {
                    db.clock.advance_past(next);
                    return next;
                }
                Err(p) => prev = p,
            }
        }
    }
}

async fn run(lwt: &Lwt, db: &Arc<Database>, req: ConditionalRequest<'_>) -> Result<Response> {
    let d = db.dialect();
    let t = req.table.clone();
    crate::describe::paxos::touch(db, &t);
    let params = req.params;
    let now = params
        .now_in_seconds
        .map(i64::from)
        .unwrap_or_else(|| db.now_secs());
    if d.is_scylla()
        && req
            .members
            .iter()
            .any(|(p, _)| p.conditional && super::is_scylla_plain_json(d, p))
    {
        if req
            .members
            .iter()
            .any(|(p, _)| p.conditional && !super::is_scylla_plain_json(d, p))
        {
            // The reference drops the connection.
            return Err(Error::server(
                "INSERT JSON ... IF NOT EXISTS cannot be combined with other conditions in a BATCH",
            ));
        }
        return plain(db, &req).await;
    }

    // 1. Keys, rows and bound conditions, in statement order.
    let mut members: Vec<Member> = Vec::with_capacity(req.members.len());
    let mut key: Option<PartitionKey> = None;
    for (plan, values) in &req.members {
        let mut cx = EvalCtx::new(values, d);
        cx.now = params.now_in_seconds.map(i64::from);
        let restrictions = plan.restrictions.as_ref();
        if !d.is_scylla()
            && let Some(r) = restrictions
        {
            if req.batch && prepare::pk_has_in(r) {
                return Err(Error::server(
                    "java.lang.IllegalArgumentException: Batch with conditions cannot span multiple partitions (you cannot use IN on the partition key)",
                ));
            }
            if !req.batch {
                prepare::check_in(plan, r)?;
            }
        }
        let (keys, targets) = modify::resolve_targets(plan, &cx)?;
        if keys.len() != 1 {
            return Err(msg::lwt_batch_partitions(d));
        }
        let k = keys.into_iter().next().unwrap_or_default();
        match &key {
            None => key = Some(k),
            Some(prev) if prev.key != k.key => return Err(msg::lwt_batch_partitions(d)),
            Some(_) => {}
        }
        if !d.is_scylla()
            && req.batch
            && let Some(r) = restrictions
            && prepare::ck_has_in(r)
        {
            prepare::check_in(plan, r)?;
        }
        // ScyllaDB: restrictions that select no clustering row (`c = 1 AND c = 2`) leave the
        // clustering key unrestricted for the condition.
        if d.is_scylla()
            && plan.conditional
            && !t.clustering.is_empty()
            && matches!(&targets, Targets::Rows(cs) if cs.is_empty())
        {
            return Err(Error::invalid(format!(
                "Unrestricted clustering key in a conditional {}",
                if plan.kind == modify::ModKind::Delete {
                    "deletion"
                } else {
                    "update"
                }
            )));
        }
        let target = match targets {
            Targets::Rows(mut cs) if cs.len() == 1 => Target::Row(cs.remove(0)),
            Targets::Partition if plan.static_only => Target::Static,
            Targets::Partition => Target::Partition,
            _ => Target::Other,
        };
        members.push(Member {
            plan,
            cx,
            target,
            conds: Vec::new(),
        });
    }
    let Some(key) = key else {
        return Err(Error::server("empty conditional request"));
    };
    for m in &mut members {
        if let Conditions::List(cs) = &m.plan.conditions {
            for c in cs {
                let b = eval::bind(c, &m.cx)?;
                m.conds.push(b);
            }
        }
    }
    let rows_conds = if d.is_scylla() {
        None
    } else {
        Some(cassandra_row_conditions(&t, &members)?)
    };
    super::check_consistency(params)?;

    // 2. Read under the partition lock.
    let _guard = db.lock_partition(&t.id, key.token).await;
    let (names, first) = match &rows_conds {
        Some(rc) => {
            let names: Vec<Clustering> = rc.rows.iter().map(|(c, _)| c.clone()).collect();
            let first = names.is_empty();
            (names, first)
        }
        None => {
            let mut names: Vec<Clustering> = Vec::new();
            let mut first = false;
            for m in members.iter().filter(|m| m.plan.conditional) {
                match &m.target {
                    Target::Row(c) if !names.contains(c) => names.push(c.clone()),
                    Target::Static => first = true,
                    _ => {}
                }
            }
            (names, first)
        }
    };
    let cur = read(db, &t, &key, names, first, now).await?;

    // 3. Evaluate, then apply.
    let applied = match &rows_conds {
        Some(rc) => cassandra_applies(&t, rc, &members, &cur, now)?,
        None => scylla_applies(&t, &members, &cur, now)?,
    };
    let mut guard_warnings = Vec::new();
    if applied {
        let ballot = lwt.ballot(db);
        let mut ups: Vec<PartitionUpdate> = Vec::new();
        for m in &members {
            let mut a = modify::attrs(db, m.plan, params, &m.cx, None)?;
            a.timestamp = ballot;
            let mut u = modify::updates(db, m.plan, &m.cx, &a).await?;
            if !d.is_scylla() {
                // `PartitionUpdate.Builder.updateAllTimestamp`: partition and range deletions
                // at `ballot - 1`.
                for x in &mut u {
                    if x.deletion.marked_at != i64::MIN {
                        x.deletion.marked_at = ballot - 1;
                    }
                    for rt in &mut x.range_tombstones {
                        rt.deletion.marked_at = ballot - 1;
                    }
                }
            }
            ups.extend(u);
        }
        // The coordinator's large data checks (the Paxos learn skips the replica's).
        let checked: Vec<(&Table, &PartitionUpdate)> = ups.iter().map(|u| (&*t, u)).collect();
        let write = crate::guardrails::large_data::Write {
            consistency: params.consistency,
            write_type: "CAS",
            replica: false,
        };
        guard_warnings = crate::guardrails::large_data::check(db, &checked, write).await?;
        modify::apply_with_observers(db, &t, ups).await?;
    }

    // 4. The result.
    let mut rows = match &rows_conds {
        Some(rc) => cassandra_result(&t, &key, rc, &members, &cur, applied, req.batch, now),
        None => scylla_result(&t, &key, &members, &cur, applied, req.batch, now),
    };
    crate::masking::mask_conditional_rows(db, &t, &mut rows);
    let mut response = Response::new(ResponseBody::Rows(rows)).with_warnings(guard_warnings);
    response.conditional = true;
    Ok(response)
}

/// ScyllaDB's `INSERT … JSON … IF NOT EXISTS`: written unconditionally, no result rows.
async fn plain(db: &Arc<Database>, req: &ConditionalRequest<'_>) -> Result<Response> {
    let d = db.dialect();
    let ts = req.params.timestamp.unwrap_or_else(|| db.timestamp());
    let mut ups = Vec::new();
    for (plan, values) in &req.members {
        let mut cx = EvalCtx::new(values, d);
        cx.now = req.params.now_in_seconds.map(i64::from);
        let a = modify::attrs(db, plan, req.params, &cx, req.batch.then_some(ts))?;
        ups.extend(modify::updates(db, plan, &cx, &a).await?);
    }
    modify::apply_with_observers(db, req.table, ups).await?;
    Ok(Response::void())
}

async fn read(
    db: &Arc<Database>,
    t: &Arc<Table>,
    key: &PartitionKey,
    mut names: Vec<Clustering>,
    first: bool,
    now: i64,
) -> Result<Current> {
    let st = db.storage_table(t)?;
    let cmp = t.comparator.clone();
    names.sort_by(|a, b| nosql_storage::compare::compare_clustering(&*cmp, a, b));
    let mut cur = Current {
        static_row: None,
        rows: Vec::new(),
        first: None,
    };
    let mut reads: Vec<(PartitionRead, bool)> = Vec::new();
    if !names.is_empty() || !first {
        let mut r = PartitionRead::new(key.clone(), now);
        if !t.clustering.is_empty() {
            r.clustering = ClusteringFilter::Names(names);
        }
        reads.push((r, false));
    }
    if first {
        let mut r = PartitionRead::new(key.clone(), now);
        r.limits = Limits {
            rows: 1,
            ..Limits::default()
        };
        reads.push((r, true));
    }
    for (r, is_first) in reads {
        let mut stream = st.read(r);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(Error::internal)?;
            if let Some(s) = chunk.static_row
                && cur.static_row.is_none()
            {
                cur.static_row = Some(s);
            }
            for row in chunk.rows {
                if !storage::row_is_live(t, &row, now) {
                    continue;
                }
                if is_first {
                    if cur.first.is_none() {
                        cur.first = Some(row);
                    }
                } else {
                    cur.rows.push(row);
                }
            }
            if is_first && cur.first.is_some() {
                break;
            }
        }
    }
    Ok(cur)
}

fn static_live(t: &Table, cur: &Current, now: i64) -> bool {
    cur.static_row
        .as_ref()
        .is_some_and(|s| storage::row_is_live(t, s, now))
}

// ---- Cassandra ---------------------------------------------------------------------------------

/// `CQL3CasRequest.RowCondition`.
enum RowCond {
    NotExists,
    Exists,
    /// `(member, condition)` pairs.
    Columns(Vec<(usize, usize)>),
}

/// The row conditions of a request, keyed by clustering (sorted) plus the static row's, and the
/// columns `columnsToRead` selects.
struct CassandraConditions {
    rows: Vec<(Clustering, RowCond)>,
    static_cond: Option<RowCond>,
    /// Columns read (by table column index).
    read: Vec<bool>,
}

fn add_exists(slot: &mut Option<RowCond>, not: bool) -> Result<()> {
    match slot {
        None => {
            *slot = Some(if not {
                RowCond::NotExists
            } else {
                RowCond::Exists
            });
            Ok(())
        }
        Some(RowCond::NotExists) if not => Ok(()),
        Some(RowCond::Exists) if !not => Ok(()),
        Some(RowCond::NotExists | RowCond::Exists) => Err(Error::invalid(
            "Cannot mix IF EXISTS and IF NOT EXISTS conditions for the same row",
        )),
        Some(RowCond::Columns(_)) => Err(Error::invalid(format!(
            "Cannot mix IF conditions and IF {}EXISTS for the same row",
            if not { "NOT " } else { "" }
        ))),
    }
}

fn add_columns(slot: &mut Option<RowCond>, conds: Vec<(usize, usize)>) -> Result<()> {
    match slot {
        None => {
            *slot = Some(RowCond::Columns(conds));
            Ok(())
        }
        Some(RowCond::Columns(cs)) => {
            cs.extend(conds);
            Ok(())
        }
        Some(_) => Err(Error::invalid(
            "Cannot mix IF conditions and IF NOT EXISTS for the same row",
        )),
    }
}

/// The entry of a row target in the request's row conditions (created on first use).
fn row_slot(rows: &mut Vec<(Clustering, Option<RowCond>)>, target: &Target) -> Option<usize> {
    let Target::Row(c) = target else {
        return None;
    };
    Some(match rows.iter().position(|(x, _)| x == c) {
        Some(i) => i,
        None => {
            rows.push((c.clone(), None));
            rows.len() - 1
        }
    })
}

/// `ModificationStatement.updatesRegularRows`.
fn updates_regular_rows(t: &Table, plan: &ModifyPlan) -> bool {
    if t.clustering.is_empty() {
        return true;
    }
    match plan.kind {
        ModKind::Insert => !plan.static_only,
        _ => plan.restrictions.as_ref().is_some_and(|r| {
            r.ck_prefix > 0
                || r.ck_slice
                || r.ck.iter().any(|p| !p.is_empty())
                || !r.multi.is_empty()
        }),
    }
}

fn cassandra_row_conditions(t: &Table, members: &[Member]) -> Result<CassandraConditions> {
    let mut rows: Vec<(Clustering, Option<RowCond>)> = Vec::new();
    let mut static_cond: Option<RowCond> = None;
    let mut condition_columns = vec![false; t.columns.len()];
    let mut updates_regular = false;
    let mut updates_static = false;
    let cmp = t.comparator.clone();
    for (mi, m) in members.iter().enumerate() {
        updates_regular |= updates_regular_rows(t, m.plan);
        if !m.plan.conditional {
            continue;
        }
        let op_columns: Vec<usize> = m
            .plan
            .ops
            .iter()
            .map(|o| o.column())
            .chain(m.plan.deletions.iter().map(Del::column))
            .collect();
        updates_static |= op_columns.iter().any(|&c| t.columns[c].is_static());
        for o in &m.plan.ops {
            if o.needs_read(t) {
                condition_columns[o.column()] = true;
            }
        }
        for x in &m.plan.deletions {
            if let Del::Element(c, _) = x
                && matches!(t.columns[*c].ty, nosql_types::CqlType::List { .. })
            {
                condition_columns[*c] = true;
            }
        }
        let slot = row_slot(&mut rows, &m.target);
        match &m.plan.conditions {
            Conditions::NotExists | Conditions::Exists => {
                let not = matches!(m.plan.conditions, Conditions::NotExists);
                match slot {
                    Some(i) => add_exists(&mut rows[i].1, not)?,
                    None => add_exists(&mut static_cond, not)?,
                }
            }
            Conditions::List(cs) => {
                let (stat, reg): (Vec<usize>, Vec<usize>) =
                    (0..cs.len()).partition(|&i| t.columns[cs[i].column].is_static());
                for c in cs {
                    condition_columns[c.column] = true;
                }
                if !reg.is_empty() {
                    match slot {
                        Some(i) => {
                            add_columns(&mut rows[i].1, reg.iter().map(|&i| (mi, i)).collect())?
                        }
                        None => {
                            add_columns(&mut static_cond, reg.iter().map(|&i| (mi, i)).collect())?
                        }
                    }
                }
                if !stat.is_empty() {
                    add_columns(&mut static_cond, stat.iter().map(|&i| (mi, i)).collect())?;
                }
            }
            Conditions::None => {}
        }
    }
    let mut rows: Vec<(Clustering, RowCond)> = rows
        .into_iter()
        .filter_map(|(c, r)| r.map(|r| (c, r)))
        .collect();
    rows.sort_by(|a, b| nosql_storage::compare::compare_clustering(&*cmp, &a.0, &b.0));
    let read = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| match c.kind {
            ColumnKind::PartitionKey | ColumnKind::Clustering => true,
            ColumnKind::Static => updates_static || condition_columns[i],
            _ => updates_regular || condition_columns[i],
        })
        .collect();
    Ok(CassandraConditions {
        rows,
        static_cond,
        read,
    })
}

/// A row restricted to the columns read (`ColumnFilter.selection`).
fn masked(t: &Table, row: &Row, read: &[bool]) -> Row {
    let mut r = row.clone();
    r.columns.retain(|cd| {
        t.columns
            .iter()
            .position(|c| c.id == cd.column())
            .is_some_and(|i| read[i])
    });
    r
}

fn cassandra_view_rows(
    t: &Table,
    rc: &CassandraConditions,
    cur: &Current,
    now: i64,
) -> (Option<Row>, Vec<Row>) {
    let static_row = cur
        .static_row
        .as_ref()
        .map(|s| masked(t, s, &rc.read))
        .filter(|s| storage::row_is_live(t, s, now));
    let source: Vec<&Row> = match &cur.first {
        Some(f) => vec![f],
        None => cur.rows.iter().collect(),
    };
    let rows = source
        .into_iter()
        .map(|r| masked(t, r, &rc.read))
        .filter(|r| storage::row_is_live(t, r, now))
        .collect();
    (static_row, rows)
}

/// `CQL3CasRequest.appliesTo`: the static row's condition first, then rows in clustering order.
fn cassandra_applies(
    t: &Table,
    rc: &CassandraConditions,
    members: &[Member],
    cur: &Current,
    now: i64,
) -> Result<bool> {
    let (static_row, rows) = cassandra_view_rows(t, rc, cur, now);
    let check = |cond: &RowCond, row: Option<&Row>, stat: Option<&Row>| -> Result<bool> {
        Ok(match cond {
            RowCond::NotExists => row.or(stat).is_none(),
            RowCond::Exists => row.or(stat).is_some(),
            RowCond::Columns(list) => {
                let conds: Vec<&BoundCondition> =
                    list.iter().map(|&(m, i)| &members[m].conds[i]).collect();
                let view = View {
                    table: t,
                    row,
                    static_row: stat,
                    now,
                };
                eval::applies(Dialect::Cassandra, &conds, &view)?
            }
        })
    };
    if let Some(sc) = &rc.static_cond
        && !check(sc, None, static_row.as_ref())?
    {
        return Ok(false);
    }
    for (c, cond) in &rc.rows {
        let row = rows.iter().find(|r| &r.clustering == c);
        if !check(cond, row, None)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `ModificationStatement.buildCasResultSet`: `[applied]` alone on success; on failure the rows
/// read, with every column after an `IF [NOT] EXISTS`, otherwise the conditioned columns (after
/// the primary key in a batch).
#[allow(clippy::too_many_arguments)]
fn cassandra_result(
    t: &Table,
    key: &PartitionKey,
    rc: &CassandraConditions,
    members: &[Member],
    cur: &Current,
    applied: bool,
    batch: bool,
    now: i64,
) -> Rows {
    let applied_cell = Some(bool_cell(applied));
    let mut metadata = ResultMetadata {
        columns: vec![super::applied_spec(t)],
    };
    if applied {
        return rows_of(metadata, vec![vec![applied_cell]]);
    }
    let columns: Vec<usize> = if members.iter().any(Member::exists_kind) {
        (0..t.columns.len()).collect()
    } else {
        let mut cols: Vec<usize> = Vec::new();
        if batch {
            cols.extend(t.partition_key.iter().chain(&t.clustering));
        }
        for m in members.iter().filter(|m| m.plan.conditional) {
            if let Conditions::List(cs) = &m.plan.conditions {
                let regular = cs.iter().filter(|c| !t.columns[c.column].is_static());
                let stat = cs.iter().filter(|c| t.columns[c.column].is_static());
                for c in regular.chain(stat) {
                    if !cols.contains(&c.column) {
                        cols.push(c.column);
                    }
                }
            }
        }
        cols
    };
    let (static_row, rows) = cassandra_view_rows(t, rc, cur, now);
    let pk = storage::key_components(t, &key.key);
    let mut out: Vec<Vec<Option<Bytes>>> = Vec::new();
    let render = |row: Option<&Row>| -> Vec<Option<Bytes>> {
        let mut v = vec![applied_cell.clone()];
        for &c in &columns {
            v.push(cell_value(t, c, &pk, row, static_row.as_ref(), now));
        }
        v
    };
    if rows.is_empty() {
        if static_row.is_some() {
            out.push(render(None));
        }
    } else {
        for r in &rows {
            out.push(render(Some(r)));
        }
    }
    if out.is_empty() {
        return rows_of(metadata, vec![vec![applied_cell]]);
    }
    metadata
        .columns
        .extend(columns.iter().map(|&c| super::column_spec(t, c)));
    rows_of(metadata, out)
}

// ---- ScyllaDB ----------------------------------------------------------------------------------

/// The old row of a statement (`cas_request::find_old_row`).
enum Old<'r> {
    None,
    /// The partition's static row and key (the row of a static statement, or of a missing row).
    Static,
    Row(&'r Row),
}

fn find_old_row<'r>(t: &Table, m: &Member, cur: &'r Current, now: i64) -> Old<'r> {
    let present = static_live(t, cur, now) || !cur.rows.is_empty() || cur.first.is_some();
    let static_entry = if present { Old::Static } else { Old::None };
    match &m.target {
        Target::Static | Target::Partition => static_entry,
        Target::Row(_) if m.plan.conditional && m.only_static_conditions(t) => static_entry,
        // The partition's first row, read for a static condition, is also a statement's old
        // row when it is the one the statement targets.
        Target::Row(c) => match cur
            .rows
            .iter()
            .chain(&cur.first)
            .find(|r| &r.clustering == c)
        {
            Some(r) => Old::Row(r),
            None if m.exists_kind() => Old::None,
            None => static_entry,
        },
        Target::Other => Old::None,
    }
}

fn scylla_applies(t: &Table, members: &[Member], cur: &Current, now: i64) -> Result<bool> {
    let stat = cur
        .static_row
        .as_ref()
        .filter(|s| storage::row_is_live(t, s, now));
    for m in members.iter().filter(|m| m.plan.conditional) {
        let exists = || match &m.target {
            Target::Row(c) => cur.rows.iter().any(|r| &r.clustering == c),
            _ => stat.is_some(),
        };
        let ok = match &m.plan.conditions {
            Conditions::NotExists => !exists(),
            Conditions::Exists => exists(),
            Conditions::List(_) => {
                let view = match find_old_row(t, m, cur, now) {
                    Old::None => View {
                        table: t,
                        row: None,
                        static_row: None,
                        now,
                    },
                    Old::Static => View {
                        table: t,
                        row: None,
                        static_row: stat,
                        now,
                    },
                    Old::Row(r) => View {
                        table: t,
                        row: Some(r),
                        static_row: stat,
                        now,
                    },
                };
                let conds: Vec<&BoundCondition> = m.conds.iter().collect();
                eval::applies(Dialect::Scylla, &conds, &view)?
            }
            Conditions::None => true,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// ScyllaDB: one row per statement with its old row, the columns of [`super::scylla_columns`].
fn scylla_result(
    t: &Table,
    key: &PartitionKey,
    members: &[Member],
    cur: &Current,
    applied: bool,
    batch: bool,
    now: i64,
) -> Rows {
    let plans: Vec<&ModifyPlan> = members.iter().map(|m| m.plan).collect();
    let columns = super::scylla_columns(&plans, batch);
    let mut metadata = ResultMetadata {
        columns: vec![super::applied_spec(t)],
    };
    metadata
        .columns
        .extend(columns.iter().map(|&c| super::column_spec(t, c)));
    let pk = storage::key_components(t, &key.key);
    let stat = cur
        .static_row
        .as_ref()
        .filter(|s| storage::row_is_live(t, s, now));
    let mut out = Vec::with_capacity(members.len());
    for m in members {
        let mut v = vec![Some(bool_cell(applied))];
        let old = find_old_row(t, m, cur, now);
        for &c in &columns {
            v.push(match &old {
                Old::None => None,
                Old::Static if t.columns[c].kind == ColumnKind::Clustering => None,
                Old::Static => cell_value(t, c, &pk, None, stat, now),
                Old::Row(r) => cell_value(t, c, &pk, Some(r), stat, now),
            });
        }
        out.push(v);
    }
    rows_of(metadata, out)
}

// ---- rows ----------------------------------------------------------------------------------------

fn bool_cell(b: bool) -> Bytes {
    Bytes::from_static(if b { &[1] } else { &[0] })
}

fn rows_of(metadata: ResultMetadata, rows: Vec<Vec<Option<Bytes>>>) -> Rows {
    Rows {
        metadata,
        rows,
        paging_state: None,
    }
}

/// Column `c` of a result row: the key from the partition key and clustering, statics from the
/// static row, the rest from the row.
fn cell_value(
    t: &Table,
    c: usize,
    pk: &[Option<Bytes>],
    row: Option<&Row>,
    static_row: Option<&Row>,
    now: i64,
) -> Option<Bytes> {
    let col = &t.columns[c];
    match col.kind {
        ColumnKind::PartitionKey => pk.get(col.position as usize).cloned().flatten(),
        ColumnKind::Clustering => row.and_then(|r| r.clustering.get(col.position as usize)),
        ColumnKind::Static => static_row.and_then(|s| storage::column_value(t, col, s, now, None)),
        _ => row.and_then(|r| storage::column_value(t, col, r, now, None)),
    }
}
