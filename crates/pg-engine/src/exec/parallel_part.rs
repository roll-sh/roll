//! The part of a plan below a Gather that PostgreSQL runs in parallel workers (EXPLAIN's
//! Gather and Gather Merge: the executor runs that part in the leader's thread). What a
//! worker holds ends with it: the heavyweight locks taken while the part runs (an advisory
//! lock a PARALLEL SAFE function takes, at transaction or session level) go once it has
//! returned its last row, as the workers' go when they exit at the Gather's end.
//!
//! Gather startup evaluates the independent initplan outputs the part reads before any
//! of its rows run, as ExecInitParallelPlan prepares parameters sent to workers.
//!
//! Lock release applies only when the workers alone run the part (parallel_leader_participation off, workers
//! launched): with the leader taking part, which rows it runs itself is up to timing, and
//! it running every one (keeping their locks) is one of PostgreSQL's schedules.

use super::{BoxNode, ExecCtx, Node, Row, trigger};
use crate::error::Result;
use crate::sql::plan::{Access, Plan};
use crate::sql::expr::{Expr, SubqueryKind, eval};
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

/// Work owned by one Gather boundary in this execution.
pub struct Part {
    pub key: usize,
    pub initplans: Arc<[Expr]>,
    pub alone: bool,
}

/// A Gather part's startup work and, when exclusive, its worker lock ownership.
pub struct WorkerPart {
    inner: BoxNode,
    initplans: Option<Arc<[Expr]>>,
    alone: bool,
    /// The transaction-level locks it took (the transaction's lock keys).
    taken: Vec<crate::txn::HeldLock>,
    advisory: Vec<Vec<u8>>,
    done: bool,
}

impl WorkerPart {
    pub fn wrap(inner: BoxNode, initplans: Arc<[Expr]>, alone: bool) -> BoxNode {
        Box::new(WorkerPart { inner, initplans: Some(initplans), alone, taken: Vec::new(), advisory: Vec::new(), done: false })
    }

    fn start(&mut self, ctx: &ExecCtx) -> Result<()> {
        if let Some(initplans) = self.initplans.take() {
            for e in initplans.iter() {
                eval(e, &[], ctx)?;
            }
        }
        Ok(())
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
        self.start(ctx)?;
        if !self.alone {
            return self.inner.next(ctx);
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

    fn next_item(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        if self.alone {
            return Ok(self.next(ctx)?.map(|r| super::pipe::Item::Row(r, super::pipe::ALL)));
        }
        self.start(ctx)?;
        self.inner.next_item(ctx)
    }

    fn pipeline(&mut self, above: Vec<Arc<super::pipe::Stage>>, spec: Option<Arc<super::partial::Spec>>, ctx: &ExecCtx) -> Result<Option<u8>> {
        if self.alone {
            return Ok(None);
        }
        self.start(ctx)?;
        self.inner.pipeline(above, spec, ctx)
    }

    fn pipeline_projected(&mut self, spec: Arc<super::partial::Spec>, ctx: &ExecCtx) -> Result<Option<u8>> {
        if self.alone {
            return Ok(None);
        }
        self.start(ctx)?;
        self.inner.pipeline_projected(spec, ctx)
    }

    fn compute_in_workers(&mut self, c: super::Computed, ctx: &ExecCtx) -> Result<Option<usize>> {
        if self.alone {
            return Ok(None);
        }
        self.start(ctx)?;
        self.inner.compute_in_workers(c, ctx)
    }

    fn projection(&self) -> Option<&[Expr]> { self.inner.projection() }
    fn topk_below(&mut self, t: super::TopK) -> bool { self.inner.topk_below(t) }
    fn take_skipped(&mut self) -> usize { self.inner.take_skipped() }
    fn restrict_keys(&mut self, col: usize, keys: &[u32], ctx: &ExecCtx) -> Result<()> { self.inner.restrict_keys(col, keys, ctx) }
    fn prefetch(&mut self, ctx: &ExecCtx) -> Result<()> { self.start(ctx)?; self.inner.prefetch(ctx) }

    fn key_order(&self) -> Option<Vec<(usize, bool, bool)>> {
        self.inner.key_order()
    }

    fn output_order(&self, ctx: &ExecCtx) -> Option<Vec<(usize, bool, bool)>> {
        self.inner.output_order(ctx)
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
    pub initplans: Arc<[Expr]>,
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
            let mut initplans = Vec::new();
            if let Some(p) = part.and_then(|k| find(plan, k)) {
                part_initplans(p, &mut initplans);
            }
            Gather { part, workers, rels: rels.into(), initplans: initplans.into() }
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

/// Independent initplan outputs the Gather must send to its workers. Do not evaluate
/// the row expressions containing them, or descend into another query level's plans.
fn part_initplans(p: &Plan, out: &mut Vec<Expr>) {
    let mut exprs = Vec::new();
    crate::sql::explain::node_exprs(p, &mut exprs);
    for e in exprs {
        e.walk(&mut |x| {
            if let Expr::Subquery(s) = x
                && s.params.is_empty() && s.func.is_none()
                && matches!(s.kind, SubqueryKind::Scalar | SubqueryKind::Exists | SubqueryKind::Array)
                && !super::reads_enclosing(&s.plan, 0)
                && !out.iter().any(|e| matches!(e, Expr::Subquery(t) if t.id == s.id))
            {
                out.push(x.clone());
            }
        });
    }
    if !matches!(p, Plan::Context { .. } | Plan::Barrier { .. } | Plan::With { .. }) {
        for c in inputs(p) {
            part_initplans(c, out);
        }
    }
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
    let exclusive = if alone && !plan.writes() { worker_parts(gathers, launched.as_ref(), ctx) } else { Vec::new() };
    prepare(gathers, &exclusive, ctx);
    launched
}

/// Attach each concrete Gather boundary's startup work to this execution. EXPLAIN
/// ANALYZE uses it too, without changing its separate worker instrumentation.
pub fn prepare(gathers: &[Gather], exclusive: &[usize], ctx: &ExecCtx) {
    *ctx.worker_parts.borrow_mut() = gathers.iter().filter(|g| g.workers > 0).filter_map(|g| {
        let key = g.part?;
        let alone = exclusive.contains(&key);
        (alone || !g.initplans.is_empty()).then(|| Part { key, initplans: g.initplans.clone(), alone })
    }).collect();
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
