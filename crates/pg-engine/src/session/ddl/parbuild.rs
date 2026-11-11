//! Parallel btree builds (PostgreSQL's `_bt_parallel_build`): a big table's rows are cut into
//! key ranges that the leader and `max_parallel_maintenance_workers`' workers read, each
//! decoding its rows, measuring them (`Ddl::measure_rows_with`'s numbers) and sorting its
//! entries; the sorted entries go in as linked runs (`Txn::put_index_runs`), and a unique
//! index's duplicates are found merging them, the one reported the serial build's (the key
//! whose second row comes first in the table's order). Only for an index of plain columns
//! without a predicate on a table too big to model (`Ddl::measures_in_build`).
//!
//! BRIN and GIN builds of such a table measure it the same way (`Ddl::measure_am_parallel`:
//! no entries to write, each participant's part of the access method's measure merged).

use super::Ddl;
use super::vacuum::RowMeasure;
use crate::catalog::btsim::Key;
use crate::catalog::{Index, Table, relsize};
use crate::error::Result;
use crate::sql::expr::Expr;
use crate::sql::plan::BoundIndex;
use crate::storage::keys::KeyColl;
use crate::types::{DataType, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use super::idxprogress::IndexProgress;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// One participant's index entries: the entry keys back to back in `data`; each entry's
/// (its key's 16 bytes after the index's prefix, offset, key length | tail length << 16 |
/// `CHECKED`) in `ents`, sorted by key at the end. Its value, the row's data key, is the
/// table's prefix and the key's tail (`index_entry_key`).
#[derive(Default)]
struct Entries {
    data: Vec<u8>,
    ents: Vec<Ent>,
}

/// An entry whose key values take part in the unique check (none null, or NULLS NOT
/// DISTINCT).
const CHECKED: u32 = 1 << 31;

/// (Two words, not a `u128`: its 16-byte alignment made an entry 32 bytes.)
type Ent = ([u64; 2], u32, u32);

impl Entries {
    fn push(&mut self, k: &[u8], tail: usize, checked: bool) {
        let at = self.data.len();
        self.data.extend_from_slice(k);
        self.push_at(at, tail, checked);
    }

    /// An entry whose key was appended to `data` from `at` on.
    fn push_at(&mut self, at: usize, tail: usize, checked: bool) {
        let k = &self.data[at..];
        let mut w = [0u8; 16];
        let rest = k.get(8..).unwrap_or(&[]);
        let n = rest.len().min(16);
        w[..n].copy_from_slice(&rest[..n]);
        let lens = k.len() as u32 | (tail as u32) << 16 | if checked { CHECKED } else { 0 };
        let word = |b: &[u8]| u64::from_be_bytes(b.try_into().unwrap());
        self.ents.push(([word(&w[..8]), word(&w[8..])], at as u32, lens));
    }

    fn key(&self, e: Ent) -> &[u8] {
        &self.data[e.1 as usize..(e.1 + (e.2 & 0xffff)) as usize]
    }

    fn tail(&self, e: Ent) -> &[u8] {
        let k = self.key(e);
        &k[k.len() - ((e.2 & !CHECKED) >> 16) as usize..]
    }

    /// By each key's first 16 bytes past the index's prefix, then the keys themselves where
    /// those tie. (Eight bytes tied for an integer column's keys a few hundred apart, and every
    /// tie read two keys at random places in `data`.)
    fn sort(&mut self) {
        let mut ents = std::mem::take(&mut self.ents);
        ents.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| self.key(*a).cmp(self.key(*b))));
        self.ents = ents;
    }
}

/// A chunk's numbers, merged in the chunks' order.
#[derive(Default)]
struct Part {
    rows: usize,
    /// The heap's tuples' sizes, in order (`relsize::heap_pages` packs them; 4 bytes each, as
    /// a big build holds a million).
    sizes: Vec<u32>,
}

