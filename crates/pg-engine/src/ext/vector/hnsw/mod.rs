//! pgvector's hnsw index access method (`hnsw.c`, `hnswbuild.c`, `hnswinsert.c`,
//! `hnswscan.c`, `hnswvacuum.c`, `hnswutils.c`), ported so that builds, inserts, scans and
//! VACUUM go as pgvector's do: the same levels from the backend's generator, the same
//! neighbors (pairing heaps, candidate selection and its tie-breaks), the same page layout,
//! the same results in the same order.
//!
//! An index's state (`graph::Graph`) is kept in memory for every session, keyed by its
//! storage, and written to its records as it changes (`AmStore`), as pgvector writes its
//! pages. A lock guards it: inserts and VACUUM hold it throughout, a scan while it searches.

mod build;
mod graph;
mod insert;
mod mem;
mod pheap;
mod scan;
mod search;
mod types;
mod vacuum;

use crate::Result;
use crate::error::Error;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, AmProps, AmStore, AmRoutine, AmScan, BuildHeap, BuildStats, CostInput, CostOutput, IndexAm};
use crate::types::{DataType, Value};
use graph::Graph;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use types::Support;

pub(super) static ROUTINE: AmRoutine = AmRoutine {
    library: "vector",
    handler: "hnswhandler",
    props: AmProps {
        strategies: 0,
        support: 3,
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
    int_options: &[("m", 16, 2, 100), ("ef_construction", 64, 4, 1000)],
    am: &Hnsw,
};

/// HNSW_DEFAULT_M, HNSW_DEFAULT_EF_CONSTRUCTION.
const DEFAULT_M: i64 = 16;
const DEFAULT_EF_CONSTRUCTION: i64 = 64;

/// A NOTICE.
pub(super) fn notice(msg: String) -> Error {
    let mut e = Error::new("00000", msg);
    e.severity = crate::error::Severity::Notice;
    e
}

/// An index's state, shared by the sessions (None until it is loaded or built).
struct Shared {
    graph: parking_lot::Mutex<Option<Graph>>,
    /// Its records (`evict_dropped`).
    store: AmStore,
}

type Cache = parking_lot::Mutex<HashMap<(u64, Vec<u8>), Arc<Shared>>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Cached indexes of database `instance` whose records start with `prefix` let go
/// (`index_am::evict`).
pub(crate) fn evict(instance: u64, prefix: &[u8]) {
    cache().lock().retain(|k, _| !(k.0 == instance && k.1.starts_with(prefix)));
}

/// The cached graphs of indexes whose records are gone (dropped, or given new storage) let
/// go: a graph holds every vector of its index. Run as an index is loaded or built.
fn evict_dropped(c: &mut HashMap<(u64, Vec<u8>), Arc<Shared>>) {
    c.retain(|_, s| s.store.has_records());
}

/// HnswGetM / HnswGetEfConstruction.
fn m_of(ix: &AmIndex) -> usize {
    ix.int_option("m").unwrap_or(DEFAULT_M) as usize
}

fn ef_construction_of(ix: &AmIndex) -> usize {
    ix.int_option("ef_construction").unwrap_or(DEFAULT_EF_CONSTRUCTION) as usize
}

/// The index's shared state, loaded from its records when no session has it yet (an
/// index without records is one built over no rows: a meta page and an empty page).
fn shared(ix: &AmIndex) -> Result<Arc<Shared>> {
    let id = ix.store.id();
    if let Some(s) = cache().lock().get(&id) {
        return Ok(s.clone());
    }
    let g = match Graph::load(&ix.store.load()?)? {
        Some(g) => g,
        None => {
            let dims = ix.columns.first().map_or(-1, |c| c.ty.typmod());
            let mut g = Graph::new(dims.max(0) as u32, m_of(ix) as u16, ef_construction_of(ix) as u16);
            g.finish_built(graph::NONE);
            g
        }
    };
    let s = Arc::new(Shared { graph: parking_lot::Mutex::new(Some(g)), store: ix.store.clone() });
    let mut c = cache().lock();
    evict_dropped(&mut c);
    Ok(c.entry(id).or_insert(s).clone())
}

/// A setting's number.
fn setting(ctx: &ExecCtx, name: &str, default: f64) -> f64 {
    ctx.sess.settings.borrow().get_str(name).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(default)
}

struct Hnsw;

impl IndexAm for Hnsw {
    /// hnswoptions (the ranges are checked from `ROUTINE.int_options`).
    fn options(&self, _opts: &[String]) -> Result<()> {
        Ok(())
    }

    /// hnswbuild: InitBuildState's checks, the rows inserted in heap order, the graph
    /// written out.
    fn build(&self, ix: &AmIndex, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<BuildStats> {
        crate::ext::vector::seed_build(ctx);
        let sup = Support::of(ix)?;
        let col = ix.columns.first().ok_or_else(|| crate::error::internal("hnsw index without a column"))?;
        if matches!(col.ty, DataType::Varbit(_)) {
            return Err(Error::new("0A000", "type not supported for hnsw index"));
        }
        let dims = col.ty.typmod();
        if dims < 0 {
            return Err(Error::new("22023", "column does not have dimensions"));
        }
        if dims > sup.max_dims() {
            return Err(Error::new("54000", format!("column cannot have more than {} dimensions for hnsw index", sup.max_dims())));
        }
        let (m, ef_construction) = (m_of(ix), ef_construction_of(ix));
        if ef_construction / 2 < m {
            return Err(Error::new("22023", "ef_construction must be greater than or equal to 2 * m"));
        }
        let memory_total = setting(ctx, "maintenance_work_mem", 65536.0) as usize * 1024;
        let mut b = build::Build::new(sup, dims, m, ef_construction, memory_total);
        let mut reltuples = 0.0;
        for row in heap.scan() {
            let row = row?;
            reltuples += 1.0;
            let Some(v) = row.values.first() else { continue };
            if matches!(v, Value::Null) {
                continue;
            }
            if b.insert(&row.dkey, v, ctx)? {
                b.indtuples += 1.0;
            }
        }
        if !b.flushed {
            b.flush()?;
        }
        // (The in-memory graph goes before the records are written.)
        let (mut g, indtuples) = {
            let b = b;
            (b.graph, b.indtuples)
        };
        g.flush(&ix.store, true)?;
        // The index's state from now on; other storage of the same index (before a
        // TRUNCATE or REINDEX) is let go, to be loaded again if it comes back.
        let id = ix.store.id();
        let mut c = cache().lock();
        let base = &id.1[..id.1.len().saturating_sub(4)];
        c.retain(|k, _| !(k.0 == id.0 && k.1.len() == id.1.len() && k.1.starts_with(base)));
        evict_dropped(&mut c);
        c.insert(id, Arc::new(Shared { graph: parking_lot::Mutex::new(Some(g)), store: ix.store.clone() }));
        Ok(BuildStats { heap_tuples: reltuples, index_tuples: indtuples, removed: 0.0 })
    }

    /// hnswinsert.
    fn insert(&self, ix: &AmIndex, dkey: &[u8], values: &[Value], ctx: &ExecCtx) -> Result<()> {
        let Some(v) = values.first().filter(|v| !matches!(v, Value::Null)) else {
            return Ok(());
        };
        let sup = Support::of(ix)?;
        let Some(v) = sup.form(v)? else {
            return Ok(());
        };
        let bytes = sup.bytes(&v);
        let varsize = sup.varsize(&bytes);
        let s = shared(ix)?;
        let mut g = s.graph.lock();
        let g = g.as_mut().ok_or_else(|| crate::error::internal("hnsw index is not valid"))?;
        let r = insert::insert_on_disk(g, &sup, &v, &bytes, varsize, dkey, ef_construction_of(ix), ctx);
        g.flush(&ix.store, false)?;
        r
    }

    /// hnswbulkdelete + hnswvacuumcleanup.
    fn vacuum(&self, ix: &AmIndex, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, _ctx: &ExecCtx) -> Result<BuildStats> {
        let sup = Support::of(ix)?;
        let s = shared(ix)?;
        let mut g = s.graph.lock();
        let g = g.as_mut().ok_or_else(|| crate::error::internal("hnsw index is not valid"))?;
        let r = vacuum::bulk_delete(g, &sup, ef_construction_of(ix), dead);
        g.flush(&ix.store, false)?;
        let stats = r?;
        Ok(BuildStats { heap_tuples: stats.num_index_tuples, index_tuples: stats.num_index_tuples, removed: stats.tuples_removed })
    }

    /// hnswbeginscan + hnswrescan.
    fn scan(&self, ix: &AmIndex, orderbys: &[Value], ctx: &ExecCtx) -> Result<Box<dyn AmScan>> {
        let sup = Support::of(ix)?;
        let s = shared(ix)?;
        let query = orderbys.first().filter(|v| !matches!(v, Value::Null)).cloned();
        Ok(Box::new(scan::Scan::new(s, sup, scan::Settings::of(ctx), query)))
    }

    /// hnswcostestimate over genericcostestimate's results.
    fn cost(&self, ix: &AmIndex, input: &CostInput, ctx: &ExecCtx) -> CostOutput {
        // Never use index without order
        if !input.has_orderbys {
            return CostOutput { startup: f64::INFINITY, total: f64::INFINITY, selectivity: 0.0, correlation: 0.0, pages: 0.0, disabled: true };
        }
        let m = m_of(ix) as i64;
        let ef_search = setting(ctx, "hnsw.ef_search", 40.0) as i64;
        let tuples = input.index_tuples;
        let ratio = if tuples > 0.0 {
            let scaling = 0.55;
            let entry_level = (tuples.ln() * (1.0 / (m as f64).ln())) as i32 as i64;
            let layer0_max = (2 * m * ef_search) as i32;
            let layer0_sel = scaling * tuples.ln() / ((m as f64).ln() * (1.0 + (ef_search as f64).ln()));
            (((entry_level * m) as i32 as f64 + layer0_max as f64 * layer0_sel) / tuples).min(1.0)
        } else {
            1.0
        };
        // Startup cost is cost before returning the first row
        let mut startup = input.total * ratio;
        // Adjust cost if needed since TOAST not included in seq scan cost
        let startup_pages = input.num_index_pages * ratio;
        if startup_pages > input.rel_pages && ratio < 0.5 {
            // Change all page cost from random to sequential
            startup -= startup_pages * (input.spc_random_page_cost - input.spc_seq_page_cost);
            // Remove cost of extra pages
            startup -= (startup_pages - input.rel_pages) * input.spc_seq_page_cost;
        }
        CostOutput { startup, total: input.total, selectivity: input.selectivity, correlation: input.correlation, pages: input.num_index_pages, disabled: false }
    }

    /// The value as the element tuple holds it (HnswFormIndexValue: normalized by a
    /// normalizing class; a bit string with its pad bits clear).
    fn entry_value(&self, ix: &AmIndex, v: &Value) -> Value {
        let Ok(sup) = Support::of(ix) else {
            return v.clone();
        };
        match sup.form(v) {
            Ok(Some(f)) => sup.value(&sup.bytes(&f), sup.dims(v).max(0) as u32),
            _ => v.clone(),
        }
    }

    fn blocks(&self, ix: &AmIndex, _ctx: &ExecCtx) -> Result<u64> {
        let s = shared(ix)?;
        let g = s.graph.lock();
        Ok(g.as_ref().map_or(0, |g| g.blocks()))
    }
}
