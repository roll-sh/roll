//! `hnswbuild.c`: the serial build. Elements are inserted in heap order into a graph held in
//! memory (a Generation context whose size is measured against maintenance_work_mem), then
//! written out as pages (FlushPages); when the graph outgrows maintenance_work_mem, it is
//! written out then and the rest of the rows are inserted on disk (`insert`).

use super::graph::{Graph, HEAPTIDS, NONE, layer_m, max_level};
use super::mem::Generation;
use super::pheap::{CHeap, Sc, WHeap};
use super::search::{Cand, select_neighbors, update_connection};
use super::types::Support;
use crate::Result;
use crate::exec::ExecCtx;
use crate::types::Value;

/// Whether the build graph's blocks come at decreasing addresses (each 1 MB block is its
/// own mmap, placed below the last one): the order of element addresses across blocks.
const BLOCKS_DESCEND: bool = true;

/// Element addresses are ordered by (block, offset / 8): 15 bits of block number (graphs up
/// to 32 GB of 1 MB blocks) and 17 of offset.
const ADDR_BLOCKS: u32 = 1 << 15;

/// An element of the in-memory graph (HnswElementData).
struct BElem {
    level: u8,
    nheap: u8,
    heaptids: [u32; HEAPTIDS],
    /// Its address, for CompareCandidateDistances.
    addr: u32,
    /// Its neighbor arrays: the first item (layer 0's 2m, then m per layer) and the first
    /// (length, closerSet) of its layers.
    items: u32,
    heads: u32,
}

/// The build state (HnswBuildState with its in-memory graph).
pub(super) struct Build {
    pub sup: Support,
    pub dims: i32,
    m: usize,
    ef_construction: usize,
    ml: f64,
    max_level: u8,
    elems: Vec<BElem>,
    /// AddElementInMemory's list, oldest first (its head is the last).
    list: Vec<u32>,
    entry: u32,
    items: Vec<Cand>,
    heads: Vec<(u16, bool)>,
    vstart: Vec<usize>,
    vlen: Vec<u32>,
    vdata: Vec<u8>,
    /// graphCtx and graph->memoryUsed / memoryTotal.
    graph_ctx: Generation,
    memory_used: usize,
    memory_total: usize,
    sc: Vec<Sc>,
    stamp: Vec<u32>,
    stamp_gen: u32,
    unvisited: Vec<u32>,
    scratch: Vec<Cand>,
    /// The index: its heap tids from the start, its pages once flushed.
    pub graph: Graph,
    pub flushed: bool,
    pub indtuples: f64,
}

impl Build {
    /// InitBuildState (its checks done by the caller).
    pub fn new(sup: Support, dims: i32, m: usize, ef_construction: usize, memory_total: usize) -> Build {
        Build {
            sup,
            dims,
            m,
            ef_construction,
            ml: 1.0 / (m as f64).ln(),
            max_level: max_level(m as u16),
            elems: Vec::new(),
            list: Vec::new(),
            entry: NONE,
            items: Vec::new(),
            heads: Vec::new(),
            vstart: Vec::new(),
            vlen: Vec::new(),
            vdata: Vec::new(),
            graph_ctx: Generation::new(1024 * 1024, 1024 * 1024, 1024 * 1024),
            memory_used: 0,
            memory_total: memory_total.min(usize::MAX / 2),
            sc: Vec::new(),
            stamp: Vec::new(),
            stamp_gen: 0,
            unvisited: Vec::new(),
            scratch: Vec::new(),
            graph: Graph::new(dims as u32, m as u16, ef_construction as u16),
            flushed: false,
            indtuples: 0.0,
        }
    }

    #[inline(always)]
    fn value(&self, e: u32) -> &[u8] {
        let s = self.vstart[e as usize];
        &self.vdata[s..s + self.vlen[e as usize] as usize]
    }

    #[inline(always)]
    fn dist(&self, a: u32, b: u32) -> f64 {
        (self.sup.dist)(self.value(a), self.value(b))
    }

