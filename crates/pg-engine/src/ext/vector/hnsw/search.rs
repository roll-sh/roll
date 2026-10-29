//! `hnswutils.c`'s graph algorithms shared by the build, inserts, scans and VACUUM:
//! SelectNeighbors (algorithm 4), HnswUpdateConnection, and HnswSearchLayer (algorithm 2)
//! over the on-disk graph. (The build's in-memory search is in `build`.)

use super::graph::{Graph, NONE, layer_m};
use super::mem::{AllocSet, Chunk, ListMem, TidHash};
use super::pheap::{CHeap, DHeap, Sc, WHeap};
use crate::Result;

/// HnswCandidate: an element (an index into the caller's elements), its distance as a
/// float, the cached "closer" flag, and the element's address (pgvector's tie-breaker for
/// equal distances when it sorts candidates).
#[derive(Clone, Copy, Debug)]
pub(super) struct Cand {
    pub e: u32,
    pub dist: f32,
    pub closer: bool,
    pub addr: u32,
}

/// CheckElementCloser: whether `e` is closer to the query than to every element of `r`.
fn closer_than(c: &[Cand], e: usize, r: &[usize], dist: &impl Fn(u32, u32) -> f64) -> bool {
    let ce = c[e];
    for &ri in r {
        let d = dist(ce.e, c[ri].e) as f32;
        if d <= ce.dist {
            return false;
        }
    }
    true
}

/// SelectNeighbors (algorithm 4, with pgvector's caching of `closer`) over the list `c`:
/// the positions kept (at most `lm`), and the pruned candidate (for HnswUpdateConnection).
/// `sort`: order the candidates first (descending distance, then address), which makes the
/// cached flags reusable (`closer_set`).
pub(super) fn select_neighbors(c: &mut [Cand], lm: usize, closer_set: &mut bool, new: Option<usize>, sort: bool, dist: &impl Fn(u32, u32) -> f64) -> (Vec<usize>, Option<usize>) {
    let mut w: Vec<usize> = (0..c.len()).collect();
    if w.len() <= lm {
        return (w, None);
    }
    let must_calculate = !*closer_set;
    let mut r: Vec<usize> = Vec::with_capacity(lm);
    let mut wd: Vec<usize> = Vec::new();
    let mut added: Vec<usize> = Vec::new();
    let mut removed_any = false;
    if sort {
        // CompareCandidateDistances: descending distance, then descending address. (NaN,
        // which C leaves unordered, sorts by its bits so that the order stays total.)
        w.sort_unstable_by(|&a, &b| {
            let (x, y) = (&c[a], &c[b]);
            let d = y.dist.partial_cmp(&x.dist).unwrap_or_else(|| y.dist.total_cmp(&x.dist));
            d.then(y.addr.cmp(&x.addr))
        });
    }
    while !w.is_empty() && r.len() < lm {
        // Assumes w is already ordered desc
        let e = w.pop().unwrap();
        if must_calculate {
            c[e].closer = closer_than(c, e, &r, dist);
        } else if !added.is_empty() {
            // A candidate that was closer only needs comparing with the ones added since.
            if c[e].closer {
                c[e].closer = closer_than(c, e, &added, dist);
                if !c[e].closer {
                    removed_any = true;
                }
            } else if removed_any {
                // One that wasn't might be now.
                c[e].closer = closer_than(c, e, &r, dist);
                if c[e].closer {
                    added.push(e);
                }
            }
        } else if Some(e) == new {
            c[e].closer = closer_than(c, e, &r, dist);
            if c[e].closer {
                added.push(e);
            }
        }
        if c[e].closer {
            r.push(e);
        } else {
            wd.push(e);
        }
    }
    // Cached value can only be used in future if sorted deterministically
    *closer_set = sort;
    // Keep pruned connections
    let mut wdoff = 0;
    while wdoff < wd.len() && r.len() < lm {
        r.push(wd[wdoff]);
        wdoff += 1;
    }
    let pruned = if wdoff < wd.len() { wd.get(wdoff).copied() } else { w.first().copied() };
    (r, pruned)
}

