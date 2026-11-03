//! The part of a plan below a Gather that PostgreSQL runs in parallel workers (EXPLAIN's
//! Gather and Gather Merge: the executor runs that part in the leader's thread). What a
//! worker holds ends with it: the heavyweight locks taken while the part runs (an advisory
//! lock a PARALLEL SAFE function takes, at transaction or session level) go once it has
//! returned its last row, as the workers' go when they exit at the Gather's end.
//!
//! Only when the workers alone run the part (parallel_leader_participation off, workers
//! launched): with the leader taking part, which rows it runs itself is up to timing, and
//! it running every one (keeping their locks) is one of PostgreSQL's schedules.

use super::{BoxNode, ExecCtx, Node, Row, trigger};
use crate::error::Result;
use crate::sql::plan::{Access, Plan};
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// While a worker part runs: the session-level advisory locks taken (their keys with
    /// the mode byte, as `SessionState::advisory` keeps them).
    static ADVISORY: RefCell<Option<Vec<Vec<u8>>>> = const { RefCell::new(None) };
}

/// A session-level advisory lock was taken (`pg_advisory_lock` and the like): a worker
/// part's, if one is running.
pub fn note_session_advisory(key: &[u8]) {
    ADVISORY.with(|a| {
        if let Some(v) = a.borrow_mut().as_mut() {
            v.push(key.to_vec());
        }
    });
}

/// A node running a Gather's part in the "workers".
pub struct WorkerPart {
    inner: BoxNode,
    /// The transaction-level locks it took (the transaction's lock keys).
    taken: Vec<crate::txn::HeldLock>,
    advisory: Vec<Vec<u8>>,
    done: bool,
}

impl WorkerPart {
    pub fn wrap(inner: BoxNode) -> BoxNode {
        Box::new(WorkerPart { inner, taken: Vec::new(), advisory: Vec::new(), done: false })
    }

    /// The workers exit: their locks go.
    fn finish(&mut self, ctx: &ExecCtx) {
        if self.done {
            return;
        }
        self.done = true;
        ctx.txn.borrow_mut().release_held(std::mem::take(&mut self.taken));
        for k in std::mem::take(&mut self.advisory) {
            crate::sql::func::system::release_session_advisory(ctx, &k);
        }
    }
}

impl Node for WorkerPart {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        let mark = ctx.txn.borrow().lock_mark();
        let outer = ADVISORY.with(|a| a.replace(Some(Vec::new())));
        let r = self.inner.next(ctx);
        if let Some(mut v) = ADVISORY.with(|a| a.replace(outer)) {
            self.advisory.append(&mut v);
        }
        let mut taken = ctx.txn.borrow_mut().take_locks_since(mark);
        self.taken.append(&mut taken);
        if matches!(r, Ok(None)) {
            self.finish(ctx);
        }
        r
    }

    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        self.finish(ctx);
        self.inner.end(ctx, stmt)
    }

    fn dml_table(&self) -> Option<Arc<crate::catalog::Table>> {
        self.inner.dml_table()
    }

    fn captures(&self) -> bool {
        self.inner.captures()
    }

    fn take_captured(&mut self) -> Vec<trigger::AfterRow> {
        self.inner.take_captured()
    }

    fn add_captured(&mut self, rows: Vec<trigger::AfterRow>) {
        self.inner.add_captured(rows)
    }

    fn key_order(&self) -> Option<Vec<(usize, bool, bool)>> {
        self.inner.key_order()
    }
}

/// The plan nodes the workers run (each Gather's child), when only they run them: none
/// with the leader taking part or no workers launched.
pub fn worker_parts(gathers: &[Gather], launched: Option<&Launched>, ctx: &ExecCtx) -> Vec<usize> {
    if launched.is_none_or(|l| l.pids.is_empty()) || crate::sql::paths::parallel::ParCfg::read(ctx).leader {
        return Vec::new();
    }
    gathers.iter().filter(|g| g.workers > 0).filter_map(|g| g.part).collect()
}

/// A Gather of the running statement, as EXPLAIN places it: the part its workers run (the
/// plan node under it), the workers it plans and the relations the part opens.
#[derive(Debug)]
pub struct Gather {
    pub part: Option<usize>,
    pub workers: usize,
    pub rels: Arc<[u32]>,
}

/// The statement's Gathers (none for a plan no Gather can be part of: `may_gather`'s
/// bound over the relations it reads, `rels`, saves building its EXPLAIN tree).
pub fn gathers(plan: &Arc<Plan>, rels: &[(u32, crate::txn::locks::LockMode)], ctx: &ExecCtx) -> Vec<Gather> {
    if !crate::sql::paths::parallel::may_gather(plan, rels, ctx) {
        return Vec::new();
    }
    of_explain(plan, crate::sql::explain::gathers(plan, ctx))
}

