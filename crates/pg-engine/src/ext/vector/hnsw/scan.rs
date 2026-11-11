//! `hnswscan.c`: an ordered scan. The first fetch searches the graph (the upper layers with
//! ef 1, layer 0 with hnsw.ef_search) and returns those elements' heap tids, nearest
//! first; iterative scans (hnsw.iterative_scan) then resume layer 0 from the candidates set
//! aside, until hnsw.max_scan_tuples elements were visited or the scan's memory passes
//! work_mem * hnsw.scan_mem_multiplier (measured as pgvector's memory context counts it).

use super::Shared;
use super::graph::NONE;
use super::mem::ListMem;
use super::pheap::DHeap;
use super::search::{CANDIDATE_SIZE, Disk, ELEMENT_SIZE, Opts};
use super::types::Support;
use crate::Result;
use crate::exec::ExecCtx;
use crate::ext::index_am::AmScan;
use crate::types::Value;
use std::sync::Arc;

/// HnswIterativeScanMode.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Iterative {
    Off,
    Relaxed,
    Strict,
}

/// The scan's settings, read when it starts.
pub(super) struct Settings {
    pub ef_search: usize,
    pub iterative: Iterative,
    pub max_scan_tuples: i64,
    /// work_mem * hnsw.scan_mem_multiplier in bytes, plus 256 (so->maxMemory).
    pub max_memory: f64,
}

impl Settings {
    pub fn of(ctx: &ExecCtx) -> Settings {
        let s = ctx.sess.settings.borrow();
        let num = |n: &str, d: f64| s.get_str(n).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(d);
        let iterative = match s.get_str("hnsw.iterative_scan").unwrap_or("off") {
            "relaxed_order" => Iterative::Relaxed,
            "strict_order" => Iterative::Strict,
            _ => Iterative::Off,
        };
        let max_memory = num("work_mem", 4096.0) * num("hnsw.scan_mem_multiplier", 1.0) * 1024.0 + 256.0;
        Settings {
            ef_search: num("hnsw.ef_search", 40.0) as usize,
            iterative,
            max_scan_tuples: num("hnsw.max_scan_tuples", 20000.0) as i64,
            max_memory: max_memory.min((usize::MAX / 2) as f64),
        }
    }
}

/// HnswScanOpaqueData.
pub(super) struct Scan {
    shared: Arc<Shared>,
    sup: Support,
    set: Settings,
    /// The query as the index compares it (None: a NULL query, every distance 0).
    q: Option<Vec<u8>>,
    query: Option<Value>,
    first: bool,
    st: Disk,
    /// so->w: the candidates to return, the nearest last, and the list's memory.
    w: Vec<u32>,
    wmem: ListMem,
    discarded: Option<DHeap>,
    previous: f64,
}

impl Scan {
    pub fn new(shared: Arc<Shared>, sup: Support, set: Settings, query: Option<Value>) -> Scan {
        Scan { shared, sup, set, q: None, query, first: true, st: Disk::scan(), w: Vec::new(), wmem: ListMem::default(), discarded: None, previous: f64::NEG_INFINITY }
    }

    /// GetScanItems: the first search.
    fn get_scan_items(&mut self) -> Result<()> {
        let shared = self.shared.clone();
        let g = shared.graph.lock();
        let g = g.as_ref().ok_or_else(|| crate::error::internal("hnsw index is not valid"))?;
        let entry = g.entry;
        if entry != NONE {
            // (HnswGetMetaPageInfo's entry element)
            self.st.alloc(ELEMENT_SIZE);
        }
        if let Some(v) = &self.query {
            self.sup.check_dim(g.dims as i32, v)?;
        }
        if entry == NONE {
            return Ok(());
        }
        let q = self.q.as_deref();
        let ep0 = self.st.entry_candidate(g, self.sup.dist, q, entry)?;
        let mut list = ListMem::default();
        if let Some(m) = &mut self.st.mem {
            list.append(m);
        }
        let mut ep = vec![ep0];
        let level = self.st.loaded[self.st.sc[ep0 as usize].e as usize].level as usize;
        for lc in (1..=level).rev() {
            ep = self.st.search_layer(g, self.sup.dists(), q, &ep, 1, lc, Opts::default(), None)?.0;
        }
        let o = Opts { keep_visited: true, init_visited: true, count: true, ..Default::default() };
        let mut discarded = (self.set.iterative != Iterative::Off).then(DHeap::new);
        let (w, wmem) = self.st.search_layer(g, self.sup.dists(), q, &ep, self.set.ef_search, 0, o, discarded.as_mut())?;
        self.discarded = discarded;
        self.w = w;
        self.wmem = wmem;
        Ok(())
    }

