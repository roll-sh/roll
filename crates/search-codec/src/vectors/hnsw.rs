//! HNSW graphs: build and search.
//!
//! Construction follows Lucene's `HnswGraphBuilder`: nodes get a random top level (`ml =
//! 1 / ln m`, a seeded generator so segments are reproducible), are linked greedily from the
//! entry point down to their level, and choose neighbors with the diversity heuristic (keep a
//! candidate only if it is closer to the node than to every neighbor already kept). Reverse
//! links that overflow a node's capacity (`2m` on level 0, `m` above) evict the worst
//! non-diverse neighbor, or the worst one. Insertions run on several threads: a node's levels
//! are all searched first and then linked bottom-up, own list before reverse links, so a
//! concurrent search never meets a half-linked node. A final pass links every node the entry
//! point cannot reach (Lucene's `connectComponents`): the diversity heuristic orphans nodes in
//! tight clusters and duplicates.
//!
//! On disk a level is a flat array of fixed-width adjacency rows (a `u16` count, then neighbor
//! ordinals bit-packed at `⌈log2 n⌉` bits), so a neighbor list is one row; upper levels also
//! store their sorted node list, binary-searched to find a row.
//!
//! Search is the standard best-first beam search: greedy (beam 1) on upper levels, beam `ef` on
//! level 0. With an accept filter, rejected nodes are still traversed but never returned, and
//! the search reports when it exceeded its visit budget so the caller can fall back to exact
//! search (Lucene's strategy for restrictive filters). Restrictive filters (under 60% of the
//! vectors) use the filtered search instead (Lucene's `FilteredHnswGraphSearcher`, after
//! ACORN-1): only accepted nodes are scored, rejected neighbors are bridges to their own
//! neighbors.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Mutex;

use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::slice::Slice;

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Scored {
    pub score: f32,
    pub ord: u32,
}

impl Eq for Scored {}

impl Ord for Scored {
    fn cmp(&self, o: &Scored) -> Ordering {
        // Higher score first; ties by lower ordinal (stable, like Lucene's doc tie-break).
        self.score.total_cmp(&o.score).then(o.ord.cmp(&self.ord))
    }
}