/// HnswUpdateConnection: give a neighbor list (`items`, at most `lm`) the new element
/// `new` if it is among the best: appended while there is room, else in place of the
/// candidate SelectNeighbors prunes. The position it took (None: not taken; Some(-2):
/// appended).
pub(super) fn update_connection(items: &mut Vec<Cand>, closer_set: &mut bool, new: Cand, lm: usize, dist: &impl Fn(u32, u32) -> f64) -> Option<i32> {
    if items.len() < lm {
        items.push(new);
        return Some(-2);
    }
    // Shrink connections
    items.push(new);
    let n = items.len() - 1;
    let (_, pruned) = select_neighbors(items, lm, closer_set, Some(n), true, dist);
    let newhc = items.pop().unwrap();
    let p = pruned?;
    if p == n {
        // (The new candidate itself: no element of the list to replace.)
        return None;
    }
    items[p] = newhc;
    Some(p as i32)
}

/// An element as a search loaded it from its tuple (HnswLoadElementFromTuple): what the
/// search goes on is the tuple as it was then.
#[derive(Clone, Copy, Debug)]
pub(super) struct Loaded {
    pub slot: u32,
    pub level: u8,
    pub version: u8,
    /// heaptidsLength, and (scans) where its heap tids were copied to in `Disk::pool`.
    pub nheap: u8,
    pub heap: u32,
}

/// sizeof(HnswElementData).
pub(super) const ELEMENT_SIZE: usize = 128;
/// sizeof(HnswSearchCandidate).
pub(super) const CANDIDATE_SIZE: usize = 64;
/// sizeof(pairingheap).
const PAIRINGHEAP_SIZE: usize = 24;

/// How a search goes (HnswSearchLayer's arguments).
#[derive(Clone, Copy, Default)]
pub(super) struct Opts {
    /// Elements being deleted don't count towards ef (a VACUUM repair's skipElement).
    pub skip: bool,
    /// Use the scan's own visited set (`Disk::vis`), else one for this search.
    pub keep_visited: bool,
    pub init_visited: bool,
    /// Count the elements visited (`so->tuples`).
    pub count: bool,
}

/// The state of searches over the on-disk graph: their candidates and loaded elements,
/// and for a scan, the memory its context has allocated.
#[derive(Default)]
pub(super) struct Disk {
    pub sc: Vec<Sc>,
    pub loaded: Vec<Loaded>,
    /// Heap tids of loaded elements (scans).
    pub pool: Vec<u32>,
    /// The scan's memory context (None: not a scan, nothing is measured).
    pub mem: Option<AllocSet>,
    pub vis: Option<TidHash>,
    pub tuples: i64,
    /// A visited set by slot, for searches that don't measure memory: a slot is visited
    /// when its stamp is the current search's.
    stamp: Vec<u32>,
    stamp_gen: u32,
    unvisited: Vec<u32>,
    /// The unvisited elements' distances (`search_layer`).
    dists: Vec<f64>,
}

/// "cannot load deleted element".
fn deleted_element() -> crate::Error {
    crate::error::internal("cannot load deleted element")
}

impl Disk {
    /// A state that measures memory as a scan's context (AllocSetContextCreate(0, 8 kB,
    /// 256 kB)).
    pub fn scan() -> Disk {
        Disk { mem: Some(AllocSet::new(0, 8 * 1024, 256 * 1024)), ..Default::default() }
    }

    pub fn alloc(&mut self, size: usize) -> Option<Chunk> {
        self.mem.as_mut().map(|m| m.alloc(size))
    }

