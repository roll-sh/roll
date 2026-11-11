//! ivfflat's scan (`ivfscan.c`): the `probes` lists nearest the query (kept in a pairing
//! heap, as pgvector keeps them), every entry of them sorted by distance through a tuplesort,
//! and with an iterative scan, the next lists' entries in batches once those run out.

use super::build::setting;
use super::page::{self, INVALID, Tuple};
use super::pairingheap::PairingHeap;
use super::tuplesort::Sorter;
use super::types::{Dist, Procs, check_expected_dim};
use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::{AmIndex, AmScan, AmStore};
use crate::ext::vector::dist::{self, Dist4};
use crate::types::Value;
use std::cmp::Ordering;
use std::sync::Arc;

/// A candidate: its distance and its tuple (an index into the scan's `pages`, its offset
/// there).
struct Item {
    distance: f64,
    page: u32,
    at: u32,
}

/// `IvfflatScanOpaqueData`.
pub(super) struct Scan {
    store: AmStore,
    procs: Procs,
    probes: usize,
    max_probes: usize,
    dimensions: i32,
    first: bool,
    /// The query (ORDER BY argument; None: NULL).
    orderby: Option<Value>,
    /// The query as the entries are compared with it (normalized when the class does).
    value: Option<Value>,
    sorter: Sorter,
    list_pages: Vec<u32>,
    list_index: usize,
    /// The tuples of the entry pages the sorted items come from, as the scan read them.
    pages: Vec<Arc<Vec<Tuple>>>,
    out: std::vec::IntoIter<Item>,
}

/// `ivfflatbeginscan` + `ivfflatrescan`.
pub(super) fn begin(ix: &AmIndex, orderbys: &[Value], ctx: &ExecCtx) -> Result<Scan> {
    let procs = Procs::of(ix)?;
    let mut probes = setting(ctx, "ivfflat.probes", 1).max(1) as usize;
    // Get lists and dimensions from metapage
    let (lists, dimensions) = page::with(&ix.store, |idx| {
        let (lists, dims, _) = idx.as_ref().ok_or_else(|| crate::error::internal("ivfflat index is not valid"))?.meta()?;
        Ok((lists.max(0) as usize, dims))
    })?;
    let iterative = ctx.sess.settings.borrow().get_str("ivfflat.iterative_scan").is_some_and(|v| v != "off");
    let mut max_probes = if iterative { (setting(ctx, "ivfflat.max_probes", 32768).max(1) as usize).max(probes) } else { probes };
    probes = probes.min(lists);
    max_probes = max_probes.min(lists);
    Ok(Scan {
        store: ix.store.clone(),
        procs,
        probes,
        max_probes,
        dimensions,
        first: true,
        orderby: orderbys.first().cloned(),
        value: None,
        sorter: Sorter::new(setting(ctx, "work_mem", 4096)),
        list_pages: Vec::new(),
        list_index: 0,
        pages: Vec::new(),
        out: Vec::new().into_iter(),
    })
}

