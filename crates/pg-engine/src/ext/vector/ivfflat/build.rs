//! ivfflat's build (`ivfbuild.c`): sample rows as ANALYZE does, k-means over the sample,
//! the meta and list pages, then each row assigned to its nearest center, sorted by list
//! (tuplesort's order) and laid out list after list on fresh entry pages.

use super::kmeans::{check_memory_usage, kmeans, vector_array_size};
use super::page::{self, INVALID, Index, List, ListInfo, Page, Tuple, maxalign};
use super::sampling::{BlockSampler, Reservoir};
use super::tuplesort::Sorter;
use super::types::{Kind, Procs, check_expected_dim, check_norm};
use crate::Result;
use crate::error::{Error, code};
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, BuildHeap, BuildStats};
use crate::types::{DataType, Value};
use std::sync::Arc;

/// MaxHeapTuplesPerPage (BLCKSZ 8192).
const MAX_HEAP_TUPLES_PER_PAGE: i64 = 291;
/// IVFFLAT_DEFAULT_LISTS.
const DEFAULT_LISTS: i64 = 100;

/// An integer setting (maintenance_work_mem, work_mem: KB).
pub(super) fn setting(ctx: &ExecCtx, name: &str, default: i64) -> i64 {
    ctx.sess.settings.borrow().get_str(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Whether in-line compression is lz4's (default_toast_compression).
pub(super) fn lz4(ctx: &ExecCtx) -> bool {
    ctx.sess.settings.borrow().get_str("default_toast_compression") == Some("lz4")
}

/// `IvfflatBuildState`'s settings.
struct Build {
    procs: Procs,
    kind: Kind,
    dims: i32,
    lists: usize,
    itemsize: usize,
    memory_used: usize,
    mwm: i64,
}

/// A row on its way to its list: the build sort's tuple (list, tid, value).
struct Assigned {
    list: i32,
    dkey: Arc<[u8]>,
    val: Value,
    orig: Option<Value>,
}

/// `ivfflatbuild`.
pub(super) fn build(ix: &AmIndex, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<BuildStats> {
    // (-DIVFFLAT_BENCH: SeedRandom(42))
    crate::ext::vector::seed_build(ctx);
    let b = init_build_state(ix, ctx)?;
    let centers = compute_centers(&b, heap, ctx)?;
    // Create pages
    let mut index = Index { pages: Vec::new() };
    // CreateMetaPage
    index.new_page(Page::Meta { dims: b.dims, lists: b.lists as i32, kind: b.kind });
    let list_info = create_list_pages(&mut index, centers, &b);
    let stats = create_entry_pages(&mut index, &list_info, &b, heap, ctx)?;
    index.save(&ix.store, 0..index.pages.len() as u32)?;
    page::install(&ix.store, index);
    Ok(stats)
}

/// `InitBuildState`: the column's checks, the class's functions, the centers' memory.
fn init_build_state(ix: &AmIndex, ctx: &ExecCtx) -> Result<Build> {
    let col = ix.columns.first().ok_or_else(|| crate::error::internal("ivfflat index without a column"))?;
    let kind = Kind::of(col);
    let lists = ix.int_option("lists").unwrap_or(DEFAULT_LISTS) as usize;
    let dims = col.ty.typmod();
    // Disallow varbit since require fixed dimensions
    if matches!(col.ty, DataType::Varbit(_)) {
        return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "type not supported for ivfflat index"));
    }
    // Require column to have dimensions to be indexed
    if dims < 0 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "column does not have dimensions"));
    }
    if dims > kind.max_dimensions() {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("column cannot have more than {} dimensions for ivfflat index", kind.max_dimensions()),
        ));
    }
    let procs = Procs::of(ix)?;
    // Require more than one dimension for spherical k-means
    if procs.kmeans_norm.is_some() && dims == 1 {
        return Err(Error::new(code::INVALID_PARAMETER_VALUE, "dimensions must be greater than one for this opclass"));
    }
    let itemsize = kind.item_size(dims);
    let mwm = setting(ctx, "maintenance_work_mem", 65536);
    let memory_used = vector_array_size(lists, itemsize);
    check_memory_usage(memory_used, mwm)?;
    Ok(Build { procs, kind, dims, lists, itemsize, memory_used, mwm })
}

