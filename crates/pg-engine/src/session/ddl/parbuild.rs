//! Parallel btree builds (PostgreSQL's `_bt_parallel_build`): a big table's rows are cut into
//! key ranges that the leader and `max_parallel_maintenance_workers`' workers read, each
//! decoding its rows, measuring them (`Ddl::measure_rows_with`'s numbers) and sorting its
//! entries; the sorted entries go in as linked runs (`Txn::put_index_runs`), and a unique
//! index's duplicates are found merging them, the one reported the serial build's (the key
//! whose second row comes first in the table's order). Only for an index of plain columns
//! without a predicate, using either measured tuple sizes or model keys.

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
/// (its key's 8 bytes after the index's prefix, offset, key length | tail length << 16 |
/// `CHECKED`) in `ents`, sorted by key at the end. Its value, the row's data key, is the
/// table's prefix and the key's tail (`index_entry_key`).
#[derive(Default)]
struct Entries {
    data: Vec<u8>,
    ents: Vec<(u64, u32, u32)>,
}

/// An entry whose key values take part in the unique check (none null, or NULLS NOT
/// DISTINCT).
const CHECKED: u32 = 1 << 31;

type Ent = (u64, u32, u32);

impl Entries {
    fn push(&mut self, k: &[u8], tail: usize, checked: bool) {
        let mut w = [0u8; 8];
        let rest = k.get(8..).unwrap_or(&[]);
        let n = rest.len().min(8);
        w[..n].copy_from_slice(&rest[..n]);
        let lens = k.len() as u32 | (tail as u32) << 16 | if checked { CHECKED } else { 0 };
        self.ents.push((u64::from_be_bytes(w), self.data.len() as u32, lens));
        self.data.extend_from_slice(k);
    }

    fn key(&self, e: Ent) -> &[u8] {
        &self.data[e.1 as usize..(e.1 + (e.2 & 0xffff)) as usize]
    }

    fn tail(&self, e: Ent) -> &[u8] {
        let k = self.key(e);
        &k[k.len() - ((e.2 & !CHECKED) >> 16) as usize..]
    }

    /// By each key's first 8 bytes past the index's prefix, then the keys themselves where
    /// those tie.
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
    /// The heap's tuples' sizes, in order (`relsize::heap_pages` packs them).
    sizes: Vec<usize>,
}

/// What a participant gathered over its chunks.
#[derive(Default)]
struct Gathered {
    entries: Entries,
    /// The measure's (sort key, key bytes, index tuple size) of each entry.
    keys: Vec<(u128, Vec<u8>, usize)>,
    parts: Vec<(usize, Part)>,
    /// A modelled table's keys with their data keys.
    model: Vec<(Key, Vec<u8>)>,
    sizes: crate::exec::rowsize::BuildSizes,
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
    #[cfg(test)]
    observed: Option<std::sync::Arc<crate::exec::rowsize::observe::Observer>>,
}

