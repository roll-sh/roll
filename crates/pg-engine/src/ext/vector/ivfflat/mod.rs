//! The ivfflat index access method of pgvector 0.8.7 (`ivfflat.c`, `ivfbuild.c`,
//! `ivfinsert.c`, `ivfscan.c`, `ivfvacuum.c`, `ivfkmeans.c`, `ivfutils.c`), natively, with
//! pgvector's results: the same sample, centers, lists, page layout and order of entries,
//! and scans returning rows in the same order, ties included.
//!
//! - `page`: the pages (meta, lists, entries) with PostgreSQL's free space arithmetic, their
//!   records in the index's store and the process-wide cache of each index.
//! - `build`: sampling (`sampling`: PostgreSQL's BlockSampler and reservoir), k-means
//!   (`kmeans`), the assignment and the sort by list (`tuplesort`: PostgreSQL's quicksort
//!   and external sort order), the pages laid out.
//! - `modify`: inserts and VACUUM.
//! - `scan`: list selection (`pairingheap`) and the candidates sorted by distance, iterative
//!   scans included.
//! - `types`: the type support (vector, halfvec, bit) and the class's support functions.

mod build;
mod kmeans;
mod modify;
pub(super) mod page;
mod pairingheap;
mod sampling;
mod scan;
mod tuplesort;
mod types;

use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, AmProps, AmRoutine, AmScan, BuildHeap, BuildStats, CostInput, CostOutput, IndexAm};
use crate::types::Value;

pub(super) static ROUTINE: AmRoutine = AmRoutine {
    library: "vector",
    handler: "ivfflathandler",
    props: AmProps {
        strategies: 0,
        support: 5,
        opts_proc: 0,
        can_order: false,
        can_order_by_op: true,
        can_backward: false,
        can_unique: false,
        can_multicol: false,
        optional_key: true,
        search_array: false,
        search_nulls: false,
        storage: false,
        clusterable: false,
        can_include: false,
        gettuple: true,
        getbitmap: false,
    },
    int_options: &[("lists", 100, 1, 32768)],
    am: &Ivfflat,
};

struct Ivfflat;

impl IndexAm for Ivfflat {
    /// `ivfflatoptions` (the engine checks `lists` against ROUTINE.int_options).
    fn options(&self, _opts: &[String]) -> Result<()> {
        Ok(())
    }

    fn build(&self, ix: &AmIndex, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<BuildStats> {
        build::build(ix, heap, ctx)
    }

    fn insert(&self, ix: &AmIndex, dkey: &[u8], values: &[Value], ctx: &ExecCtx) -> Result<()> {
        modify::insert(ix, dkey, values, ctx)
    }

    fn vacuum(&self, ix: &AmIndex, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, ctx: &ExecCtx) -> Result<BuildStats> {
        modify::vacuum(ix, dead, ctx)
    }

    fn scan(&self, ix: &AmIndex, orderbys: &[Value], ctx: &ExecCtx) -> Result<Box<dyn AmScan>> {
        Ok(Box::new(scan::begin(ix, orderbys, ctx)?))
    }

    /// `ivfflatcostestimate`.
    fn cost(&self, ix: &AmIndex, input: &CostInput, ctx: &ExecCtx) -> CostOutput {
        // Never use index without order
        if !input.has_orderbys {
            return CostOutput { startup: f64::INFINITY, total: f64::INFINITY, selectivity: 0.0, correlation: 0.0, pages: 0.0, disabled: true };
        }
        let lists = page::with(&ix.store, |idx| Ok(idx.as_ref().and_then(|i| i.meta().ok()).map(|m| m.0))).ok().flatten();
        let mut out = CostOutput {
            startup: input.startup,
            total: input.total,
            selectivity: input.selectivity,
            correlation: input.correlation,
            pages: input.num_index_pages,
            disabled: false,
        };
        let Some(lists) = lists else {
            return out;
        };
        let sequential_ratio = 0.5;
        let probes = build::setting(ctx, "ivfflat.probes", 1);
        // Get the ratio of lists that we need to visit
        let ratio = (probes as f64 / lists as f64).min(1.0);
        let (random, seq) = (input.spc_random_page_cost, input.spc_seq_page_cost);
        // Change some page cost from random to sequential
        out.total -= sequential_ratio * input.num_index_pages * (random - seq);
        // Startup cost is cost before returning the first row
        out.startup = out.total * ratio;
        // Adjust cost if needed since TOAST not included in seq scan cost
        let startup_pages = input.num_index_pages * ratio;
        if startup_pages > input.rel_pages && ratio < 0.5 {
            // Change rest of page cost from random to sequential
            out.startup -= (1.0 - sequential_ratio) * startup_pages * (random - seq);
            // Remove cost of extra pages
            out.startup -= (startup_pages - input.rel_pages) * seq;
        }
        out
    }

    /// RelationGetNumberOfBlocks of the index.
    fn blocks(&self, ix: &AmIndex, _ctx: &ExecCtx) -> Result<u64> {
        page::with(&ix.store, |idx| Ok(idx.as_ref().map_or(0, |i| i.pages.len() as u64)))
    }
}