impl PartialOrd for Scored {
    fn partial_cmp(&self, o: &Scored) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Visited-set: a bitset that remembers which words it touched, so resetting costs the size of
/// the last search rather than of the graph.
pub(crate) struct Visited {
    words: Vec<u64>,
    touched: Vec<u32>,
}

impl Visited {
    pub(crate) fn new(n: usize) -> Visited {
        Visited {
            words: vec![0; n.div_ceil(64)],
            touched: Vec::new(),
        }
    }
    /// Makes room for `n` nodes.
    pub(crate) fn ensure(&mut self, n: usize) {
        let w = n.div_ceil(64);
        if self.words.len() < w {
            self.words.resize(w.next_power_of_two(), 0);
        }
    }
    pub(crate) fn reset(&mut self) {
        for &w in &self.touched {
            self.words[w as usize] = 0;
        }
        self.touched.clear();
    }
    /// Marks `i`; returns whether it was unvisited.
    #[inline]
    pub(crate) fn insert(&mut self, i: u32) -> bool {
        let w = (i >> 6) as usize;
        let bit = 1u64 << (i & 63);
        let word = &mut self.words[w];
        if *word & bit != 0 {
            return false;
        }
        if *word == 0 {
            self.touched.push(w as u32);
        }
        *word |= bit;
        true
    }
}

thread_local! {
    /// Per-thread visited set reused by searches, so a query allocates nothing proportional to
    /// the graph.
    static VISITED: std::cell::RefCell<Visited> = std::cell::RefCell::new(Visited::new(0));
}

/// Runs `f` with a cleared visited set for `n` nodes.
pub(crate) fn with_visited<R>(n: usize, f: impl FnOnce(&mut Visited) -> R) -> R {
    VISITED.with(|cell| match cell.try_borrow_mut() {
        Ok(mut v) => {
            v.ensure(n);
            v.reset();
            let r = f(&mut v);
            v.reset();
            r
        }
        Err(_) => f(&mut Visited::new(n)),
    })
}

/// Access to a graph's adjacency, in memory or on disk.
pub(crate) trait Adjacency {
    fn num_levels(&self) -> usize;
    fn entry(&self) -> Option<u32>;
    fn for_each_neighbor(&self, level: usize, ord: u32, f: impl FnMut(u32));
}

/// Reusable heaps of [`search_layer`].
#[derive(Default)]
pub(crate) struct Scratch {
    candidates: BinaryHeap<Scored>,
    results: BinaryHeap<std::cmp::Reverse<Scored>>,
    nbrs: Vec<u32>,
}

/// Best-first search on one level from `entry` points; leaves up to `ef` best in `out`, best
/// first, and returns whether the visit budget sufficed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_layer<G: Adjacency, S: FnMut(u32) -> f32, P: Fn(u32)>(
    g: &G,
    score: &mut S,
    prefetch: &P,
    entry: &[Scored],
    ef: usize,
    level: usize,
    visited: &mut Visited,
    accept: Option<&dyn Fn(u32) -> bool>,
    visit_budget: &mut usize,
    scratch: &mut Scratch,
    out: &mut Vec<Scored>,
) -> bool {
    let Scratch { candidates, results, nbrs } = scratch;
    candidates.clear();
    results.clear();
    for &e in entry {
        if visited.insert(e.ord) {
            candidates.push(e);
            if accept.is_none_or(|a| a(e.ord)) {
                results.push(std::cmp::Reverse(e));
            }
        }
    }
    while results.len() > ef {
        results.pop();
    }
    let mut complete = true;
    while let Some(c) = candidates.pop() {
        if results.len() >= ef && c.score < results.peek().unwrap().0.score {
            break;
        }
        // Collect the unvisited neighbors and prefetch their vectors before scoring any, so the
        // memory accesses overlap instead of stalling one after another.
        nbrs.clear();
        g.for_each_neighbor(level, c.ord, |nb| {
            if visited.insert(nb) {
                nbrs.push(nb);
            }
        });
        for &nb in nbrs.iter() {
            prefetch(nb);
        }
        for &nb in nbrs.iter() {
            if *visit_budget == 0 {
                complete = false;
                break;
            }
            *visit_budget -= 1;
            let s = score(nb);
            let worst = results.peek().map(|r| r.0.score);
            if results.len() < ef || worst.is_none_or(|w| s > w) {
                candidates.push(Scored { score: s, ord: nb });
                if accept.is_none_or(|a| a(nb)) {
                    results.push(std::cmp::Reverse(Scored { score: s, ord: nb }));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
        if !complete {
            break;
        }
    }
    out.clear();
    out.extend(results.drain().map(|r| r.0));
    out.sort_unstable_by(|a, b| b.cmp(a));
    complete
}

/// The neighbors of one node during construction: the first `sorted` entries are ordered by
/// descending score and were checked for diversity together; later entries were appended by
/// reverse links and are checked only when the list overflows (Lucene's `NeighborArray`).
#[derive(Clone, Default)]
struct Nbrs {
    items: Vec<Scored>,
    sorted: usize,
}

/// Per-thread state of graph construction.
#[derive(Default)]
struct BuildCtx {
    visited: Option<Visited>,
    scratch: Scratch,
    ep: Vec<Scored>,
    cands: Vec<Scored>,
    unchecked: Vec<usize>,
    /// Neighbors chosen per level for the node being inserted.
    chosen: Vec<Vec<Scored>>,
}

/// An in-memory graph under construction over ordinals `0..n`.
///
/// Levels are assigned up front (with a seeded generator, so they are reproducible), which fixes
/// the nodes of every level before any insertion: neighbor lists are preallocated behind
/// per-node mutexes and the node that becomes the entry point (the first one on the highest
/// level) is inserted first. The remaining insertions never change the entry point, so they can
/// run on several threads, each locking only the one list it reads or links at a time (Lucene's
/// concurrent merge builder works the same way). With one thread the graph is deterministic.
pub(crate) struct GraphBuilder {
    m: usize,
    ef_construction: usize,
    levels: Vec<u8>,
    seeded: Vec<bool>,
    level0: Vec<Mutex<Nbrs>>,
    upper: Vec<HashMap<u32, Mutex<Nbrs>>>,
    entry: Option<u32>,
    max_level: usize,
    rng: u64,
}

fn lock(m: &Mutex<Nbrs>) -> std::sync::MutexGuard<'_, Nbrs> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Adjacency for GraphBuilder {
    fn num_levels(&self) -> usize {
        self.max_level + 1
    }
    fn entry(&self) -> Option<u32> {
        self.entry
    }
    fn for_each_neighbor(&self, level: usize, ord: u32, mut f: impl FnMut(u32)) {
        let list = if level == 0 {
            self.level0.get(ord as usize)
        } else {
            self.upper.get(level - 1).and_then(|u| u.get(&ord))
        };
        if let Some(l) = list {
            for s in &lock(l).items {
                f(s.ord);
            }
        }
    }
}

impl GraphBuilder {
    pub(crate) fn new(n: usize, m: usize, ef_construction: usize) -> GraphBuilder {
        GraphBuilder {
            m: m.max(2),
            ef_construction: ef_construction.max(1),
            levels: vec![0; n],
            seeded: vec![false; n],
            level0: (0..n).map(|_| Mutex::new(Nbrs::default())).collect(),
            upper: Vec::new(),
            entry: None,
            max_level: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn random_level(&mut self) -> usize {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let u = ((z >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let ml = 1.0 / (self.m as f64).ln();
        ((-u.ln() * ml) as usize).min(15)
    }

    fn max_conn(&self, level: usize) -> usize {
        if level == 0 { 2 * self.m } else { self.m }
    }

    fn list(&self, level: usize, ord: u32) -> &Mutex<Nbrs> {
        if level == 0 {
            &self.level0[ord as usize]
        } else {
            &self.upper[level - 1][&ord]
        }
    }

    fn ensure_levels(&mut self, levels: usize) {
        while self.upper.len() + 1 < levels {
            self.upper.push(HashMap::new());
        }
    }

    /// Starts from an existing graph (a merge input's): node `i` of `g` becomes `map[i]`
    /// (`u32::MAX`: dropped; the caller only offers seeds that drop nothing, since dropping nodes
    /// could disconnect the graph). Edge scores are recomputed with `sim`, which is far cheaper
    /// than inserting the nodes again. Call before [`build`](Self::build).
    pub(crate) fn seed<F: Fn(u32, u32) -> f32>(&mut self, g: &GraphReader, map: &[u32], sim: &F) {
        let Some(entry) = g.entry() else { return };
        let n = self.level0.len() as u32;
        let mapped = |o: u32| map.get(o as usize).copied().filter(|&x| x < n);
        let Some(new_entry) = mapped(entry) else { return };
        let levels = g.num_levels();
        self.ensure_levels(levels);
        for level in 0..levels {
            let nodes: Vec<u32> = if level == 0 { (0..g.n).collect() } else { g.level_nodes(level).collect() };
            for old in nodes {
                let Some(node) = mapped(old) else { continue };
                let mut items = Vec::new();
                g.for_each_neighbor(level, old, |nb| {
                    if let Some(x) = mapped(nb) {
                        items.push(Scored { score: sim(node, x), ord: x });
                    }
                });
                items.sort_unstable_by(|a, b| b.cmp(a));
                items.truncate(self.max_conn(level));
                let sorted = items.len();
                let nbrs = Nbrs { items, sorted };
                if level == 0 {
                    self.seeded[node as usize] = true;
                    *lock(&self.level0[node as usize]) = nbrs;
                } else {
                    self.levels[node as usize] = self.levels[node as usize].max(level as u8);
                    self.upper[level - 1].insert(node, Mutex::new(nbrs));
                }
            }
        }
        self.entry = Some(new_entry);
        self.max_level = levels - 1;
    }

    /// Inserts every node not seeded, on up to `threads` threads, then makes every node
    /// reachable from the entry point.
    pub(crate) fn build<F: Fn(u32, u32) -> f32 + Sync>(&mut self, sim: &F, threads: usize) {
        self.insert_all(sim, threads);
        self.connect_components(sim);
    }

    fn insert_all<F: Fn(u32, u32) -> f32 + Sync>(&mut self, sim: &F, threads: usize) {
        let n = self.level0.len();
        let mut todo = Vec::with_capacity(n);
        for ord in 0..n {
            if !self.seeded[ord] {
                let level = self.random_level();
                self.levels[ord] = level as u8;
                todo.push(ord as u32);
            }
        }
        let top = todo.iter().map(|&o| self.levels[o as usize] as usize).max();
        let Some(top) = top else { return };
        self.ensure_levels(top + 1);
        for &o in &todo {
            for l in 1..=self.levels[o as usize] as usize {
                self.upper[l - 1].insert(o, Mutex::new(Nbrs::default()));
            }
        }
        // The node that may become the entry point goes first, alone.
        let first = if self.entry.is_none() || top > self.max_level {
            todo.iter().position(|&o| self.levels[o as usize] as usize == top)
        } else {
            None
        };
        let mut ctx = BuildCtx::default();
        if let Some(i) = first {
            let o = todo.remove(i);
            match self.entry {
                None => {
                    self.entry = Some(o);
                    self.max_level = top;
                }
                Some(_) => {
                    self.insert(&mut ctx, o, sim);
                    self.entry = Some(o);
                    self.max_level = top;
                }
            }
        }
        let threads = threads.clamp(1, 64).min(todo.len().div_ceil(256).max(1));
        if threads == 1 {
            for &o in &todo {
                self.insert(&mut ctx, o, sim);
            }
            return;
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        let this = &*self;
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    let mut ctx = BuildCtx::default();
                    loop {
                        let start = next.fetch_add(64, std::sync::atomic::Ordering::Relaxed);
                        if start >= todo.len() {
                            break;
                        }
                        for &o in &todo[start..(start + 64).min(todo.len())] {
                            this.insert(&mut ctx, o, sim);
                        }
                    }
                });
            }
        });
    }

    /// Links node `ord` into the graph (the entry point exists and is at least as high).
    ///
    /// All levels are searched first, then linked bottom-up, each level's own list before the
    /// reverse links (Lucene's order): a concurrent search that reaches the node through a
    /// reverse link on some level then always finds its neighbors there and on every level
    /// below, instead of a dead end that would derail its beam.
    fn insert<F: Fn(u32, u32) -> f32>(&self, ctx: &mut BuildCtx, ord: u32, sim: &F) {
        let Some(entry) = self.entry else { return };
        let level = self.levels[ord as usize] as usize;
        let n = self.level0.len();
        let visited = ctx.visited.get_or_insert_with(|| Visited::new(n));
        let mut score = |o: u32| sim(ord, o);
        ctx.ep.clear();
        ctx.ep.push(Scored { score: score(entry), ord: entry });
        let mut budget = usize::MAX;
        for l in ((level + 1)..=self.max_level).rev() {
            visited.reset();
            search_layer(self, &mut score, &|_| {}, &ctx.ep, 1, l, visited, None, &mut budget, &mut ctx.scratch, &mut ctx.cands);
            if !ctx.cands.is_empty() {
                std::mem::swap(&mut ctx.ep, &mut ctx.cands);
            }
        }
        let top = level.min(self.max_level);
        if ctx.chosen.len() <= top {
            ctx.chosen.resize_with(top + 1, Vec::new);
        }
        for l in (0..=top).rev() {
            visited.reset();
            search_layer(
                self,
                &mut score,
                &|_| {},
                &ctx.ep,
                self.ef_construction,
                l,
                visited,
                None,
                &mut budget,
                &mut ctx.scratch,
                &mut ctx.cands,
            );
            select_diverse(&ctx.cands, self.max_conn(l), sim, &mut ctx.chosen[l]);
            if !ctx.cands.is_empty() {
                std::mem::swap(&mut ctx.ep, &mut ctx.cands);
            }
        }
        visited.reset();
        for l in 0..=top {
            let max = self.max_conn(l);
            let chosen = &ctx.chosen[l];
            {
                let mut own = lock(self.list(l, ord));
                if own.items.is_empty() {
                    own.items.extend_from_slice(chosen);
                    own.sorted = chosen.len();
                } else {
                    // Other threads already linked to this node: merge, then trim.
                    for &c in chosen {
                        push_and_trim(&mut own, c, max, sim, &mut ctx.unchecked);
                    }
                }
            }
            for nb in chosen {
                let mut list = lock(self.list(l, nb.ord));
                push_and_trim(&mut list, Scored { score: nb.score, ord }, max, sim, &mut ctx.unchecked);
            }
        }
    }

    /// Links every node unreachable from the entry point back into the graph (Lucene's
    /// `HnswGraphBuilder.connectComponents`).
    ///
    /// The diversity heuristic can leave nodes, or small clusters, with no incoming link: a
    /// node whose only in-links were evicted by later reverse links, or concurrent insertions
    /// that did not see each other. Search can never return them. Per level, a traversal from
    /// the entry point marks what is reachable; each unreached node is linked from its nearest
    /// reachable node that still has room (found with a beam search filtered to such nodes),
    /// and everything reachable from it is marked in turn. If every candidate is full, the
    /// nearest one gives up its worst link, and the level is checked again, since that link may
    /// have been the only way to some other node. Costs `O(n · m)` per level plus one small
    /// search per orphan, negligible next to construction.
    fn connect_components<F: Fn(u32, u32) -> f32>(&self, sim: &F) {
        let Some(entry) = self.entry else { return };
        let n = self.level0.len();
        let mut ctx = BuildCtx::default();
        let mut visited = Visited::new(n);
        let mut reached = vec![0u64; n.div_ceil(64)];
        let mut stack = Vec::new();
        for level in 0..=self.max_level {
            let max = self.max_conn(level);
            let mut nodes: Vec<u32> = if level == 0 {
                (0..n as u32).collect()
            } else {
                self.upper[level - 1].keys().copied().collect()
            };
            nodes.sort_unstable();
            for _round in 0..4 {
                reached.fill(0);
                let mark = |from: u32, reached: &mut Vec<u64>, stack: &mut Vec<u32>| {
                    if reached[from as usize >> 6] >> (from & 63) & 1 != 0 {
                        return;
                    }
                    reached[from as usize >> 6] |= 1 << (from & 63);
                    stack.push(from);
                    while let Some(o) = stack.pop() {
                        self.for_each_neighbor(level, o, |nb| {
                            let w = &mut reached[nb as usize >> 6];
                            if *w >> (nb & 63) & 1 == 0 {
                                *w |= 1 << (nb & 63);
                                stack.push(nb);
                            }
                        });
                    }
                };
                mark(entry, &mut reached, &mut stack);
                let mut forced = false;
                for &u in &nodes {
                    if reached[u as usize >> 6] >> (u & 63) & 1 != 0 {
                        continue;
                    }
                    let target = self.nearest_reachable(&mut ctx, &mut visited, level, u, &reached, max, sim);
                    let Some(v) = target else { continue };
                    let score = sim(v.ord, u);
                    {
                        let mut list = lock(self.list(level, v.ord));
                        if list.items.len() >= max {
                            // Full: give up the worst link.
                            let worst = (0..list.items.len())
                                .min_by(|&a, &b| list.items[a].cmp(&list.items[b]))
                                .unwrap_or(0);
                            list.items.remove(worst);
                            if worst < list.sorted {
                                list.sorted -= 1;
                            }
                            forced = true;
                        }
                        // Appended unchecked, like a reverse link.
                        list.items.push(Scored { score, ord: u });
                    }
                    {
                        let mut own = lock(self.list(level, u));
                        if own.items.len() < max && own.items.iter().all(|s| s.ord != v.ord) {
                            own.items.push(Scored { score: sim(u, v.ord), ord: v.ord });
                        }
                    }
                    mark(u, &mut reached, &mut stack);
                }
                if !forced {
                    break;
                }
            }
        }
    }

    /// The node nearest to `u` among those `reached` on `level`, preferring nodes whose
    /// neighbor list has room.
    #[allow(clippy::too_many_arguments)]
    fn nearest_reachable<F: Fn(u32, u32) -> f32>(
        &self,
        ctx: &mut BuildCtx,
        visited: &mut Visited,
        level: usize,
        u: u32,
        reached: &[u64],
        max: usize,
        sim: &F,
    ) -> Option<Scored> {
        let entry = self.entry?;
        let mut score = |o: u32| sim(u, o);
        ctx.ep.clear();
        ctx.ep.push(Scored { score: score(entry), ord: entry });
        let mut budget = usize::MAX;
        for l in ((level + 1)..=self.max_level).rev() {
            visited.reset();
            search_layer(self, &mut score, &|_| {}, &ctx.ep, 1, l, visited, None, &mut budget, &mut ctx.scratch, &mut ctx.cands);
            if !ctx.cands.is_empty() {
                std::mem::swap(&mut ctx.ep, &mut ctx.cands);
            }
        }
        // The descent may end in a part of this level the entry cannot reach: seed the beam
        // with the entry as well, which is reached by definition.
        if ctx.ep.iter().all(|s| s.ord != entry) {
            ctx.ep.push(Scored { score: score(entry), ord: entry });
        }
        let is_reached = |o: u32| o != u && reached[o as usize >> 6] >> (o & 63) & 1 != 0;
        let has_room = |o: u32| is_reached(o) && lock(self.list(level, o)).items.len() < max;
        let ef = self.ef_construction.clamp(2, 32);
        visited.reset();
        search_layer(self, &mut score, &|_| {}, &ctx.ep, ef, level, visited, Some(&has_room), &mut budget, &mut ctx.scratch, &mut ctx.cands);
        visited.reset();
        if let Some(&best) = ctx.cands.first() {
            return Some(best);
        }
        // Every reachable node near `u` is full: take the nearest reachable one.
        search_layer(self, &mut score, &|_| {}, &ctx.ep, ef, level, visited, Some(&is_reached), &mut budget, &mut ctx.scratch, &mut ctx.cands);
        visited.reset();
        ctx.cands.first().copied()
    }

    /// Serializes the graph (see [`GraphReader`] for the layout).
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        let n = self.level0.len() as u32;
        let empty = self.entry.is_none();
        let bits = crate::bitpack::direct::bits_for(n.saturating_sub(1) as u64).max(1);
        out.put_u16(self.m as u16);
        out.put_u8(if empty { 0 } else { (self.max_level + 1) as u8 });
        out.put_u32(self.entry.unwrap_or(0));
        out.put_u32(if empty { 0 } else { n });
        out.put_u8(bits as u8);
        if empty {
            return;
        }
        for list in &self.level0 {
            write_row(out, &lock(list).items, 2 * self.m, bits);
        }
        out.put_bytes(&[0; 8]);
        for l in 1..=self.max_level {
            let map = &self.upper[l - 1];
            let mut nodes: Vec<u32> = map.keys().copied().collect();
            nodes.sort_unstable();
            out.put_u32(nodes.len() as u32);
            for &o in &nodes {
                out.put_u32(o);
            }
            for o in &nodes {
                write_row(out, &lock(&map[o]).items, self.m, bits);
            }
            out.put_bytes(&[0; 8]);
        }
    }
}

/// The diversity heuristic: keep a candidate (best first) only if it is closer to the node than
/// to every neighbor kept so far.
fn select_diverse<F: Fn(u32, u32) -> f32>(cands: &[Scored], max: usize, sim: &F, out: &mut Vec<Scored>) {
    out.clear();
    for &c in cands {
        if out.len() >= max {
            break;
        }
        if out.iter().all(|r| sim(c.ord, r.ord) < c.score) {
            out.push(c);
        }
    }
}

/// Adds a link to a neighbor list. Entries are appended unchecked; when the list overflows, the
/// unchecked ones are sorted into place and the worst entry that is closer to a better neighbor
/// than to the list's owner is evicted (else the worst). Only pairs involving unchecked entries
/// are compared, so a link costs O(list) similarity calls, not O(list²) (Lucene's
/// `NeighborArray`).
fn push_and_trim<F: Fn(u32, u32) -> f32>(
    list: &mut Nbrs,
    new: Scored,
    max: usize,
    sim: &F,
    unchecked: &mut Vec<usize>,
) {
    list.items.push(new);
    if list.items.len() <= max {
        return;
    }
    // Insert each unchecked entry into the sorted prefix, tracking where they end up.
    unchecked.clear();
    while list.sorted < list.items.len() {
        let s = list.sorted;
        let x = list.items[s];
        let at = list.items[..s].partition_point(|e| e.score >= x.score);
        list.items.copy_within(at..s, at + 1);
        list.items[at] = x;
        for u in unchecked.iter_mut() {
            if *u >= at {
                *u += 1;
            }
        }
        unchecked.push(at);
        list.sorted += 1;
    }
    unchecked.sort_unstable();
    while list.items.len() > max {
        let items = &list.items;
        let mut evict = items.len() - 1;
        let mut cursor = unchecked.len();
        'scan: for i in (1..items.len()).rev() {
            if cursor == 0 {
                break;
            }
            let c = items[i];
            if unchecked[cursor - 1] == i {
                // An unchecked candidate: compare with every better neighbor.
                for r in items[..i].iter().rev() {
                    if sim(c.ord, r.ord) >= c.score {
                        evict = i;
                        break 'scan;
                    }
                }
                cursor -= 1;
            } else {
                // A checked candidate: only the better unchecked entries are new to it.
                for &u in unchecked[..cursor].iter().rev() {
                    if sim(c.ord, items[u].ord) >= c.score {
                        evict = i;
                        break 'scan;
                    }
                }
            }
        }
        list.items.remove(evict);
        unchecked.retain(|&u| u != evict);
        for u in unchecked.iter_mut() {
            if *u > evict {
                *u -= 1;
            }
        }
    }
    list.sorted = list.items.len();
}

