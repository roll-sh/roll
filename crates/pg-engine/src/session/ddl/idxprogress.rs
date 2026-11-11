//! pg_stat_progress_create_index: an index build's report, as DefineIndex, index_build and
//! btbuild make it (`commands/progress.h`, `access/nbtree.h`), in the session's progress
//! slot other backends read (`state::Progress`).

use super::Ddl;
use crate::catalog::{Index, Table};
use crate::session::state::Progress;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// PROGRESS_CREATEIDX_* (0-based parameter numbers).
pub(super) const COMMAND: usize = 0;
const INDEX_OID: usize = 6;
const ACCESS_METHOD_OID: usize = 8;
const PHASE: usize = 9;
const SUBPHASE: usize = 10;
const TUPLES_TOTAL: usize = 11;
const TUPLES_DONE: usize = 12;
const BLOCKS_TOTAL: usize = 15;
const BLOCKS_DONE: usize = 16;

/// PROGRESS_CREATEIDX_PHASE_BUILD.
const PHASE_BUILD: i64 = 2;
/// PROGRESS_CREATEIDX_SUBPHASE_INITIALIZE, and btree's PROGRESS_BTREE_PHASE_*.
const SUB_INITIALIZE: i64 = 1;
const SUB_TABLESCAN: i64 = 2;
const SUB_PERFORMSORT_1: i64 = 3;
const SUB_LEAF_LOAD: i64 = 5;

/// One build's report: started (with the table, the index and its method, phase "building
/// index: initializing") unless the session reports another command already, ended when
/// this drops. Reports nothing when the session's builds report none (`index_progress`).
pub(super) struct IndexProgress {
    p: Option<Arc<Progress>>,
    started: bool,
    /// Rows per block, for the scan's blocks done.
    per_block: u64,
    blocks: i64,
    /// btree's subphases are reported (other methods' builds have none).
    btree: bool,
}

impl IndexProgress {
    pub(super) fn start(d: &Ddl, t: &Table, ix: &Index, blocks: Option<u32>) -> IndexProgress {
        let sess = d.ctx.sess;
        let cmd = sess.index_progress.get();
        let p = sess.progress.clone();
        let running = p.command.load(Ordering::Acquire);
        if cmd == 0 || (running != 0 && running != Progress::CREATE_INDEX) {
            return IndexProgress { p: None, started: false, per_block: 1, blocks: 0, btree: false };
        }
        let started = running == 0;
        if started {
            p.start(Progress::CREATE_INDEX, t.oid);
            p.set(COMMAND, cmd);
        }
        // (DefineIndex reports a new index's oid after index_create, which builds it: 0 while a
        // CREATE INDEX builds; REINDEX and the concurrent build's later phases have it.)
        p.set(INDEX_OID, if p.params[COMMAND].load(Ordering::Relaxed) == 1 { 0 } else { ix.oid as i64 });
        p.set(ACCESS_METHOD_OID, crate::catalog::system::access_method_oid(&ix.method) as i64);
        p.set(PHASE, PHASE_BUILD);
        p.set(SUBPHASE, SUB_INITIALIZE);
        for i in [TUPLES_TOTAL, TUPLES_DONE, BLOCKS_TOTAL, BLOCKS_DONE] {
            p.set(i, 0);
        }
        let blocks = blocks.unwrap_or(0) as i64;
        let stats = d.cat.relstats.get(&t.oid).filter(|s| s.pages > 0 && s.tuples > 0.0);
        let per_block = stats.map_or(64, |s| ((s.tuples / s.pages as f64) as u64).max(1));
        IndexProgress { p: Some(p), started, per_block, blocks, btree: ix.method == "btree" }
    }

    pub(super) fn set(&self, i: usize, v: i64) {
        if let Some(p) = &self.p {
            p.set(i, v);
        }
    }

    fn sub(&self, phase: i64) {
        if self.btree {
            self.set(SUBPHASE, phase);
        }
    }

    /// The table scan begins (btree's "scanning table"): its blocks. (While it reads the
    /// first block, heapam_scan_get_blocks_done counts all of them done: its block isn't past
    /// the start one, so it counts from the end around.)
    pub(super) fn scanning(&self) {
        self.sub(SUB_TABLESCAN);
        self.set(BLOCKS_TOTAL, self.blocks);
        self.set(BLOCKS_DONE, self.blocks);
    }

    /// `rows` rows of the table scanned so far, checked per row: the blocks they fill.
    pub(super) fn scanned(&self, rows: u64) {
        if self.p.is_some() && rows.is_multiple_of(self.per_block) {
            self.scanned_now(rows);
        }
    }

    /// `rows` rows of the table scanned so far: the blocks they fill (never past the total).
    pub(super) fn scanned_now(&self, rows: u64) {
        let done = (rows / self.per_block) as i64;
        self.set(BLOCKS_DONE, if done == 0 { self.blocks } else { done.min(self.blocks) });
    }

    /// The scan is done (`tuples` entries made): sorting, then loading them.
    pub(super) fn scanned_all(&self, tuples: u64) {
        self.set(BLOCKS_DONE, self.blocks);
        self.set(TUPLES_TOTAL, tuples as i64);
        self.sub(SUB_PERFORMSORT_1);
    }

    /// The sorted entries go in (btree's "loading tuples in tree"), all of them at once.
    pub(super) fn loaded(&self, tuples: u64) {
        self.sub(SUB_LEAF_LOAD);
        self.set(TUPLES_DONE, tuples as i64);
    }
}

impl Drop for IndexProgress {
    fn drop(&mut self) {
        if self.started
            && let Some(p) = &self.p
        {
            p.end();
        }
    }
}

/// While this lives, the session's builds report `cmd` as pg_stat_progress_create_index's
/// command (0: none).
pub(super) struct Command<'s> {
    cell: &'s std::cell::Cell<i64>,
    prev: i64,
}

pub(super) fn command(cell: &std::cell::Cell<i64>, cmd: i64) -> Command<'_> {
    Command { prev: cell.replace(cmd), cell }
}

impl Drop for Command<'_> {
    fn drop(&mut self) {
        self.cell.set(self.prev);
    }
}