/// `ComputeCenters`: sample, then k-means.
fn compute_centers(b: &Build, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<Vec<Value>> {
    let max_tuples = heap.blocks() as i64 * MAX_HEAP_TUPLES_PER_PAGE;
    // Target 50 samples per list, with at least 10000 samples
    let num_samples = (b.lists as i64 * 50).max(10000);
    // Save memory since will not have more than max tuples
    let num_samples = num_samples.min(max_tuples).max(1) as usize;
    let memory_used = b.memory_used + vector_array_size(num_samples, b.itemsize);
    check_memory_usage(memory_used, b.mwm)?;
    let samples = sample_rows(b, heap, num_samples, ctx)?;
    if samples.len() < b.lists {
        ctx.notice(
            crate::session::notice("ivfflat index created with little data")
                .detail("This will cause low recall.")
                .hint("Drop the index until the table has more data."),
        );
    }
    kmeans(&samples, b.lists, b.dims as usize, &b.procs, memory_used, b.mwm, ctx)
}

/// `SampleRows`: the rows of the blocks BlockSampler picks, through a reservoir (as
/// ANALYZE), then normalized for spherical k-means.
fn sample_rows(b: &Build, heap: &mut dyn BuildHeap, targsamples: usize, ctx: &ExecCtx) -> Result<Vec<Value>> {
    let totalblocks = heap.blocks();
    let mut bs = BlockSampler::new(totalblocks, targsamples as i32, ctx.sess.global_random_u32());
    let mut rstate = Reservoir::new(targsamples as i32, ctx.sess.global_random_u32());
    // (BlockSampler draws from its own generator: its blocks are known up front.)
    let mut picked = vec![false; totalblocks as usize];
    while bs.has_more() {
        picked[bs.next() as usize] = true;
    }
    let mut samples: Vec<Value> = Vec::with_capacity(targsamples.min(1 << 16));
    let mut samplerows = 0f64;
    let mut rowstoskip = -1f64;
    for row in heap.scan() {
        let row = row?;
        if !picked.get(row.block as usize).copied().unwrap_or(false) {
            continue;
        }
        // Skip nulls
        let Some(value) = row.values.into_iter().next().filter(|v| !v.is_null()) else {
            continue;
        };
        // AddSample: check dimensions match index
        check_expected_dim(b.dims, b.kind.dimensions(&value))?;
        // Check with KMEANS_NORM_PROC that the value can be normalized since spherical
        // distance function expects unit vectors
        if let Some(norm) = b.procs.kmeans_norm
            && !check_norm(norm, &value)
        {
            continue;
        }
        if samples.len() < targsamples {
            samples.push(value);
        } else {
            if rowstoskip < 0.0 {
                rowstoskip = rstate.next_s(samplerows, targsamples as i32);
            }
            if rowstoskip <= 0.0 {
                let k = (targsamples as f64 * rstate.rand.fract()) as usize;
                samples[k] = value;
            }
            rowstoskip -= 1.0;
        }
        // Increment after reservoir_get_next_S
        samplerows += 1.0;
    }
    // Normalize if needed
    if b.procs.kmeans_norm.is_some() {
        for s in samples.iter_mut() {
            let n = b.kind.normalize(s);
            check_expected_dim(b.dims, b.kind.dimensions(&n))?;
            *s = n;
        }
    }
    Ok(samples)
}

/// `CreateListPages`: a list item per center, from block 1 on; where each list is.
fn create_list_pages(index: &mut Index, centers: Vec<Value>, b: &Build) -> Vec<ListInfo> {
    let list_size = 8 + maxalign(b.itemsize);
    let mut blkno = index.new_page(Page::Lists { next: INVALID, lists: Vec::new() });
    let mut info = Vec::with_capacity(centers.len());
    for center in centers {
        let Page::Lists { lists, .. } = &index.pages[blkno as usize] else { unreachable!() };
        // Ensure free space
        if page::free_space(lists.len(), lists.len() * list_size) < list_size {
            let next = index.new_page(Page::Lists { next: INVALID, lists: Vec::new() });
            index.pages[blkno as usize].set_next(next);
            blkno = next;
        }
        let Page::Lists { lists, .. } = &mut index.pages[blkno as usize] else { unreachable!() };
        lists.push(List { start: INVALID, insert: INVALID, center });
        info.push(ListInfo { blkno, off: lists.len() - 1 });
    }
    info
}

/// The bytes a build sort tuple takes (`MAXALIGN(t_len)` of the minimal tuple of list
/// int4, tid, value: the value packed to a 1-byte header when short, else int-aligned).
fn sort_tuplen(kind: Kind, v: &Value) -> i64 {
    let vs = kind.varsize(v);
    let data = if vs - 3 <= 127 { 10 + vs - 3 } else { 12 + vs };
    maxalign(16 + data) as i64
}

/// `CreateEntryPages`: `AssignTuples` (each row to its nearest center), the sort by list,
/// then `InsertTuples`.
fn create_entry_pages(index: &mut Index, list_info: &[ListInfo], b: &Build, heap: &mut dyn BuildHeap, ctx: &ExecCtx) -> Result<BuildStats> {
    let centers: Vec<Value> = index.lists().map(|(_, l)| l.center.clone()).collect();
    // (The centers' bytes, for the four-row kernels.)
    let center_bytes: Option<Vec<&[u8]>> = centers
        .iter()
        .map(|c| match c {
            Value::Ext(_, b) => Some(&b[..]),
            _ => None,
        })
        .collect();
    let mut reltuples = 0f64;
    let mut items = Vec::new();
    for row in heap.scan() {
        let row = row?;
        reltuples += 1.0;
        // Skip nulls
        let Some(value) = row.values.into_iter().next().filter(|v| !v.is_null()) else {
            continue;
        };
        // AddTupleToSort: normalize if needed
        let (value, orig) = match b.procs.norm {
            Some(norm) => {
                if !check_norm(norm, &value) {
                    continue;
                }
                (b.kind.normalize(&value), Some(value))
            }
            None => (value, None),
        };
        // Check dimensions match index
        check_expected_dim(b.dims, b.kind.dimensions(&value))?;
        // Find the list that minimizes the distance (four centers at a time where the kernel
        // has that form: the same distances, taken in the same order)
        let mut min = f64::MAX;
        let mut closest = 0;
        let mut take = |i: usize, d: f64| {
            if d < min {
                min = d;
                closest = i;
            }
        };
        let mut i = 0;
        if let (Some(d4), Value::Ext(_, vb), Some(cb)) = (b.procs.distance4, &value, center_bytes.as_ref()) {
            while let Some(c) = cb.get(i..i + 4) {
                for (j, d) in d4(&[c[0], c[1], c[2], c[3]], vb).into_iter().enumerate() {
                    take(i + j, d);
                }
                i += 4;
            }
        }
        for (j, c) in centers.iter().enumerate().skip(i) {
            take(j, (b.procs.distance)(&value, c));
        }
        items.push(Assigned { list: closest as i32, dkey: row.dkey.into(), val: value, orig });
    }
    let indtuples = items.len() as f64;
    // tuplesort_performsort: by list (int4), with maintenance_work_mem
    let kind = b.kind;
    let sorted = Sorter::new(b.mwm).sort(items, |t| sort_tuplen(kind, &t.val), |x, y| x.list.cmp(&y.list));
    insert_tuples(index, list_info, sorted, b, ctx)?;
    Ok(BuildStats { heap_tuples: reltuples, index_tuples: indtuples, removed: 0.0 })
}

/// `InsertTuples`: each list's tuples on a new page, more appended as they fill; the list's
/// start and insert pages set.
fn insert_tuples(index: &mut Index, list_info: &[ListInfo], sorted: Vec<Assigned>, b: &Build, ctx: &ExecCtx) -> Result<()> {
    let lz4 = lz4(ctx);
    let mut tuples = sorted.into_iter().peekable();
    for (i, at) in list_info.iter().enumerate() {
        ctx.check()?;
        let start = index.new_page(Page::entries());
        let mut cur = start;
        // Get all tuples for list
        while let Some(t) = tuples.next_if(|t| t.list == i as i32) {
            let size = page::tuple_size(b.kind, &t.val, lz4);
            // Check for free space
            if index.pages[cur as usize].free() < size {
                // IvfflatAppendPage
                let next = index.new_page(Page::entries());
                index.pages[cur as usize].set_next(next);
                cur = next;
            }
            index.pages[cur as usize].add(Tuple { dkey: t.dkey, val: t.val, orig: t.orig, size: size as u32 });
        }
        // Set the start and insert pages
        index.update_list(*at, cur, INVALID, start);
    }
    Ok(())
}