/// Bytes of one adjacency row: a `u16` count, then `max_conn` slots of `bits` bits.
fn row_len(max_conn: usize, bits: u32) -> usize {
    2 + (max_conn * bits as usize).div_ceil(8)
}

fn write_row(out: &mut Vec<u8>, list: &[Scored], max_conn: usize, bits: u32) {
    let start = out.len();
    out.put_u16(list.len().min(max_conn) as u16);
    let mut acc = 0u128;
    let mut filled = 0u32;
    for s in list.iter().take(max_conn) {
        acc |= (s.ord as u128) << filled;
        filled += bits;
        while filled >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        out.push(acc as u8);
    }
    out.resize(start + row_len(max_conn, bits), 0);
}

/// A graph on disk.
///
/// ```text
/// m u16 | levels u8 | entry u32 | n u32 | bits u8
/// level 0: n rows                               | 8 zero bytes
/// level l: count u32, count node ords (sorted)   | count rows | 8 zero bytes
/// row: neighbor count u16, then max_conn slots of `bits` (= ⌈log2 n⌉) bits
/// ```
///
/// Rows are fixed width, so a node's neighbors are one multiplication away, yet bit-packed
/// ordinals keep the graph about half the size of `u32` slots (it must stay in memory for fast
/// search). The padding lets every slot be read with one unaligned 8-byte load.
#[derive(Clone, Debug)]
pub(crate) struct GraphReader {
    m: usize,
    n: u32,
    entry: u32,
    bits: u32,
    level0: Slice,
    /// Per upper level: (sorted node ords, adjacency rows).
    upper: Vec<(Slice, Slice)>,
}