    /// ResumeScanItems: the next batch of discarded candidates as entry points of a search
    /// that goes on with the scan's visited set.
    fn resume_scan_items(&mut self) -> Result<()> {
        let Some(mut discarded) = self.discarded.take() else {
            return Ok(());
        };
        let batch = self.set.ef_search;
        let mut ep = Vec::new();
        let mut list = ListMem::default();
        while ep.len() < batch && !discarded.is_empty() {
            ep.push(discarded.remove_first(&mut self.st.sc));
            if let Some(m) = &mut self.st.mem {
                list.append(m);
            }
        }
        let r = if ep.is_empty() {
            Ok(())
        } else {
            let shared = self.shared.clone();
            let g = shared.graph.lock();
            let g = g.as_ref().ok_or_else(|| crate::error::internal("hnsw index is not valid"))?;
            let o = Opts { keep_visited: true, init_visited: false, count: true, ..Default::default() };
            self.st.search_layer(g, self.sup.dists(), self.q.as_deref(), &ep, batch, 0, o, Some(&mut discarded)).map(|(w, wmem)| {
                self.w = w;
                self.wmem = wmem;
            })
        };
        self.discarded = Some(discarded);
        r
    }

    fn mem_allocated(&self) -> usize {
        self.st.mem.as_ref().map_or(0, |m| m.mem)
    }

    /// GetScanValue: the query normalized when the class normalizes (in the scan's memory).
    fn scan_value(&mut self) {
        let Some(v) = &self.query else {
            return;
        };
        let orig = self.sup.bytes(v);
        let q = self.sup.query(v);
        let bytes = self.sup.bytes(&q);
        if self.sup.norm.is_some() {
            let m = self.st.mem.as_mut().unwrap();
            let first = m.alloc(self.sup.varsize(&orig));
            // (sparsevec_l2_normalize allocates again when elements became zero)
            if bytes.len() != orig.len() {
                m.alloc(self.sup.varsize(&bytes));
                m.free(first);
            }
        }
        self.q = Some(bytes);
    }
}

impl AmScan for Scan {
    /// hnswgettuple.
    fn next(&mut self, ctx: &ExecCtx, wants: &dyn Fn(&[u8]) -> bool) -> Result<Option<(Vec<u8>, Option<Value>)>> {
        if self.first {
            ctx.check()?;
            self.scan_value();
            self.get_scan_items()?;
            self.first = false;
        }
        loop {
            if self.w.is_empty() {
                if self.set.iterative == Iterative::Off {
                    return Ok(None);
                }
                let limit = self.st.tuples >= self.set.max_scan_tuples || self.mem_allocated() as f64 > self.set.max_memory;
                // Empty index
                let Some(d) = &mut self.discarded else {
                    return Ok(None);
                };
                // Reached max number of tuples or memory limit
                if limit {
                    if d.is_empty() {
                        return Ok(None);
                    }
                    // Return remaining tuples
                    let s = d.remove_first(&mut self.st.sc);
                    self.w.push(s);
                    self.wmem.append(self.st.mem.as_mut().unwrap());
                } else {
                    ctx.check()?;
                    self.resume_scan_items()?;
                }
                if self.w.is_empty() {
                    return Ok(None);
                }
            }
            let sc = self.st.sc[*self.w.last().unwrap() as usize];
            let l = &mut self.st.loaded[sc.e as usize];
            // Move to next element if no valid heap TIDs
            if l.nheap == 0 {
                self.w.pop();
                let m = self.st.mem.as_mut().unwrap();
                self.wmem.delete_last(m);
                // Mark memory as free for next iteration
                if self.set.iterative != Iterative::Off {
                    m.free_size(ELEMENT_SIZE);
                    m.free_size(CANDIDATE_SIZE);
                }
                continue;
            }
            l.nheap -= 1;
            let (slot, h) = (l.slot, self.st.pool[(l.heap + l.nheap as u32) as usize]);
            if self.set.iterative == Iterative::Strict {
                if sc.dist < self.previous {
                    continue;
                }
                self.previous = sc.dist;
            }
            let g = self.shared.graph.lock();
            let g = g.as_ref().ok_or_else(|| crate::error::internal("hnsw index is not valid"))?;
            let dkey = g.heap(h).to_vec();
            let v = wants(&dkey).then(|| self.sup.value(g.value(slot), g.dims));
            return Ok(Some((dkey, v)));
        }
    }
}