/// What a participant gathered over its chunks.
#[derive(Default)]
struct Gathered {
    entries: Entries,
    /// The measure's (sort key, key bytes, index tuple size) of each entry.
    keys: Vec<super::vacuum::MeasureKey>,
    parts: Vec<(usize, Part)>,
    /// The first entry in index order past a btree's third of a page (`rowsize`): its key,
    /// size and row's data key.
    oversized: Option<(Vec<u8>, usize, Vec<u8>)>,
    /// A modelled table's: each row's key in the index's model, with its data key.
    model: Vec<(Key, Vec<u8>)>,
    /// A row's key values and sort key (`row`'s, kept for the next row: no allocation each).
    vals: Vec<Value>,
    sort: Vec<u8>,
}

/// What every participant needs, shared.
struct Plan<'a> {
    t: &'a Table,
    ix: &'a Index,
    cols: Vec<usize>,
    colls: &'a [Option<KeyColl>],
    types: Vec<DataType>,
    ix_types: Vec<DataType>,
    virt: Vec<usize>,
    virt_mask: Vec<bool>,
    need: Vec<bool>,
    fmt: crate::types::text::FormatCtx,
    check_nulls: bool,
    /// The table is modelled: each row's key in the index's model is taken (`sizing::
    /// btree_from_build` lays the model out from them) in place of the measure.
    model: bool,
}

impl Plan<'_> {
    /// A row: its entry, and its measure as `measure_rows_with` takes it.
    fn row(&self, g: &mut Gathered, part: &mut Part, dkey: &[u8], bytes: &[u8]) -> Result<()> {
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, &self.fmt);
        part.rows += 1;
        if self.model {
            let row = crate::exec::scan::decode_table_row_need(self.t, bytes, &self.need)?;
            let mut vals: Vec<Value> = self.cols.iter().map(|&i| row[i].clone()).collect();
            let k = crate::exec::scan::index_entry_key(self.t, self.ix, &vals, dkey, self.colls);
            let checked = !self.check_nulls || !vals.iter().any(Value::is_null);
            g.entries.push(&k, dkey.len().saturating_sub(8), checked);
            // (The measure's tuple size checks, as below.)
            vals.extend(self.ix.include.iter().map(|&i| row[i].clone()));
            let size = relsize::index_tuple(&self.ix_types, &vals, &text);
            if size > crate::exec::rowsize::INDEX_SIZE_MASK {
                return Err(crate::exec::rowsize::form_error(size));
            }
            if size > crate::exec::rowsize::BT_MAX_ITEM && g.oversized.as_ref().is_none_or(|o| k < o.0) {
                g.oversized = Some((k.clone(), size, dkey.to_vec()));
            }
            vals.truncate(self.cols.len());
            let ty = |n: usize| self.ix_types.get(n).cloned();
            let key = crate::exec::sizing::index_key_of(self.t, self.ix, &ty, self.colls, vals, &row, &self.fmt);
            g.model.push((key, dkey.to_vec()));
            return Ok(());
        }
        let stored = crate::storage::row::stored_tuple_len(crate::exec::scan::row_data(bytes)?, &self.types, &self.virt_mask);
        let mut row = match stored {
            Some(_) => crate::exec::scan::decode_table_row_need(self.t, bytes, &self.need)?,
            None => crate::exec::scan::decode_table_row(self.t, bytes)?,
        };
        let mut vals = std::mem::take(&mut g.vals);
        vals.clear();
        vals.extend(self.cols.iter().map(|&i| row[i].clone()));
        let at = g.entries.data.len();
        crate::exec::scan::index_entry_key_into(&mut g.entries.data, self.t, self.ix, &vals, dkey, self.colls);
        let checked = !self.check_nulls || !vals.iter().any(Value::is_null);
        g.entries.push_at(at, dkey.len().saturating_sub(8), checked);
        let k = &g.entries.data[at..];
        super::table::key_bytes_into(&mut g.sort, &vals);
        vals.extend(self.ix.include.iter().map(|&i| row[i].clone()));
        let size = relsize::index_tuple(&self.ix_types, &vals, &text);
        if size > crate::exec::rowsize::INDEX_SIZE_MASK {
            return Err(crate::exec::rowsize::form_error(size));
        }
        if size > crate::exec::rowsize::BT_MAX_ITEM && g.oversized.as_ref().is_none_or(|o| k < o.0.as_slice()) {
            g.oversized = Some((k.to_vec(), size, dkey.to_vec()));
        }
        g.keys.push(super::vacuum::measure_key(&g.sort, size));
        g.vals = vals;
        match stored {
            Some(len) => part.sizes.push(relsize::heap_tuple_of_len(len) as u32),
            None => {
                for &i in &self.virt {
                    if i < row.len() {
                        row[i] = Value::Null;
                    }
                }
                part.sizes.push(relsize::heap_tuple(&self.types, &row, &text) as u32);
            }
        }
        Ok(())
    }

    /// Take chunks until none are left, then sort the entries: `check` every so many rows
    /// (the leader's cancel checks), stopping early once `stop` is set (another failed).
    fn run(
        &self,
        chunks: &Mutex<VecDeque<(usize, crate::exec::parscan::Chunk)>>,
        stop: &AtomicBool,
        check: &dyn Fn() -> Result<()>,
        progress: (&IndexProgress, &AtomicU64),
    ) -> Result<Gathered> {
        let mut g = Gathered::default();
        loop {
            let next = chunks.lock().unwrap().pop_front();
            let Some((n, chunk)) = next else { break };
            let mut part = Part::default();
            for (i, r) in chunk.enumerate() {
                if i % 1024 == 0 {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(g);
                    }
                    check()?;
                    if i > 0 {
                        progress.0.scanned_now(progress.1.fetch_add(1024, Ordering::Relaxed) + 1024);
                    }
                }
                let (dkey, bytes) = r?;
                self.row(&mut g, &mut part, &dkey, &bytes)?;
            }
            g.parts.push((n, part));
        }
        g.entries.sort();
        // (Each participant's keys sorted here, merged by the leader: `btree_pages_sorted`.)
        g.keys.sort_unstable();
        Ok(g)
    }
}