impl GraphReader {
    pub(crate) fn open(data: Slice) -> Result<GraphReader> {
        let mut r = In::new(&data);
        let m = r.u16()? as usize;
        let levels = r.u8()? as usize;
        let entry = r.u32()?;
        let n = r.u32()?;
        let bits = r.u8()? as u32;
        if m < 2 || bits == 0 || bits > 32 || (n > 0 && (levels == 0 || entry >= n)) {
            return Err(corrupt("bad graph header"));
        }
        if n == 0 {
            return Ok(GraphReader { m, n, entry, bits, level0: Slice::empty(), upper: Vec::new() });
        }
        let l0len = (n as usize)
            .checked_mul(row_len(2 * m, bits))
            .and_then(|x| x.checked_add(8))
            .ok_or_else(|| corrupt("graph size"))?;
        let level0 = data
            .try_slice(r.pos() as u64, l0len as u64)
            .ok_or_else(|| corrupt("graph truncated"))?;
        r.skip(l0len)?;
        let mut upper = Vec::new();
        for _ in 1..levels {
            let cnt = r.u32()? as usize;
            if cnt > n as usize {
                return Err(corrupt("bad graph level size"));
            }
            let nodes = data
                .try_slice(r.pos() as u64, cnt as u64 * 4)
                .ok_or_else(|| corrupt("graph truncated"))?;
            r.skip(cnt * 4)?;
            let rows_len = cnt * row_len(m, bits) + 8;
            let rows = data
                .try_slice(r.pos() as u64, rows_len as u64)
                .ok_or_else(|| corrupt("graph truncated"))?;
            r.skip(rows_len)?;
            upper.push((nodes, rows));
        }
        Ok(GraphReader { m, n, entry, bits, level0, upper })
    }

