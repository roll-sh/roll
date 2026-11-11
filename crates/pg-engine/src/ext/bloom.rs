//! bloom (`contrib/bloom`): a signature index access method on the native index interface
//! (`ext::index_am`). Its pages are modelled as bloom lays them out (a metapage, then data
//! pages of fixed-size tuples, the metapage's list of not-full pages, pages emptied by VACUUM
//! marked deleted and reused), so the index's size and the planner's costs are PostgreSQL's.
//! It has amgetbitmap only: a bitmap scan of a bloom index reads the heap with the index's
//! conditions checked on each row (`AmProps::getbitmap`), the rows the signatures would
//! select after their recheck. The signatures themselves aren't kept: nothing a client sees
//! depends on them.

use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, AmProps, AmRoutine, AmScan, AmStore, BuildHeap, BuildStats, CostInput, CostOutput, IndexAm};
use crate::types::Value;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// add_int_reloption's: length (bits, rounded up to words), col1 .. col32 (bits per column).
const OPTIONS: &[(&str, i64, i64, i64)] = &[
    ("length", 80, 1, 4096),
    ("col1", 2, 1, 4095), ("col2", 2, 1, 4095), ("col3", 2, 1, 4095), ("col4", 2, 1, 4095),
    ("col5", 2, 1, 4095), ("col6", 2, 1, 4095), ("col7", 2, 1, 4095), ("col8", 2, 1, 4095),
    ("col9", 2, 1, 4095), ("col10", 2, 1, 4095), ("col11", 2, 1, 4095), ("col12", 2, 1, 4095),
    ("col13", 2, 1, 4095), ("col14", 2, 1, 4095), ("col15", 2, 1, 4095), ("col16", 2, 1, 4095),
    ("col17", 2, 1, 4095), ("col18", 2, 1, 4095), ("col19", 2, 1, 4095), ("col20", 2, 1, 4095),
    ("col21", 2, 1, 4095), ("col22", 2, 1, 4095), ("col23", 2, 1, 4095), ("col24", 2, 1, 4095),
    ("col25", 2, 1, 4095), ("col26", 2, 1, 4095), ("col27", 2, 1, 4095), ("col28", 2, 1, 4095),
    ("col29", 2, 1, 4095), ("col30", 2, 1, 4095), ("col31", 2, 1, 4095), ("col32", 2, 1, 4095),
];

pub(crate) static ROUTINE: AmRoutine = AmRoutine {
    library: "bloom",
    handler: "blhandler",
    props: AmProps {
        strategies: 1,
        support: 2,
        opts_proc: 2,
        can_order: false,
        can_order_by_op: false,
        can_backward: false,
        can_unique: false,
        can_multicol: true,
        optional_key: true,
        search_array: false,
        search_nulls: false,
        storage: false,
        clusterable: false,
        can_include: false,
        gettuple: false,
        getbitmap: true,
    },
    int_options: OPTIONS,
    am: &Bloom,
};

/// The C function `sym` of library `bloom`: the handler, run by the engine itself.
pub(super) fn symbol(sym: &str) -> Option<crate::ext::native::Native> {
    (sym == "blhandler").then(|| crate::ext::native::Native::scalar(|_, _| Err(crate::error::internal("index access method handlers are run by the index itself"))))
}

/// BLCKSZ less the page header and the special space (MAXALIGN'd BloomPageOpaqueData).
const PAGE_ROOM: usize = 8192 - 24 - 8;
/// The first data page (BLOOM_HEAD_BLKNO).
const HEAD: u32 = 1;
/// BloomMetaBlockN: how many not-full pages the metapage lists (the room left after the
/// metadata and BloomOptions' 136 bytes, MAXALIGN'd, in block numbers).
const META_BLOCKS: usize = (8192 - 24 - 8 - 144) / 4;

/// A data page: its tuples (the row's data key and its first indexed value), or deleted.
#[derive(Clone, Default)]
struct Page {
    deleted: bool,
    tuples: Vec<(Vec<u8>, Value)>,
}

/// An index: its pages from HEAD on, and the metapage's not-full list (nStart .. nEnd).
#[derive(Default)]
struct State {
    pages: Vec<Page>,
    not_full: Vec<u32>,
    n_start: usize,
    /// Pages VACUUM found deleted (the free space map's), reused before extending.
    free: Vec<u32>,
}

impl State {
    fn page(&mut self, blk: u32) -> &mut Page {
        &mut self.pages[(blk - HEAD) as usize]
    }

    fn blocks(&self) -> u64 {
        1 + self.pages.len() as u64
    }
}