/// Sorted lists merged into one sorted sequence, each item moved out once.
fn merge_sorted<T: Ord>(lists: Vec<Vec<T>>) -> impl Iterator<Item = T> {
    let mut its: Vec<std::vec::IntoIter<T>> = lists.into_iter().map(Vec::into_iter).collect();
    let mut heads: Vec<Option<T>> = its.iter_mut().map(Iterator::next).collect();
    std::iter::from_fn(move || {
        let mut min: Option<usize> = None;
        for (i, h) in heads.iter().enumerate() {
            if let Some(x) = h
                && min.is_none_or(|m| heads[m].as_ref().is_some_and(|y| x < y))
            {
                min = Some(i);
            }
        }
        let m = min?;
        std::mem::replace(&mut heads[m], its[m].next())
    })
}

/// CREATE INDEX's workers as PostgreSQL's plan_create_index_workers counts them: by the
/// heap's pages against min_parallel_table_scan_size (one more for each tripling, or the
/// table's parallel_workers), at most max_parallel_maintenance_workers, fewer while
/// maintenance_work_mem would give a participant (the leader one) under 32 MB.
pub(super) fn index_workers(d: &Ddl, t: &Table, pages: u32) -> usize {
    let s = d.ctx.sess.settings.borrow();
    let num = |n: &str, dflt: f64| s.get_str(n).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(dflt);
    let max = num("max_parallel_maintenance_workers", 2.0).max(0.0) as usize;
    if max == 0 || t.temporary {
        return 0;
    }
    let reloption = t.options.iter().find_map(|o| o.strip_prefix("parallel_workers="));
    let mut workers = match reloption {
        Some(n) => n.trim_matches('\'').parse().unwrap_or(0),
        None => {
            let min = (num("min_parallel_table_scan_size", 1024.0) as u64).max(1);
            if (pages as u64) < min {
                return 0;
            }
            let (mut w, mut threshold) = (1usize, min);
            while pages as u64 >= threshold.saturating_mul(3) {
                w += 1;
                threshold = threshold.saturating_mul(3);
            }
            w
        }
    }
    .min(max);
    let mwm = num("maintenance_work_mem", 65536.0) as usize;
    while workers > 0 && mwm / (workers + 1) < 32768 {
        workers -= 1;
    }
    workers
}