    /// Max connections per node above level 0 (`m`).
    pub(crate) fn m(&self) -> usize {
        self.m
    }

    /// Number of nodes (0 for an empty graph).
    pub(crate) fn len(&self) -> u32 {
        self.n
    }

    /// Nodes present on upper level `level` (>= 1).
    pub(crate) fn level_nodes(&self, level: usize) -> impl Iterator<Item = u32> + '_ {
        let nodes: &[u8] = self.upper.get(level - 1).map_or(&[], |u| &u.0[..]);
        nodes.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()))
    }
}

impl Adjacency for GraphReader {
    fn num_levels(&self) -> usize {
        if self.n == 0 { 0 } else { self.upper.len() + 1 }
    }
    fn entry(&self) -> Option<u32> {
        (self.n > 0).then_some(self.entry)
    }
    #[inline]
    fn for_each_neighbor(&self, level: usize, ord: u32, mut f: impl FnMut(u32)) {
        let (rows, idx, max_conn) = if level == 0 {
            if ord >= self.n {
                return;
            }
            (&self.level0[..], ord as usize, 2 * self.m)
        } else {
            let Some((nodes, rows)) = self.upper.get(level - 1) else {
                return;
            };
            let cnt = nodes.len() / 4;
            let (mut lo, mut hi) = (0usize, cnt);
            let mut found = None;
            while lo < hi {
                let mid = (lo + hi) / 2;
                let v = u32::from_le_bytes(nodes[mid * 4..mid * 4 + 4].try_into().unwrap());
                match v.cmp(&ord) {
                    Ordering::Less => lo = mid + 1,
                    Ordering::Greater => hi = mid,
                    Ordering::Equal => {
                        found = Some(mid);
                        break;
                    }
                }
            }
            let Some(i) = found else { return };
            (&rows[..], i, self.m)
        };
        let at = idx * row_len(max_conn, self.bits);
        let Some([c0, c1, slots @ ..]) = rows.get(at..) else { return };
        let count = (u16::from_le_bytes([*c0, *c1]) as usize).min(max_conn);
        for i in 0..count {
            let nb = crate::bitpack::direct::get(slots, self.bits, i as u64) as u32;
            if nb < self.n {
                f(nb);
            }
        }
    }
}