/// sizeOfBloomTuple: the heap pointer and the signature's words.
fn tuple_size(ix: &AmIndex) -> usize {
    let bits = ix.int_option("length").unwrap_or(80) as usize;
    6 + 2 * bits.div_ceil(16)
}

fn capacity(ix: &AmIndex) -> usize {
    (PAGE_ROOM / tuple_size(ix)).max(1)
}

type Cache = Mutex<HashMap<(u64, Vec<u8>), Arc<Mutex<State>>>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Cached indexes of database `instance` whose records start with `prefix` let go
/// (`index_am::evict`).
pub(crate) fn evict(instance: u64, prefix: &[u8]) {
    cache().lock().retain(|k, _| !(k.0 == instance && k.1.starts_with(prefix)));
}

/// An index's state: cached, else read from its records (a page per record; key 0 the
/// metapage).
fn state(store: &AmStore) -> Result<Arc<Mutex<State>>> {
    if let Some(s) = cache().lock().get(&store.id()) {
        return Ok(s.clone());
    }
    let mut st = State::default();
    for (k, v) in store.load()? {
        let blk = u32::from_be_bytes(k[..4].try_into().unwrap_or_default());
        let row = crate::storage::row::decode_row(&v)?;
        if blk == 0 {
            st.n_start = row.first().and_then(Value::as_i64).unwrap_or(0) as usize;
            st.not_full = row[1..].iter().filter_map(Value::as_i64).map(|b| b as u32).collect();
            continue;
        }
        let mut p = Page { deleted: matches!(row.first(), Some(Value::Bool(true))), tuples: Vec::new() };
        for pair in row[1..].chunks(2) {
            if let [Value::Bytea(k), v] = pair {
                p.tuples.push((k.to_vec(), v.clone()));
            }
        }
        let i = (blk - HEAD) as usize;
        if st.pages.len() <= i {
            st.pages.resize(i + 1, Page::default());
        }
        st.pages[i] = p;
    }
    st.free = (0..st.pages.len()).filter(|&i| st.pages[i].deleted).map(|i| i as u32 + HEAD).collect();
    let s = Arc::new(Mutex::new(st));
    Ok(cache().lock().entry(store.id()).or_insert(s).clone())
}

/// Write pages `blocks` (and the metapage).
fn save(store: &AmStore, st: &State, blocks: &[u32]) -> Result<()> {
    let mut meta = vec![Value::Int8(st.n_start as i64)];
    meta.extend(st.not_full.iter().map(|b| Value::Int8(*b as i64)));
    let mut writes = vec![(0u32.to_be_bytes().to_vec(), Some(crate::storage::row::encode_row(&meta)))];
    for &blk in blocks {
        let p = &st.pages[(blk - HEAD) as usize];
        let mut row = vec![Value::Bool(p.deleted)];
        for (k, v) in &p.tuples {
            row.push(Value::Bytea(k.clone().into()));
            row.push(v.clone());
        }
        writes.push((blk.to_be_bytes().to_vec(), Some(crate::storage::row::encode_row(&row))));
    }
    store.write(writes)
}

struct Bloom;

impl IndexAm for Bloom {
    fn options(&self, _opts: &[String]) -> Result<()> {
        Ok(())
    }