    /// Where element `e`'s layer `lc` neighbors start in `items`.
    #[inline(always)]
    fn layer_start(&self, e: u32, lc: usize) -> usize {
        self.elems[e as usize].items as usize + if lc == 0 { 0 } else { 2 * self.m + (lc - 1) * self.m }
    }

    #[inline(always)]
    fn head(&self, e: u32, lc: usize) -> usize {
        self.elems[e as usize].heads as usize + lc
    }

    /// HnswMemoryContextAlloc: a chunk of the graph context (memoryUsed follows).
    fn alloc(&mut self, size: usize) -> (u32, usize) {
        let at = self.graph_ctx.alloc(size);
        self.memory_used = self.graph_ctx.mem;
        at
    }

    /// HnswInitElement + the copy of the value: the element's memory (element, neighbor
    /// list, neighbor arrays, value) from the graph context, its level drawn.
    fn init_element(&mut self, heaptid: u32, value: &[u8], varsize: usize, ctx: &ExecCtx) -> u32 {
        let (block, off) = self.alloc(super::search::ELEMENT_SIZE);
        let uniform = ctx.sess.global_random_double();
        let level = if uniform == 0.0 { self.max_level as i64 } else { (-uniform.ln() * self.ml) as i64 };
        let level = level.min(self.max_level as i64) as u8;
        self.alloc(8 * (level as usize + 1));
        for lc in 0..=level as usize {
            self.alloc(8 + 16 * layer_m(self.m, lc));
        }
        self.alloc(varsize);
        let block = if BLOCKS_DESCEND { ADDR_BLOCKS - 1 - block.min(ADDR_BLOCKS - 1) } else { block };
        let e = self.elems.len() as u32;
        let mut heaptids = [NONE; HEAPTIDS];
        heaptids[0] = heaptid;
        let nitems = 2 * self.m + level as usize * self.m;
        self.elems.push(BElem { level, nheap: 1, heaptids, addr: (block.min(ADDR_BLOCKS - 1)) << 17 | (off >> 3) as u32, items: self.items.len() as u32, heads: self.heads.len() as u32 });
        self.items.resize(self.items.len() + nitems, Cand { e: NONE, dist: 0.0, closer: false, addr: 0 });
        self.heads.resize(self.heads.len() + level as usize + 1, (0, false));
        self.vstart.push(self.vdata.len());
        self.vlen.push(value.len() as u32);
        self.vdata.extend_from_slice(value);
        e
    }
}

impl Build {
    /// HnswSearchLayer in memory (no discarded candidates, no element skipped): the ef
    /// nearest to element `q`'s value at layer `lc` from candidates `ep` (indices of `sc`),
    /// furthest first.
    fn search_layer(&mut self, q: u32, ep: &[u32], ef: usize, lc: usize) -> Vec<u32> {
        let mut c = CHeap::new();
        let mut w = WHeap::new();
        let mut wlen = 0usize;
        // InitVisited: a new stamp.
        if self.stamp.len() < self.elems.len() {
            self.stamp.resize(self.elems.len(), 0);
        }
        self.stamp_gen = self.stamp_gen.wrapping_add(1);
        if self.stamp_gen == 0 {
            self.stamp.fill(0);
            self.stamp_gen = 1;
        }
        let g = self.stamp_gen;
        for &s in ep {
            self.stamp[self.sc[s as usize].e as usize] = g;
            c.add(&mut self.sc, s);
            w.add(&mut self.sc, s);
            wlen += 1;
        }
        while !c.is_empty() {
            let ci = c.remove_first(&mut self.sc);
            let f = w.first();
            if self.sc[ci as usize].dist > self.sc[f as usize].dist {
                break;
            }
            // HnswLoadUnvisitedFromMemory
            let ce = self.sc[ci as usize].e;
            let (start, len) = (self.layer_start(ce, lc), self.heads[self.head(ce, lc)].0 as usize);
            self.unvisited.clear();
            for i in start..start + len {
                let n = self.items[i].e;
                if self.stamp[n as usize] != g {
                    self.stamp[n as usize] = g;
                    self.unvisited.push(n);
                }
            }
            for i in 0..self.unvisited.len() {
                let u = self.unvisited[i];
                let f = w.first();
                let d = self.dist(q, u);
                let always = wlen < ef;
                if !(d < self.sc[f as usize].dist || always) {
                    continue;
                }
                // Make robust to issues
                if (self.elems[u as usize].level as usize) < lc {
                    continue;
                }
                self.sc.push(Sc::new(u, d));
                let n = (self.sc.len() - 1) as u32;
                c.add(&mut self.sc, n);
                w.add(&mut self.sc, n);
                wlen += 1;
                if wlen > ef {
                    w.remove_first(&mut self.sc);
                }
            }
        }
        let mut out = Vec::with_capacity(ef);
        while !w.is_empty() {
            out.push(w.remove_first(&mut self.sc));
        }
        out
    }