/// k-NN over a graph: returns the best `ef` accepted nodes (best first), the number of nodes
/// scored, and whether the search finished within `visit_limit`.
pub(crate) fn search_graph<G: Adjacency, S: FnMut(u32) -> f32, P: Fn(u32)>(
    g: &G,
    n: usize,
    score: &mut S,
    prefetch: &P,
    ef: usize,
    accept: Option<&dyn Fn(u32) -> bool>,
    visit_limit: usize,
) -> (Vec<Scored>, usize, bool) {
    let Some(entry) = g.entry() else {
        return (Vec::new(), 0, true);
    };
    with_visited(n, |visited| search_graph_in(g, entry, score, prefetch, ef, accept, visit_limit, visited))
}

/// Filtered k-NN for restrictive filters (Lucene's `FilteredHnswGraphSearcher`, after
/// ACORN-1): on level 0 only accepted nodes are scored and become candidates; rejected
/// neighbors serve as bridges whose own neighbors are examined (two hops), so the beam neither
/// wastes distance computations on docs it can never return nor strands in regions the filter
/// empties. `accepted` is the number of ordinals the filter accepts (`0 < accepted < m`'s graph
/// size); `m` is the graph's `m`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_graph_filtered<G: Adjacency, S: FnMut(u32) -> f32, P: Fn(u32)>(
    g: &G,
    n: usize,
    m: usize,
    score: &mut S,
    prefetch: &P,
    ef: usize,
    accept: &dyn Fn(u32) -> bool,
    accepted: usize,
    visit_limit: usize,
) -> (Vec<Scored>, usize, bool) {
    let Some(entry) = g.entry() else {
        return (Vec::new(), 0, true);
    };
    with_visited(n, |visited| {
        let mut budget = visit_limit.max(1);
        let mut scratch = Scratch::default();
        let mut ep = vec![Scored { score: score(entry), ord: entry }];
        let mut r = Vec::new();
        budget -= 1;
        for l in (1..g.num_levels()).rev() {
            visited.reset();
            if !search_layer(g, score, prefetch, &ep, 1, l, visited, None, &mut budget, &mut scratch, &mut r) {
                return (Vec::new(), visit_limit - budget, false);
            }
            if !r.is_empty() {
                std::mem::swap(&mut ep, &mut r);
            }
        }
        visited.reset();
        let complete = filtered_level0(g, n, m, score, prefetch, &ep, ef, accept, accepted, visited, &mut budget, &mut r);
        (r, visit_limit.max(1) - budget, complete)
    })
}