    /// blbuild: every row's tuple (NULLs set no bits but are indexed), pages filled in heap
    /// order; the metapage's not-full list stays empty.
    fn build(&self, ix: &AmIndex, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<BuildStats> {
        let cap = capacity(ix);
        let mut st = State::default();
        let mut cur = Page::default();
        let mut n = 0f64;
        for row in heap.scan() {
            ctx.check()?;
            let row = row?;
            if cur.tuples.len() == cap {
                st.pages.push(std::mem::take(&mut cur));
            }
            cur.tuples.push((row.dkey, row.values.into_iter().next().unwrap_or(Value::Null)));
            n += 1.0;
        }
        if !cur.tuples.is_empty() {
            st.pages.push(cur);
        }
        let blocks: Vec<u32> = (0..st.pages.len() as u32).map(|i| i + HEAD).collect();
        save(&ix.store, &st, &blocks)?;
        // (The index's other storage generations, a TRUNCATE's or REINDEX's earlier ones,
        // leave the cache: a rollback reads them back.)
        let id = ix.store.id();
        let base = &id.1[..id.1.len().saturating_sub(4)];
        let mut c = cache().lock();
        c.retain(|k, _| !(k.0 == id.0 && k.1.len() == id.1.len() && k.1.starts_with(base)));
        c.insert(id, Arc::new(Mutex::new(st)));
        drop(c);
        Ok(BuildStats { heap_tuples: n, index_tuples: n, removed: 0.0 })
    }

    /// blinsert: the first page of the metapage's not-full list with room (a deleted one is
    /// empty again), else a new page (a free one first), which becomes the only not-full one.
    fn insert(&self, ix: &AmIndex, dkey: &[u8], values: &[Value], _ctx: &ExecCtx) -> Result<()> {
        let cap = capacity(ix);
        let s = state(&ix.store)?;
        let mut st = s.lock();
        let tuple = (dkey.to_vec(), values.first().cloned().unwrap_or(Value::Null));
        let mut i = st.n_start;
        while i < st.not_full.len() {
            let blk = st.not_full[i];
            let p = st.page(blk);
            if p.deleted {
                *p = Page::default();
            }
            if p.tuples.len() < cap {
                p.tuples.push(tuple);
                st.n_start = i;
                return save(&ix.store, &st, &[blk]);
            }
            i += 1;
        }
        let blk = if st.free.is_empty() {
            st.pages.push(Page::default());
            st.pages.len() as u32 - 1 + HEAD
        } else {
            st.free.remove(0)
        };
        *st.page(blk) = Page { deleted: false, tuples: vec![tuple] };
        st.n_start = 0;
        st.not_full = vec![blk];
        save(&ix.store, &st, &[blk])
    }

    /// blbulkdelete + blvacuumcleanup: dead tuples removed (a page left empty is deleted),
    /// the pages with room for a tuple listed in the metapage, deleted pages free for reuse.
    /// (Without a dead tuple there is no bulk delete: nothing changes.)
    fn vacuum(&self, ix: &AmIndex, dead: &mut dyn FnMut(&[u8], &Value) -> Result<bool>, ctx: &ExecCtx) -> Result<BuildStats> {
        let cap = capacity(ix);
        let s = state(&ix.store)?;
        let mut st = s.lock();
        let mut gone: Vec<(u32, Vec<bool>)> = Vec::new();
        let (mut kept, mut removed) = (0f64, 0f64);
        for (i, p) in st.pages.iter().enumerate() {
            ctx.check()?;
            if p.deleted {
                continue;
            }
            let mut marks = Vec::with_capacity(p.tuples.len());
            for (k, v) in &p.tuples {
                let d = dead(k, v)?;
                if d {
                    removed += 1.0;
                } else {
                    kept += 1.0;
                }
                marks.push(d);
            }
            gone.push((i as u32 + HEAD, marks));
        }
        if removed == 0.0 {
            return Ok(BuildStats { heap_tuples: kept, index_tuples: kept, removed });
        }
        let mut changed = Vec::new();
        let mut not_full = Vec::new();
        for (blk, marks) in gone {
            let p = st.page(blk);
            if marks.iter().any(|d| *d) {
                let mut m = marks.iter();
                p.tuples.retain(|_| !m.next().copied().unwrap_or(false));
                if p.tuples.is_empty() {
                    p.deleted = true;
                }
                changed.push(blk);
            }
            if !p.deleted && p.tuples.len() < cap && not_full.len() < META_BLOCKS {
                not_full.push(blk);
            }
        }
        st.n_start = 0;
        st.not_full = not_full;
        st.free = (0..st.pages.len()).filter(|&i| st.pages[i].deleted).map(|i| i as u32 + HEAD).collect();
        save(&ix.store, &st, &changed)?;
        Ok(BuildStats { heap_tuples: kept, index_tuples: kept, removed })
    }

    fn scan(&self, _ix: &AmIndex, _orderbys: &[Value], _ctx: &ExecCtx) -> Result<Box<dyn AmScan>> {
        Err(crate::error::internal("bloom indexes have bitmap scans only"))
    }

    /// blcostestimate: every index tuple is visited.
    fn cost_index_tuples(&self, _ix: &AmIndex, index_tuples: f64) -> Option<f64> {
        Some(index_tuples)
    }

    /// blcostestimate: genericcostestimate's.
    fn cost(&self, _ix: &AmIndex, input: &CostInput, _ctx: &ExecCtx) -> CostOutput {
        CostOutput {
            startup: input.startup,
            total: input.total,
            selectivity: input.selectivity,
            correlation: input.correlation,
            pages: input.num_index_pages,
            disabled: false,
        }
    }

    fn blocks(&self, ix: &AmIndex, _ctx: &ExecCtx) -> Result<u64> {
        Ok(state(&ix.store)?.lock().blocks())
    }
}