impl Plan<'_> {
    /// A row: its entry, and its measure as `measure_rows_with` takes it.
    fn row(&self, g: &mut Gathered, part: &mut Part, dkey: &[u8], bytes: &[u8], _participant: usize) -> Result<()> {
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, &self.fmt);
        part.rows += 1;
        #[cfg(test)]
        if let Some(o) = &self.observed {
            o.row(_participant);
        }
        let stored = if self.model {
            None
        } else {
            crate::storage::row::stored_tuple_len(crate::exec::scan::row_data(bytes)?, &self.types, &self.virt_mask)
        };
        let mut row = if self.model || stored.is_some() {
            crate::exec::scan::decode_table_row_need(self.t, bytes, &self.need)?
        } else {
            crate::exec::scan::decode_table_row(self.t, bytes)?
        };
        let vals: Vec<Value> = self.cols.iter().map(|&i| row[i].clone()).collect();
        let k = crate::exec::scan::index_entry_key(self.t, self.ix, &vals, dkey, self.colls);
        let checked = !self.check_nulls || !vals.iter().any(Value::is_null);
        let all: Vec<Value> = vals.iter().cloned().chain(self.ix.include.iter().map(|&i| row[i].clone())).collect();
        #[cfg(test)]
        let expanded = all.iter().filter_map(|v| if let Value::Text(s) = v { Some(s.len()) } else { None }).max().unwrap_or(0);
        #[cfg(test)]
        if let Some(o) = &self.observed {
            o.expanded(_participant, expanded);
        }
        let size = crate::exec::rowsize::btree_size(&self.ix_types, &all, &self.fmt);
        g.sizes.btree_row(size, || k.clone(), dkey)?;
        #[cfg(test)]
        if let Some(o) = &self.observed {
            o.validated(_participant, g.sizes.candidate(), expanded);
        }
        g.entries.push(&k, dkey.len().saturating_sub(8), checked);
        if self.model {
            let ty = |n: usize| self.ix_types.get(n).cloned();
            let key = crate::exec::sizing::index_key_of(self.t, self.ix, &ty, self.colls, vals, &row, &self.fmt);
            g.model.push((key, dkey.to_vec()));
            #[cfg(test)]
            if let Some(o) = &self.observed {
                o.model_return(_participant);
            }
            return Ok(());
        }
        let mut sort = super::table::key_bytes(&vals);
        let prefix = super::vacuum::key_prefix(&sort);
        if sort.len() <= 16 {
            sort = Vec::new();
        }
        let exact = size.unwrap_or_else(|| relsize::index_tuple(&self.ix_types, &all, &text));
        g.keys.push((prefix, sort, exact));
        match stored {
            Some(len) => part.sizes.push(relsize::heap_tuple_of_len(len)),
            None => {
                for &i in &self.virt {
                    if i < row.len() {
                        row[i] = Value::Null;
                    }
                }
                part.sizes.push(relsize::heap_tuple(&self.types, &row, &text));
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
        _participant: usize,
    ) -> Result<Gathered> {
        let mut g = Gathered::default();
        #[cfg(test)]
        let mut first = true;
        loop {
            #[cfg(test)]
            let next = match &self.observed {
                Some(o) if first => o.first(_participant, chunks)?,
                _ => chunks.lock().unwrap().pop_front(),
            };
            #[cfg(not(test))]
            let next = chunks.lock().unwrap().pop_front();
            let Some((n, chunk)) = next else { break };
            #[cfg(test)]
            if let Some(o) = &self.observed {
                o.chunk(_participant, n);
            }
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
                self.row(&mut g, &mut part, &dkey, &bytes, _participant)?;
            }
            g.parts.push((n, part));
            #[cfg(test)]
            if first {
                first = false;
                if let Some(o) = &self.observed
                    && !o.completed_first(_participant)?
                {
                    return Ok(g);
                }
            }
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
        let end = crate::storage::keys::prefix_end(&start);
        let range_start = crate::storage::keys::index_prefix(t.oid, ix.id);
        let range_end = crate::storage::keys::prefix_end(&range_start);
        if !self.ctx.txn.borrow().index_runs_direct(&range_start, &range_end)? {
            return Ok(None);
        }
        let pool = self.ctx.sess.settings.borrow().get_str("max_parallel_workers").and_then(|v| v.trim().parse().ok()).unwrap_or(8);
        let release = Release(crate::exec::parscan::reserve_workers(workers, pool));
        crate::exec::parscan::count_index_build();
        let got = release.0;
        #[cfg(test)]
        let observed = crate::exec::rowsize::observe::current();
        #[cfg(test)]
        if let Some(o) = &observed {
            o.reserve(model, workers, got);
        }
        let chunks = {
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
            bounds
                .windows(2)
                .enumerate()
                .map(|(i, w)| (i, crate::exec::parscan::chunk(txn.scan_snap(&w[0], &w[1], &snap), bulk)))
                .collect::<VecDeque<_>>()
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
            #[cfg(test)]
            observed,
        };
        let chunks = Mutex::new(chunks);
        let stop = AtomicBool::new(false);
        let done = AtomicU64::new(0);
        let catalog = self.ctx.catalog();
        let gathered: Vec<Result<Gathered>> = std::thread::scope(|s| {
            let (plan, chunks, stop, report) = (&plan, &chunks, &stop, (progress, &done));
            let catalog = &catalog;
            let handles: Vec<_> = (0..got)
                .filter_map(|i| {
                    let handle = std::thread::Builder::new()
                        .name(format!("roll-build-{i}"))
                        .spawn_scoped(s, move || {
                            let _default = crate::types::collation::scoped_default(catalog);
                            let r = plan.run(chunks, stop, &|| Ok(()), report, i + 1);
                            #[cfg(test)]
                            if let (Some(o), Err(e)) = (&plan.observed, &r) {
                                o.error(i + 1, e);
                            }
                            if r.is_err() {
                                stop.store(true, Ordering::Relaxed);
                            }
                            r
                        })
                        .ok();
                    #[cfg(test)]
                    if let Some(o) = &plan.observed {
                        o.spawned(handle.is_some());
                    }
                    handle
                })
                .collect();
            let lead = plan.run(chunks, stop, &|| self.ctx.check(), report, 0);
            #[cfg(test)]
            if let (Some(o), Err(e)) = (&plan.observed, &lead) {
                o.error(0, e);
            }
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
        let mut model_keys = Vec::new();
        let mut sizes = crate::exec::rowsize::BuildSizes::default();
        for (_participant, g) in gathered.into_iter().enumerate() {
            let g = g?;
            #[cfg(test)]
            let candidate = g.sizes.candidate().clone();
            sizes.merge(g.sizes);
            #[cfg(test)]
            if let Some(o) = &plan.observed {
                o.folded(_participant, &candidate, sizes.candidate());
            }
            parts.extend(g.parts);
            keys.push(g.keys);
            sorted.push(g.entries);
            model_keys.extend(g.model);
        }
        if ix.unique {
            self.unique_check(t, bix, ix, &sorted)?;
        }
        sizes.finish(t, bix, self.ctx)?;
        parts.sort_by_key(|p| p.0);
        let rows = parts.iter().map(|p| p.1.rows).sum();
        if model {
            let tuples = model_keys.len();
            crate::exec::pgstat::count_returned(self.ctx, t.oid, tuples as i64);
            progress.scanned_all(tuples as u64);
            self.put_runs(&start, sorted)?;
            progress.loaded(tuples as u64);
            return Ok(Some(ParBuilt::Modelled { rows, tuples, keys: model_keys }));
        }
        let live = relsize::heap_pages(parts.into_iter().flat_map(|p| p.1.sizes));
        let tuples = keys.iter().map(Vec::len).sum::<usize>();
        progress.scanned_all(tuples as u64);
        let pages = super::vacuum::btree_pages_sorted(bix, &plan.ix_types, merge_sorted(keys), tuples);
        self.put_runs(&start, sorted)?;
        progress.loaded(tuples as u64);
        Ok(Some(ParBuilt::Measured(RowMeasure::new(rows, live, vec![(ix.oid, pages, tuples as f64)]))))
    }

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