    /// HnswLoadElementImpl's load of the tuple of `slot` (an element struct allocated).
    fn load(&mut self, g: &Graph, slot: u32) -> u32 {
        let e = &g.elems[slot as usize];
        let heap = if self.mem.is_some() {
            let at = self.pool.len() as u32;
            self.pool.extend_from_slice(e.heaptids());
            at
        } else {
            0
        };
        self.loaded.push(Loaded { slot, level: e.level, version: e.version, nheap: e.nheap, heap });
        (self.loaded.len() - 1) as u32
    }

    /// HnswEntryCandidate: the entry point loaded (`loaded`: already, by the caller) and
    /// its distance to `q`, as a candidate.
    pub fn entry_candidate(&mut self, g: &Graph, dist: super::types::DistFn, q: Option<&[u8]>, slot: u32) -> Result<u32> {
        if g.elems[slot as usize].deleted {
            return Err(deleted_element());
        }
        let d = q.map_or(0.0, |q| dist(q, g.value(slot)));
        let l = self.load(g, slot);
        self.alloc(CANDIDATE_SIZE);
        self.sc.push(Sc::new(l, d));
        Ok((self.sc.len() - 1) as u32)
    }

    /// AddToVisited of an index tid (`slot`): whether it was visited already.
    #[inline(always)]
    fn visit(&mut self, g: &Graph, slot: u32, local: &mut Option<TidHash>) -> bool {
        match &mut self.mem {
            Some(m) => {
                let t = if local.is_some() { local.as_mut().unwrap() } else { self.vis.as_mut().unwrap() };
                t.insert(m, g.tid_key(slot))
            }
            None => {
                let s = &mut self.stamp[slot as usize];
                let found = *s == self.stamp_gen;
                *s = self.stamp_gen;
                found
            }
        }
    }

    /// The loaded element's neighbors at layer `lc` not visited yet (HnswLoadNeighborTids +
    /// HnswLoadUnvisitedFromDisk): none when its neighbor tuple changed since it was loaded.
    fn unvisited(&mut self, g: &Graph, l: Loaded, lc: usize, local: &mut Option<TidHash>) {
        self.unvisited.clear();
        let m = g.m as usize;
        let e = &g.elems[l.slot as usize];
        if e.nversion != l.version || e.neighbors.len() != (l.level as usize + 2) * m {
            return;
        }
        let start = (l.level as usize - lc) * m;
        for i in 0..layer_m(m, lc) {
            let n = g.elems[l.slot as usize].neighbors[start + i];
            if n == NONE {
                break;
            }
            if !self.visit(g, n, local) {
                self.unvisited.push(n);
            }
        }
    }