    /// HnswFindElementNeighbors (algorithm 1) in memory: element `e`'s neighbors at each of
    /// its layers, from entry point `entry`.
    fn find_neighbors(&mut self, e: u32, entry: u32) {
        if entry == NONE {
            return;
        }
        self.sc.clear();
        self.sc.push(Sc::new(entry, self.dist(e, entry)));
        let mut ep = vec![0u32];
        let entry_level = self.elems[entry as usize].level as usize;
        let level = self.elems[e as usize].level as usize;
        // 1st phase: greedy search to insert level
        for lc in (level + 1..=entry_level).rev() {
            ep = self.search_layer(e, &ep, 1, lc);
        }
        // 2nd phase
        for lc in (0..=level.min(entry_level)).rev() {
            let lm = layer_m(self.m, lc);
            let w = self.search_layer(e, &ep, self.ef_construction, lc);
            let mut lw: Vec<Cand> = w.iter().map(|&s| self.sc[s as usize]).map(|s| Cand { e: s.e, dist: s.dist as f32, closer: false, addr: self.elems[s.e as usize].addr }).collect();
            let h = self.head(e, lc);
            let mut closer_set = self.heads[h].1;
            let (r, _) = {
                let this = &*self;
                select_neighbors(&mut lw, lm, &mut closer_set, None, false, &|a, b| this.dist(a, b))
            };
            self.heads[h].1 = closer_set;
            // AddConnections
            let start = self.layer_start(e, lc);
            let mut len = self.heads[h].0 as usize;
            for i in r {
                self.items[start + len] = lw[i];
                len += 1;
            }
            self.heads[h].0 = len as u16;
            ep = w;
        }
    }

    /// FindDuplicateInMemory: whether `e`'s value is a neighbor's (the nearest come first)
    /// with room for another heap tid, which then gets `e`'s.
    fn find_duplicate(&mut self, e: u32) -> bool {
        let (start, len) = (self.layer_start(e, 0), self.heads[self.head(e, 0)].0 as usize);
        for i in start..start + len {
            let n = self.items[i].e;
            if self.value(n) != self.value(e) {
                return false;
            }
            let ne = &mut self.elems[n as usize];
            if (ne.nheap as usize) < HEAPTIDS {
                let h = self.elems[e as usize].heaptids[0];
                let ne = &mut self.elems[n as usize];
                ne.heaptids[ne.nheap as usize] = h;
                ne.nheap += 1;
                return true;
            }
        }
        false
    }

    /// UpdateNeighborsInMemory: each neighbor of `e` gets `e` as a neighbor if it is among
    /// its best (HnswUpdateConnection).
    fn update_neighbors(&mut self, e: u32) {
        let new_addr = self.elems[e as usize].addr;
        for lc in (0..=self.elems[e as usize].level as usize).rev() {
            let lm = layer_m(self.m, lc);
            let (start, len) = (self.layer_start(e, lc), self.heads[self.head(e, lc)].0 as usize);
            for i in start..start + len {
                let hc = self.items[i];
                let n = hc.e;
                let h = self.head(n, lc);
                let nstart = self.layer_start(n, lc);
                let (nlen, mut closer_set) = self.heads[h];
                let newhc = Cand { e, dist: hc.dist, closer: false, addr: new_addr };
                if (nlen as usize) < lm {
                    self.items[nstart + nlen as usize] = newhc;
                    self.heads[h].0 = nlen + 1;
                    continue;
                }
                let mut scratch = std::mem::take(&mut self.scratch);
                scratch.clear();
                scratch.extend_from_slice(&self.items[nstart..nstart + lm]);
                {
                    let this = &*self;
                    update_connection(&mut scratch, &mut closer_set, newhc, lm, &|a, b| this.dist(a, b));
                }
                self.items[nstart..nstart + lm].copy_from_slice(&scratch[..lm]);
                self.heads[h].1 = closer_set;
                self.scratch = scratch;
            }
        }
    }

