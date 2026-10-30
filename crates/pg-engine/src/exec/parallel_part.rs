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
/// with the leader taking part or no workers to launch.
pub fn worker_parts(plan: &Arc<crate::sql::plan::Plan>, ctx: &ExecCtx) -> Vec<usize> {
    let cfg = crate::sql::paths::parallel::ParCfg::read(ctx);
    let launchable = ctx
        .sess
        .settings
        .borrow()
        .get_raw("max_parallel_workers")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(8)
        > 0;
    if cfg.leader || cfg.max_workers == 0 || !launchable {
        return Vec::new();
    }
    crate::sql::explain::gather_parts(plan, ctx)
}