/// What a parallel build gives: its measure (a table too big to model), or a modelled
/// table's rows and entries counted with each row's key in the index's model and its data key
/// (`sizing::btree_from_build`).
pub(super) enum ParBuilt {
    Measured(RowMeasure),
    Modelled { rows: usize, tuples: usize, keys: Vec<(Key, Vec<u8>)> },
}

/// Where a parallel measure's rows go in the heap, as the serial measure places them
/// (`measure_rows_with`): by the heap's kept measure, `per` rows a page; or the first rows as
/// sized and packed (`sample`) and the rest from page `first` on at their density, up to the
/// estimate's last page.
enum Places {
    Kept { per: u64 },
    Sampled { sample: Vec<(u32, u16)>, first: u32, per: u64, last: u32 },
}

impl Places {
    /// The TID of the table's row `i` (from 0, in the table's order).
    fn tid(&self, i: u64) -> (u32, u16) {
        match self {
            Places::Kept { per } => ((i / per) as u32, (i % per + 1) as u16),
            Places::Sampled { sample, first, per, last } => match sample.get(i as usize) {
                Some(&t) => t,
                None => {
                    let j = i - sample.len() as u64;
                    ((first + (j / per) as u32).min(*last), (j % per + 1) as u16)
                }
            },
        }
    }
}

/// Gives the reserved workers back to the pool when the build ends.
struct Release(usize);

impl Drop for Release {
    fn drop(&mut self) {
        crate::exec::parscan::release_workers(self.0);
    }
}