    /// InsertTuple: a row's value (not NULL) into the graph, in memory while it fits, else
    /// on disk. Whether it was indexed (a zero vector under a normalizing class isn't).
    pub fn insert(&mut self, dkey: &[u8], v: &Value, ctx: &ExecCtx) -> Result<bool> {
        let Some(v) = self.sup.form(v)? else {
            return Ok(false);
        };
        self.sup.check_dim(self.dims, &v)?;
        let bytes = self.sup.bytes(&v);
        let varsize = self.sup.varsize(&bytes);
        if !self.flushed && self.memory_used >= self.memory_total {
            ctx.notice(
                super::notice(format!("hnsw graph no longer fits into maintenance_work_mem after {} tuples", self.indtuples as i64))
                    .detail("Building will take significantly more time.")
                    .hint("Increase maintenance_work_mem to speed up builds."),
            );
            self.flush()?;
        }
        if self.flushed {
            super::insert::insert_on_disk(&mut self.graph, &self.sup, &v, &bytes, varsize, dkey, self.ef_construction, ctx)?;
            return Ok(true);
        }
        let heaptid = self.graph.add_heap(dkey);
        let e = self.init_element(heaptid, &bytes, varsize, ctx);
        let entry = self.entry;
        self.find_neighbors(e, entry);
        // UpdateGraphInMemory
        if self.find_duplicate(e) {
            return Ok(true);
        }
        self.list.push(e);
        self.update_neighbors(e);
        if entry == NONE || self.elems[e as usize].level > self.elems[entry as usize].level {
            self.entry = e;
        }
        Ok(true)
    }

    /// FlushPages: the graph written out as pages (CreateGraphPages, newest element first,
    /// then WriteNeighborTuples), the memory graph let go.
    pub fn flush(&mut self) -> Result<()> {
        let mut slot = vec![NONE; self.elems.len()];
        // (The graph takes the values over where they are: never held twice.)
        self.graph.adopt_values(std::mem::take(&mut self.vdata));
        for &e in self.list.iter().rev() {
            let el = &self.elems[e as usize];
            let (level, heaptids) = (el.level, el.heaptids);
            let nheap = el.nheap as usize;
            let (s, n) = (self.vstart[e as usize], self.vlen[e as usize] as usize);
            let varsize = self.sup.varsize(self.graph.value_bytes(s, n));
            slot[e as usize] = self.graph.place_built_at(level, &heaptids[..nheap], (s, n), varsize)?;
        }
        self.graph.finish_built(if self.entry == NONE { NONE } else { slot[self.entry as usize] });
        let m = self.m;
        for &e in &self.list {
            let level = self.elems[e as usize].level as usize;
            let mut out = Vec::with_capacity((level + 2) * m);
            for lc in (0..=level).rev() {
                let (start, len) = (self.layer_start(e, lc), self.heads[self.head(e, lc)].0 as usize);
                for i in 0..layer_m(m, lc) {
                    out.push(if i < len { slot[self.items[start + i].e as usize] } else { NONE });
                }
            }
            self.graph.elems[slot[e as usize] as usize].neighbors = out.into_boxed_slice();
        }
        self.flushed = true;
        // MemoryContextReset(graphCtx)
        self.elems = Vec::new();
        self.list = Vec::new();
        self.items = Vec::new();
        self.heads = Vec::new();
        self.vstart = Vec::new();
        self.vlen = Vec::new();
        self.vdata = Vec::new();
        self.stamp = Vec::new();
        self.entry = NONE;
        Ok(())
    }
}