    /// HnswSearchLayer (algorithm 2) at layer `lc` from entry candidates `ep`: the ef
    /// nearest candidates found, furthest first (the list w). With `discarded`, candidates
    /// not kept go there (iterative scans). Also the list's memory (scans).
    #[allow(clippy::too_many_arguments)]
    pub fn search_layer(&mut self, g: &Graph, dist: super::types::Dists, q: Option<&[u8]>, ep: &[u32], ef: usize, lc: usize, o: Opts, mut discarded: Option<&mut DHeap>) -> Result<(Vec<u32>, ListMem)> {
        let m = g.m as usize;
        let lm = layer_m(m, lc);
        let mut c = CHeap::new();
        let mut w = WHeap::new();
        let mut wlen = 0usize;
        let mut local: Option<TidHash> = None;
        if let Some(mm) = &mut self.mem {
            mm.alloc(PAIRINGHEAP_SIZE);
            mm.alloc(PAIRINGHEAP_SIZE);
            mm.alloc(lm * 8);
        }
        let init = o.init_visited || !o.keep_visited;
        if init {
            match &mut self.mem {
                Some(mm) => {
                    let t = TidHash::create(mm, (ef * m * 2) as u32);
                    if o.keep_visited {
                        self.vis = Some(t);
                    } else {
                        local = Some(t);
                    }
                    if discarded.is_some() {
                        mm.alloc(PAIRINGHEAP_SIZE);
                    }
                }
                None => {
                    if self.stamp.len() < g.elems.len() {
                        self.stamp.resize(g.elems.len(), 0);
                    }
                    self.stamp_gen = self.stamp_gen.wrapping_add(1);
                    if self.stamp_gen == 0 {
                        self.stamp.fill(0);
                        self.stamp_gen = 1;
                    }
                }
            }
        }
        // Add entry points to v, C, and W
        for &s in ep {
            if init {
                let slot = self.loaded[self.sc[s as usize].e as usize].slot;
                self.visit(g, slot, &mut local);
                if o.count {
                    self.tuples += 1;
                }
            }
            c.add(&mut self.sc, s);
            w.add(&mut self.sc, s);
            if !o.skip || self.loaded[self.sc[s as usize].e as usize].nheap != 0 {
                wlen += 1;
            }
        }
        while !c.is_empty() {
            let ci = c.remove_first(&mut self.sc);
            let f = w.first();
            if self.sc[ci as usize].dist > self.sc[f as usize].dist {
                break;
            }
            let cl = self.loaded[self.sc[ci as usize].e as usize];
            self.unvisited(g, cl, lc, &mut local);
            if o.count {
                self.tuples += self.unvisited.len() as i64;
            }
            // The unvisited neighbors' distances, four at a time (vector values: a deleted
            // element's value is still there, and the loop below stops at it first).
            let four = q.zip(dist.four);
            if let Some((q, d4)) = four {
                let u = &self.unvisited;
                self.dists.clear();
                let mut i = 0;
                while let Some(n) = u.get(i..i + 4) {
                    self.dists.extend(d4(&[g.value(n[0]), g.value(n[1]), g.value(n[2]), g.value(n[3])], q));
                    i += 4;
                }
                self.dists.extend(u[i..].iter().map(|&n| (dist.one)(q, g.value(n))));
            }
            for i in 0..self.unvisited.len() {
                let u = self.unvisited[i];
                let f = w.first();
                let fdist = self.sc[f as usize].dist;
                let always = wlen < ef;
                let e = &g.elems[u as usize];
                if e.deleted {
                    return Err(deleted_element());
                }
                let d = match four {
                    Some(_) => self.dists[i],
                    None => q.map_or(0.0, |q| (dist.one)(q, g.value(u))),
                };
                // (An element is only loaded when it may be added.)
                if !(always || discarded.is_some() || d < fdist) {
                    continue;
                }
                self.alloc(ELEMENT_SIZE);
                let l = self.load(g, u);
                if !(d < fdist || always) {
                    if let Some(dh) = discarded.as_deref_mut() {
                        self.alloc(CANDIDATE_SIZE);
                        self.sc.push(Sc::new(l, d));
                        let n = (self.sc.len() - 1) as u32;
                        dh.add(&mut self.sc, n);
                    }
                    continue;
                }
                // Make robust to issues
                if (self.loaded[l as usize].level as usize) < lc {
                    continue;
                }
                self.alloc(CANDIDATE_SIZE);
                self.sc.push(Sc::new(l, d));
                let n = (self.sc.len() - 1) as u32;
                c.add(&mut self.sc, n);
                w.add(&mut self.sc, n);
                if !o.skip || self.loaded[l as usize].nheap != 0 {
                    wlen += 1;
                    if wlen > ef {
                        let d = w.remove_first(&mut self.sc);
                        if let Some(dh) = discarded.as_deref_mut() {
                            dh.add(&mut self.sc, d);
                        }
                    }
                }
            }
        }
        // Add each element of W to w
        let mut out = Vec::with_capacity(ef.min(1024));
        let mut list = ListMem::default();
        while !w.is_empty() {
            out.push(w.remove_first(&mut self.sc));
            if let Some(mm) = &mut self.mem {
                list.append(mm);
            }
        }
        Ok((out, list))
    }
}