impl Ddl<'_, '_> {
    /// The parallel build of `ix` over `t` (see the module), with up to `workers` workers:
    /// its measure, or (`model`: a modelled table) the rows' keys in the index's model.
    /// None: not for this index (the serial build runs).
    pub(super) fn build_index_parallel(
        &self,
        t: &Table,
        ix: &Index,
        bix: &BoundIndex,
        workers: usize,
        progress: &IndexProgress,
        model: bool,
    ) -> Result<Option<ParBuilt>> {
        let cols: Option<Vec<usize>> = bix.keys.iter().map(|k| if let Expr::Col(i, _) = k { Some(*i) } else { None }).collect();
        let (Some(cols), None) = (cols, &bix.predicate) else {
            return Ok(None);
        };
        let start = crate::storage::keys::index_prefix(t.oid, 0);
        let range_start = crate::storage::keys::index_prefix(t.oid, ix.id);
        let range_end = crate::storage::keys::prefix_end(&range_start);
        if !self.ctx.txn.borrow().index_runs_direct(&range_start, &range_end)? {
            return Ok(None);
        }
        let pool = self.ctx.sess.settings.borrow().get_str("max_parallel_workers").and_then(|v| v.trim().parse().ok()).unwrap_or(8);
        let release = Release(crate::exec::parscan::reserve_workers(workers, pool));
        crate::exec::parscan::count_index_build();
        let got = release.0;
        let Some(chunks) = self.build_chunks(t, got)? else {
            return Ok(None);
        };
        let virt: Vec<usize> = t.columns.iter().enumerate().filter(|(_, c)| c.virtual_gen).map(|(i, _)| i).collect();
        let mut needed = std::collections::HashSet::new();
        crate::exec::sizing::index_columns(bix, &mut needed);
        let mut ix_types: Vec<DataType> = bix.keys.iter().map(|k| k.ty()).collect();
        ix_types.extend(ix.include.iter().map(|&i| t.columns[i].ty.clone()));
        let plan = Plan {
            t,
            ix,
            cols,
            colls: &bix.colls,
            types: t.columns.iter().map(|c| c.ty.clone()).collect(),
            ix_types,
            virt_mask: (0..t.columns.len()).map(|i| virt.contains(&i)).collect(),
            virt,
            need: (0..t.columns.len()).map(|i| needed.contains(&i)).collect(),
            fmt: self.ctx.fmt().clone(),
            check_nulls: ix.unique && !ix.nulls_not_distinct,
            model,
        };
        let chunks = Mutex::new(chunks);
        let stop = AtomicBool::new(false);
        let done = AtomicU64::new(0);
        let gathered: Vec<Result<Gathered>> = std::thread::scope(|s| {
            let (plan, chunks, stop, report) = (&plan, &chunks, &stop, (progress, &done));
            let handles: Vec<_> = (0..got)
                .filter_map(|i| {
                    std::thread::Builder::new()
                        .name(format!("roll-build-{i}"))
                        .spawn_scoped(s, move || {
                            let r = plan.run(chunks, stop, &|| Ok(()), report);
                            if r.is_err() {
                                stop.store(true, Ordering::Relaxed);
                            }
                            r
                        })
                        .ok()
                })
                .collect();
            let lead = plan.run(chunks, stop, &|| self.ctx.check(), report);
            if lead.is_err() {
                stop.store(true, Ordering::Relaxed);
            }
            let mut all = vec![lead];
            all.extend(handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(crate::error::internal("index build worker panicked")))));
            all
        });
        drop(release);
        let mut parts: Vec<(usize, Part)> = Vec::new();
        let mut keys = Vec::new();
        let mut sorted = Vec::new();
        let mut oversized: Option<(Vec<u8>, usize, Vec<u8>)> = None;
        let mut model_keys = Vec::new();
        for g in gathered {
            let g = g?;
            if let Some(o) = g.oversized
                && oversized.as_ref().is_none_or(|m| o.0 < m.0)
            {
                oversized = Some(o);
            }
            parts.extend(g.parts);
            keys.push(g.keys);
            sorted.push(g.entries);
            model_keys.extend(g.model);
        }
        if ix.unique {
            self.unique_check(t, bix, ix, &sorted)?;
        }
        if let Some((_, n, dkey)) = oversized {
            let tid = crate::exec::dml::row_tid(t, &dkey, self.ctx);
            return Err(crate::exec::rowsize::btree_error(t, bix, n, tid, self.ctx));
        }
        parts.sort_by_key(|p| p.0);
        let rows = parts.iter().map(|p| p.1.rows).sum();
        if model {
            let tuples = model_keys.len();
            // (The serial build's count of the rows it read, as heap_getnext's.)
            crate::exec::pgstat::count_returned(self.ctx, t.oid, tuples as i64);
            progress.scanned_all(tuples as u64);
            self.put_runs(&start, sorted)?;
            progress.loaded(tuples as u64);
            return Ok(Some(ParBuilt::Modelled { rows, tuples, keys: model_keys }));
        }
        let live = relsize::heap_pages(parts.into_iter().flat_map(|p| p.1.sizes).map(|s| s as usize));
        let tuples = keys.iter().map(Vec::len).sum::<usize>();
        progress.scanned_all(tuples as u64);
        let pages = super::vacuum::btree_pages_sorted(bix, &plan.ix_types, merge_sorted(keys), tuples);
        self.put_runs(&start, sorted)?;
        progress.loaded(tuples as u64);
        Ok(Some(ParBuilt::Measured(RowMeasure::new(rows, live, vec![(ix.oid, pages, tuples as f64)]))))
    }

    /// `t`'s rows cut by key into chunks for `got` workers and the leader, in the table's
    /// order, read at the transaction's view now (None: no rows).
    fn build_chunks(&self, t: &Table, got: usize) -> Result<Option<VecDeque<(usize, crate::exec::parscan::Chunk)>>> {
        let start = crate::storage::keys::index_prefix(t.oid, 0);
        let end = crate::storage::keys::prefix_end(&start);
        let txn = self.ctx.txn.borrow();
        let snap = txn.snap();
        let lo = txn.scan_snap(&start, &end, &snap).raw().next().transpose()?.map(|(k, _)| k);
        let hi = txn.scan_rev(&start, &end, Some(&snap)).next().transpose()?.map(|(k, _)| k);
        let (Some(lo), Some(hi)) = (lo, hi) else {
            return Ok(None);
        };
        let fixed = (t.has_rowid() && lo.len() == hi.len()).then_some(lo.len());
        let cuts = crate::exec::parscan::split_keys(&lo, &hi, (got + 1) * 8);
        let cuts = crate::exec::scan::keyed_cuts(&txn, cuts, &start, &end, &snap, fixed)?;
        let mut bounds = vec![start.clone()];
        bounds.extend(cuts);
        bounds.push(end.clone());
        let bulk = self.ctx.sess.db.store.bulk_range(&start, &end);
        Ok(Some(
            bounds
                .windows(2)
                .enumerate()
                .map(|(i, w)| (i, crate::exec::parscan::chunk(txn.scan_snap(&w[0], &w[1], &snap), bulk)))
                .collect(),
        ))
    }

    /// The parallel build of a BRIN or GIN index `ix` over `t`, a table too big to model, with
    /// up to `workers` workers (PostgreSQL 18's `_brin_parallel_*` / `_gin_parallel_*`): the
    /// leader and the workers read chunks of the rows, each measuring its part
    /// (`am_measure::AmPlan`), merged once every chunk's rows are counted (a row's TID is the
    /// serial measure's for its place in the table: `measure_rows_with`). None: not for this
    /// index or table (the serial build runs).
    pub(super) fn measure_am_parallel(
        &self,
        t: &Table,
        ix: &Index,
        bix: &BoundIndex,
        workers: usize,
        progress: &IndexProgress,
    ) -> Result<Option<ParBuilt>> {
        use super::am_measure::{AmPart, AmPlan, merged_pages};
        let types: Vec<DataType> = bix.keys.iter().map(|k| k.ty()).collect();
        let mwm_kb = {
            let s = self.ctx.sess.settings.borrow();
            s.get_str("maintenance_work_mem").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(65536)
        };
        let db = &self.ctx.sess.db;
        let Ok(Some(blocks)) = crate::exec::sizing::heap_blocks(t, self.ctx) else {
            return Ok(None);
        };
        if !crate::exec::sizing::is_estimated(t.oid, db) {
            return Ok(None);
        }
        let Some(plan) = AmPlan::of(bix, &types, mwm_kb, workers + 1) else {
            return Ok(None);
        };
        // (Where each row goes in the heap, as the serial measure places it: by the measure
        // kept while nothing changed, else the first rows sized and packed and the rest spread
        // at their density over the heap's estimated pages.)
        let all_types: Vec<DataType> = t.columns.iter().map(|c| c.ty.clone()).collect();
        let virt: Vec<usize> = t.columns.iter().enumerate().filter(|(_, c)| c.virtual_gen).map(|(i, _)| i).collect();
        let virt_mask: Vec<bool> = (0..t.columns.len()).map(|i| virt.contains(&i)).collect();
        let layout = crate::txn::unchanged::layout_of(&all_types, &virt_mask);
        let kept = self.kept_live(t, layout, self.ctx.txn.borrow().snapshot_ts());
        let start = crate::storage::keys::index_prefix(t.oid, 0);
        let end = crate::storage::keys::prefix_end(&start);
        let places = match kept {
            Some((live, rows)) => Places::Kept { per: rows.div_ceil(live.max(1) as usize).max(1) as u64 },
            None => {
                let mut packer = relsize::HeapPacker::default();
                let (mut sample, mut bytes) = (Vec::with_capacity(super::vacuum::SIZED_SAMPLE), 0usize);
                let scan = self.ctx.txn.borrow().scan(&start, &end);
                for r in scan.take(super::vacuum::SIZED_SAMPLE) {
                    let (_, row) = r?;
                    let size = self.stored_tuple_size(t, &all_types, &virt, &virt_mask, &row)?;
                    packer.push(size);
                    bytes += size;
                    sample.push(packer.last());
                }
                if sample.len() < super::vacuum::SIZED_SAMPLE {
                    return Ok(None);
                }
                let mean = (bytes / super::vacuum::SIZED_SAMPLE).max(1);
                Places::Sampled {
                    sample,
                    first: packer.pages() as u32,
                    per: (relsize::HEAP_PAGE_ROOM / mean).clamp(1, relsize::MAX_HEAP_TUPLES) as u64,
                    last: blocks.max(1) - 1,
                }
            }
        };
        let pool = self.ctx.sess.settings.borrow().get_str("max_parallel_workers").and_then(|v| v.trim().parse().ok()).unwrap_or(8);
        let release = Release(crate::exec::parscan::reserve_workers(workers, pool));
        let got = release.0;
        let Some(chunks) = self.build_chunks(t, got)? else {
            return Ok(None);
        };
        crate::exec::parscan::count_index_build();
        let mut needed = std::collections::HashSet::new();
        crate::exec::sizing::index_columns(bix, &mut needed);
        let need: Vec<bool> = (0..t.columns.len()).map(|i| needed.contains(&i)).collect();
        let cols = plan.columns();
        // (GIN array_ops' index row size check of an array of a variable-size type, on each
        // row's keys: `rowsize::BuildSizes`.)
        let form = ix.method == "gin" && matches!(&types[0], DataType::Array(e) if relsize::type_layout(e).0 <= 0);
        let fmt = self.ctx.fmt().clone();
        let chunks = std::sync::Mutex::new(chunks);
        let stop = AtomicBool::new(false);
        let done = AtomicU64::new(0);
        // A participant's part: its measure, (chunk, rows) of the chunks it read, and the
        // first oversized GIN key it met.
        type Took = (AmPart, Vec<(usize, u64)>, Option<crate::exec::rowsize::GinFormError>);
        let run = |check: &dyn Fn() -> Result<()>| -> Result<Took> {
            let (mut part, mut counts, mut oversized) = (AmPart::default(), Vec::new(), None);
            loop {
                let next = chunks.lock().unwrap().pop_front();
                let Some((c, chunk)) = next else { break };
                let mut n = 0u32;
                for r in chunk {
                    if n.is_multiple_of(1024) {
                        if stop.load(Ordering::Relaxed) {
                            return Ok((part, counts, oversized));
                        }
                        check()?;
                        if n > 0 {
                            progress.scanned_now(done.fetch_add(1024, Ordering::Relaxed) + 1024);
                        }
                    }
                    let (_, bytes) = r?;
                    let row = crate::exec::scan::decode_table_row_need(t, &bytes, &need)?;
                    if form
                        && let Some(e) = crate::exec::rowsize::gin_form_error_with(ix, true, std::slice::from_ref(&row[cols[0]]), &fmt)
                        && oversized.as_ref().is_none_or(|o: &crate::exec::rowsize::GinFormError| e.precedes(o))
                    {
                        oversized = Some(e);
                    }
                    part.row(&plan, c, n, &row);
                    n += 1;
                }
                counts.push((c, n as u64));
            }
            Ok((part, counts, oversized))
        };
        let took: Vec<Result<Took>> = std::thread::scope(|s| {
            let (run, stop) = (&run, &stop);
            let handles: Vec<_> = (0..got)
                .filter_map(|i| {
                    std::thread::Builder::new()
                        .name(format!("roll-build-{i}"))
                        .spawn_scoped(s, move || {
                            let r = run(&|| Ok(()));
                            if r.is_err() {
                                stop.store(true, Ordering::Relaxed);
                            }
                            r
                        })
                        .ok()
                })
                .collect();
            let lead = run(&|| self.ctx.check());
            if lead.is_err() {
                stop.store(true, Ordering::Relaxed);
            }
            let mut all = vec![lead];
            all.extend(handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(crate::error::internal("index build worker panicked")))));
            all
        });
        drop(release);
        let (mut parts, mut counts, mut oversized) = (Vec::new(), Vec::new(), None);
        for r in took {
            let (p, c, o) = r?;
            if let Some(o) = o
                && oversized.as_ref().is_none_or(|m| o.precedes(m))
            {
                oversized = Some(o);
            }
            parts.push(p);
            counts.extend(c);
        }
        if let Some(e) = oversized {
            return Err(e.error);
        }
        counts.sort_unstable();
        // (Each chunk's first row's place in the table.)
        let mut starts = vec![0u64; counts.last().map_or(0, |c| c.0 + 1)];
        let mut rows = 0u64;
        for (c, n) in counts {
            starts[c] = rows;
            rows += n;
        }
        crate::exec::pgstat::count_returned(self.ctx, t.oid, rows as i64);
        progress.scanned_all(rows);
        let live = match kept {
            Some((live, _)) => live,
            None => blocks as i32,
        };
        let pages = merged_pages(plan, bix, &types, parts, &starts, &|i| places.tid(i), live as u32);
        progress.loaded(rows);
        let measure = super::vacuum::RowMeasure::with_estimate(rows as usize, live, kept.is_none(), vec![(ix.oid, pages, rows as f64)]);
        Ok(Some(ParBuilt::Measured(measure)))
    }

    /// The participants' sorted entries in as linked runs (`Txn::put_index_runs`).
    fn put_runs(&self, start: &[u8], sorted: Vec<Entries>) -> Result<()> {
        let runs: Vec<(Vec<u8>, Vec<(u32, u32, u32)>)> = sorted
            .into_iter()
            .map(|e| (e.data, e.ents.into_iter().map(|(_, o, l)| (o, l & 0xffff, (l & !CHECKED) >> 16)).collect()))
            .collect();
        self.ctx.txn.borrow_mut().put_index_runs(start, runs)
    }

    /// A unique index's duplicates, merging the participants' sorted entries (entries of
    /// one key sort by their data keys: the table's order).
    fn unique_check(&self, t: &Table, bix: &BoundIndex, ix: &Index, sorted: &[Entries]) -> Result<()> {
        // (An entry's key values: its key without its row's tail, `index_entry_key`.)
        fn values(e: &Entries, x: Ent) -> &[u8] {
            let k = e.key(x);
            &k[..k.len() - e.tail(x).len()]
        }
        let mut at = vec![0usize; sorted.len()];
        // (The current group of equal checked keys: its first entry and how many so far; and
        // the duplicate to report: the serial build's, the key whose second row comes first
        // in the table's order, with that key's first row: their data keys' tails.)
        let mut group: Option<((usize, Ent), usize)> = None;
        let mut found: Option<(&[u8], &[u8])> = None;
        loop {
            let mut min: Option<usize> = None;
            for (i, e) in sorted.iter().enumerate() {
                if at[i] < e.ents.len() && min.is_none_or(|m| e.key(e.ents[at[i]]) < sorted[m].key(sorted[m].ents[at[m]])) {
                    min = Some(i);
                }
            }
            let Some(m) = min else { break };
            let ent = sorted[m].ents[at[m]];
            at[m] += 1;
            if ent.2 & CHECKED == 0 {
                continue;
            }
            match &mut group {
                Some(((l, lent), n)) if values(&sorted[*l], *lent) == values(&sorted[m], ent) => {
                    *n += 1;
                    let second = sorted[m].tail(ent);
                    if *n == 2 && found.is_none_or(|f| second < f.0) {
                        found = Some((second, sorted[*l].tail(*lent)));
                    }
                }
                _ => group = Some(((m, ent), 1)),
            }
        }
        let Some((_, first)) = found else { return Ok(()) };
        let mut dkey = crate::storage::keys::index_prefix(t.oid, 0);
        dkey.extend_from_slice(first);
        let bytes = self.ctx.txn.borrow().get(&dkey)?;
        let first = match bytes {
            Some(b) => super::table::index_values(bix, &crate::exec::scan::decode_table_row(t, &b)?, self)?,
            None => None,
        };
        Err(self.duplicate_key(t, bix, first.as_deref().unwrap_or(&[]), &ix.name))
    }
}