/// The Gathers `explain::gathers` found in `plan`, with the relations each part opens.
pub fn of_explain(plan: &Arc<Plan>, found: Vec<(Option<usize>, usize)>) -> Vec<Gather> {
    found
        .into_iter()
        .map(|(part, workers)| {
            let mut rels = Vec::new();
            if let Some(p) = part.and_then(|k| find(plan, k)) {
                part_rels(p, &mut rels);
            }
            Gather { part, workers, rels: rels.into() }
        })
        .collect()
}

/// The plan's inputs (what `Plan::writes` looks through).
fn inputs(p: &Plan) -> Vec<&Arc<Plan>> {
    match p {
        Plan::With { ctes, body, .. } => ctes.iter().map(|(_, p)| p).chain(std::iter::once(body)).collect(),
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. }
        | Plan::Context { input, .. } => vec![input],
        Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => vec![left, right],
        Plan::Append { inputs, .. } => inputs.iter().collect(),
        Plan::Recursive { base, recursive, .. } => vec![base, recursive],
        _ => Vec::new(),
    }
}

fn find(p: &Arc<Plan>, key: usize) -> Option<&Arc<Plan>> {
    if Arc::as_ptr(p) as usize == key {
        return Some(p);
    }
    inputs(p).into_iter().find_map(|c| find(c, key))
}

/// The relations (tables, and the indexes they're read through) a part opens.
fn part_rels(p: &Plan, out: &mut Vec<u32>) {
    if let Plan::Scan(s) = p {
        out.push(s.table.oid);
        if let (Some((ix, _)), Access::Index { .. } | Access::Unplanned) = (&s.index, &s.access) {
            out.push(ix.oid);
        }
    }
    for c in inputs(p) {
        part_rels(c, out);
    }
}

/// A statement's Gathers start (out of the statement's frame, which deep expressions have on
/// their stack): their workers launched, pg_stat_statements' count of them in `counted`
/// (when `count`), the parts only the workers run noted (`alone`: the leader doesn't
/// take part). `rels`: the relations the statement reads (`Perms::locks`).
#[inline(never)]
pub fn start(
    plan: &Arc<Plan>,
    rels: &[(u32, crate::txn::locks::LockMode)],
    ctx: &ExecCtx,
    count: bool,
    alone: bool,
    counted: &mut i64,
    found: &std::sync::OnceLock<(Arc<Plan>, Arc<[Gather]>)>,
) -> Option<Launched> {
    // (Found once per plan: `BoundQuery::gathers`.)
    let own;
    let gathers: &[Gather] = match found.get_or_init(|| (plan.clone(), gathers(plan, rels, ctx).into())) {
        (p, g) if Arc::ptr_eq(p, plan) => g,
        _ => {
            own = gathers(plan, rels, ctx);
            &own
        }
    };
    if count {
        *counted = gathers.iter().map(|g| g.workers as i64).sum();
    }
    let launched = launch(gathers, ctx);
    if alone && !plan.writes() {
        *ctx.worker_parts.borrow_mut() = worker_parts(gathers, launched.as_ref(), ctx);
    }
    launched
}

/// The workers a statement's Gathers launch as other backends see them (pg_stat_activity's
/// parallel worker rows, their locks in pg_locks), until they exit at the statement's end;
/// counted in pg_stat_database's parallel_workers_to_launch / _launched.
pub struct Launched {
    cluster: Arc<dyn crate::session::state::Cluster>,
    leader: i32,
    pids: Vec<i32>,
}

pub fn launch(gathers: &[Gather], ctx: &ExecCtx) -> Option<Launched> {
    let planned: usize = gathers.iter().map(|g| g.workers).sum();
    if planned == 0 {
        return None;
    }
    let (max_workers, participating) = {
        let s = ctx.sess.settings.borrow();
        let max = s.get_raw("max_parallel_workers").and_then(|v| v.parse::<usize>().ok()).unwrap_or(8);
        (max, s.get_str("parallel_leader_participation").is_none_or(|v| v != "off"))
    };
    let cluster = ctx.sess.cluster.clone();
    let mut pids = Vec::new();
    for g in gathers.iter().filter(|g| g.workers > 0) {
        pids.extend(cluster.start_parallel_workers(ctx.sess.pid, g.workers, max_workers, participating, g.rels.clone()));
    }
    super::pgstat::count_parallel_workers(ctx, planned as i64, pids.len() as i64);
    Some(Launched { cluster, leader: ctx.sess.pid, pids })
}

impl Drop for Launched {
    fn drop(&mut self) {
        self.cluster.end_parallel_workers(self.leader, &self.pids);
    }
}