/// Below this share of rejected neighbors, a candidate's direct neighbors suffice (Lucene's
/// `EXPANDED_EXPLORATION_LAMBDA`).
const EXPANDED_EXPLORATION_LAMBDA: f32 = 0.10;

#[allow(clippy::too_many_arguments)]
fn filtered_level0<G: Adjacency, S: FnMut(u32) -> f32, P: Fn(u32)>(
    g: &G,
    n: usize,
    m: usize,
    score: &mut S,
    prefetch: &P,
    entry: &[Scored],
    ef: usize,
    accept: &dyn Fn(u32) -> bool,
    accepted: usize,
    visited: &mut Visited,
    budget: &mut usize,
    out: &mut Vec<Scored>,
) -> bool {
    let ratio = (accepted.max(1) as f64 / n.max(1) as f64).min(1.0);
    let max_conn = m.max(1) as f64;
    let multiplier = (1.0 / ratio).min(max_conn / 2.0).round().max(1.0) as usize;
    let min_to_score = (1.0 / ratio - 2.0 * max_conn).clamp(0.0, max_conn).round() as usize;
    let cap = 2 * m.max(1) * 2 * multiplier;
    let mut candidates: BinaryHeap<Scored> = BinaryHeap::new();
    let mut results: BinaryHeap<std::cmp::Reverse<Scored>> = BinaryHeap::new();
    let mut nbrs: Vec<u32> = Vec::new();
    let mut to_score: Vec<u32> = Vec::with_capacity(cap);
    let mut to_explore: std::collections::VecDeque<u32> = std::collections::VecDeque::with_capacity(cap);
    for &e in entry {
        if visited.insert(e.ord) {
            candidates.push(e);
            if accept(e.ord) {
                results.push(std::cmp::Reverse(e));
            }
        }
    }
    while results.len() > ef {
        results.pop();
    }
    let min_competitive = |results: &BinaryHeap<std::cmp::Reverse<Scored>>| {
        if results.len() < ef { f32::NEG_INFINITY } else { results.peek().unwrap().0.score }
    };
    let mut complete = true;
    'search: while let Some(c) = candidates.pop() {
        if c.score < min_competitive(&results) {
            break;
        }
        nbrs.clear();
        g.for_each_neighbor(0, c.ord, |nb| nbrs.push(nb));
        let neighbor_count = nbrs.len().max(1);
        to_score.clear();
        to_explore.clear();
        for &nb in &nbrs {
            if to_score.len() == cap {
                break;
            }
            if !visited.insert(nb) {
                continue;
            }
            if accept(nb) {
                to_score.push(nb);
            } else if to_explore.len() < cap {
                to_explore.push_back(nb);
            }
        }
        let filtered = to_explore.len() as f32 / neighbor_count as f32;
        let max_to_score = (neighbor_count as f32 * (multiplier as f32).min(1.0 / (1.0 - filtered))) as usize;
        let max_explore = cap - 1;
        let mut explored = to_score.len() + to_explore.len();
        if to_score.len() < max_to_score && filtered > EXPANDED_EXPLORATION_LAMBDA {
            while explored < max_explore && to_score.len() < max_to_score {
                let Some(bridge) = to_explore.pop_front() else { break };
                nbrs.clear();
                g.for_each_neighbor(0, bridge, |nb| nbrs.push(nb));
                for &nb in &nbrs {
                    if to_score.len() >= max_to_score.min(cap) {
                        break;
                    }
                    if !visited.insert(nb) {
                        continue;
                    }
                    explored += 1;
                    if accept(nb) {
                        to_score.push(nb);
                    } else if explored < max_explore && to_score.len() < min_to_score && to_explore.len() < cap {
                        to_explore.push_back(nb);
                    }
                }
            }
        }
        for &nb in &to_score {
            prefetch(nb);
        }
        for &nb in &to_score {
            if *budget == 0 {
                complete = false;
                break 'search;
            }
            *budget -= 1;
            let s = score(nb);
            if s >= min_competitive(&results) {
                candidates.push(Scored { score: s, ord: nb });
                results.push(std::cmp::Reverse(Scored { score: s, ord: nb }));
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }
    out.clear();
    out.extend(results.drain().map(|r| r.0));
    out.sort_unstable_by(|a, b| b.cmp(a));
    complete
}

#[allow(clippy::too_many_arguments)]
fn search_graph_in<G: Adjacency, S: FnMut(u32) -> f32, P: Fn(u32)>(
    g: &G,
    entry: u32,
    score: &mut S,
    prefetch: &P,
    ef: usize,
    accept: Option<&dyn Fn(u32) -> bool>,
    visit_limit: usize,
    visited: &mut Visited,
) -> (Vec<Scored>, usize, bool) {
    let mut budget = visit_limit.max(1);
    let mut scratch = Scratch::default();
    let mut ep = vec![Scored {
        score: score(entry),
        ord: entry,
    }];
    let mut r = Vec::new();
    budget -= 1;
    for l in (1..g.num_levels()).rev() {
        visited.reset();
        let complete = search_layer(g, score, prefetch, &ep, 1, l, visited, None, &mut budget, &mut scratch, &mut r);
        if !complete {
            return (Vec::new(), visit_limit - budget, false);
        }
        if !r.is_empty() {
            std::mem::swap(&mut ep, &mut r);
        }
    }
    visited.reset();
    let complete = search_layer(g, score, prefetch, &ep, ef, 0, visited, accept, &mut budget, &mut scratch, &mut r);
    (r, visit_limit.max(1) - budget, complete)
}