/// `CompareLists`: by distance (NaN equal to everything).
fn cmp(a: usize, b: usize, l: &[(u32, f64)]) -> Ordering {
    if l[a].1 > l[b].1 {
        Ordering::Greater
    } else if l[a].1 < l[b].1 {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// `float8_cmp_internal`: NaN above everything, equal to itself.
fn float8_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

impl Scan {
    /// The distance function: FUNCTION 1, or `ZeroDistance` for a NULL query.
    fn distfunc(&self) -> Dist {
        fn zero(_: &Value, _: &Value) -> f64 {
            0.0
        }
        if self.value.is_some() { self.procs.distance } else { zero }
    }

    /// `GetScanValue`: the query, normalized when the class normalizes, its dimensions
    /// checked.
    fn scan_value(&mut self) -> Result<()> {
        self.value = match self.orderby.take() {
            None | Some(Value::Null) => None,
            Some(v) => {
                let v = if self.procs.norm.is_some() { self.procs.kind.normalize(&v) } else { v };
                check_expected_dim(self.dimensions, self.procs.kind.dimensions(&v))?;
                Some(v)
            }
        };
        Ok(())
    }

    /// `GetScanLists`: the start pages of the `max_probes` nearest lists, nearest first.
    fn scan_lists(&mut self) -> Result<()> {
        let dist = self.distfunc();
        let q = self.value.clone().unwrap_or(Value::Null);
        let max_probes = self.max_probes;
        let pages = page::with(&self.store, |idx| {
            let idx = idx.as_ref().ok_or_else(|| crate::error::internal("ivfflat index is not valid"))?;
            // (start page, distance) of the heap's nodes
            let mut lists: Vec<(u32, f64)> = Vec::with_capacity(max_probes);
            let mut heap = PairingHeap::new(max_probes);
            let mut max_distance = f64::MAX;
            for (_, list) in idx.lists() {
                // Use procinfo from the index instead of scan key for performance
                let distance = dist(&list.center, &q);
                if lists.len() < max_probes {
                    lists.push((list.start, distance));
                    let n = lists.len() - 1;
                    heap.add(n, &mut |a, b| cmp(a, b, &lists));
                    // Calculate max distance
                    if lists.len() == max_probes {
                        max_distance = lists[heap.first()].1;
                    }
                } else if distance < max_distance {
                    // Remove, reuse
                    let n = heap.remove_first(&mut |a, b| cmp(a, b, &lists));
                    lists[n] = (list.start, distance);
                    heap.add(n, &mut |a, b| cmp(a, b, &lists));
                    // Update max distance
                    max_distance = lists[heap.first()].1;
                }
            }
            let mut pages = vec![INVALID; lists.len()];
            for i in (0..lists.len()).rev() {
                pages[i] = lists[heap.remove_first(&mut |a, b| cmp(a, b, &lists))].0;
            }
            debug_assert!(heap.is_empty());
            Ok(pages)
        })?;
        self.list_pages = pages;
        Ok(())
    }

    /// `GetScanItems`: the entries of the next `probes` lists, sorted by distance.
    fn scan_items(&mut self, ctx: &ExecCtx) -> Result<()> {
        ctx.check()?;
        let dist = self.distfunc();
        let dist4 = self.value.as_ref().and(self.procs.distance4);
        let q = self.value.clone().unwrap_or(Value::Null);
        self.sorter.reset();
        let mut batch = Vec::new();
        let mut probes = 0;
        // Search closest probes lists
        while self.list_index < self.max_probes {
            probes += 1;
            if probes > self.probes {
                break;
            }
            batch.push(self.list_pages.get(self.list_index).copied().unwrap_or(INVALID));
            self.list_index += 1;
        }
        // The lists' pages' tuples as they are now (shared, not copied).
        let pages = page::with(&self.store, |idx| {
            let idx = idx.as_ref().ok_or_else(|| crate::error::internal("ivfflat index is not valid"))?;
            let mut pages = Vec::new();
            for start in batch {
                // Search all entry pages for list
                let mut search = start;
                while search != INVALID {
                    let p = &idx.pages[search as usize];
                    pages.extend(p.shared_tuples());
                    search = p.next();
                }
            }
            Ok(pages)
        })?;
        let mut items = Vec::with_capacity(pages.iter().map(|p| p.len()).sum());
        for (page, tuples) in pages.iter().enumerate() {
            let mut at = 0;
            distances(tuples, &q, dist, dist4, |distance| {
                items.push(Item { distance, page: page as u32, at });
                at += 1;
            });
        }
        // (A tuple of distance float8 and heaptid takes 32 bytes.)
        let sorted = self.sorter.sort(items, |_| 32, |a, b| float8_cmp(a.distance, b.distance));
        self.pages = pages;
        self.out = sorted.into_iter();
        Ok(())
    }
}

/// Each tuple's distance to the query `q`, in their order: four at a time where the kernel
/// has that form (the next four's values fetched meanwhile), else one by one.
fn distances(tuples: &[Tuple], q: &Value, dist: Dist, dist4: Option<Dist4>, mut out: impl FnMut(f64)) {
    let mut i = 0;
    if let (Some(d4), Value::Ext(_, qb)) = (dist4, q) {
        fn ext(t: &Tuple) -> Option<&[u8]> {
            match &t.val {
                Value::Ext(_, b) => Some(b),
                _ => None,
            }
        }
        while let Some(g) = tuples.get(i..i + 4) {
            let (Some(a), Some(b), Some(c), Some(d)) = (ext(&g[0]), ext(&g[1]), ext(&g[2]), ext(&g[3])) else {
                break;
            };
            for t in tuples.iter().skip(i + 4).take(4) {
                if let Some(b) = ext(t) {
                    dist::prefetch(b);
                }
            }
            for d in d4(&[a, b, c, d], qb) {
                out(d);
            }
            i += 4;
        }
    }
    for t in &tuples[i..] {
        out(dist(&t.val, q));
    }
}

impl AmScan for Scan {
    /// `ivfflatgettuple`.
    fn next(&mut self, ctx: &ExecCtx, wants: &dyn Fn(&[u8]) -> bool) -> Result<Option<(Vec<u8>, Option<Value>)>> {
        if self.first {
            self.scan_value()?;
            self.scan_lists()?;
            self.scan_items(ctx)?;
            self.first = false;
        }
        loop {
            if let Some(it) = self.out.next() {
                let t = &self.pages[it.page as usize][it.at as usize];
                return Ok(Some((t.dkey.to_vec(), wants(&t.dkey).then(|| t.row_value().clone()))));
            }
            if self.list_index == self.max_probes {
                return Ok(None);
            }
            self.scan_items(ctx)?;
        }
    }
}
