//! VecSim's HNSW graph as RediSearch 8.10.1 runs it: `HNSWIndex` with its single and multi
//! value label lookups, wrapped by the tiered index FT.CREATE makes for an HNSW field.
//!
//! The graph is built the way the reference builds it, one element at a time: the level drawn
//! from `std::default_random_engine` seeded with 100, the greedy descent through the upper
//! levels, `searchLayer` with EF_CONSTRUCTION, the neighbour selection heuristic over the
//! candidates in their binary heap's array order (libstdc++'s heap and introsort, so ties land
//! where the module's do), mutual connections and the revisit of a full neighbour's list, with
//! the unidirectional (incoming) edges kept per level. Deletes follow the write mode the worker
//! pool sets: with `search-workers 0` (`VecSim_WriteInPlace`) the element is repaired out of
//! the graph and swapped with the last id at once; with workers (`VecSim_WriteAsync`) it is
//! marked deleted, its incoming neighbours repaired by jobs (run in submission order, as the
//! pool does for a quiet index), and the swap waits in the tiered index's swap jobs map until
//! the fork GC (or 1024 ready jobs) executes it, in the map's iteration order.
//!
//! Background ingestion is modelled as settled: a vector inserted through the flat buffer is
//! in the graph before the next command, inserted in arrival order. Queries search the graph
//! (KNN with EF_RUNTIME, range with EPSILON, batches), so approximate misses match.
//!
//! The allocator's view (FT.INFO `memory`) is computed from the structures: blocks, metadata,
//! lookup buckets, visited-nodes handlers, each element's upper levels and incoming edge
//! vectors with libstdc++'s growth and `shrink_to_fit`, and the tiered layer's job maps.

use super::spec::{Metric, VecType};
use super::vecsim::{Score, block_bytes, bucket_bytes, distance, hdr, hnsw_level, key, next_bkt};
use std::collections::{BTreeSet, HashMap, HashSet};

pub type Id = u32;
pub const INVALID: Id = u32::MAX;
const DELETE_MARK: u8 = 1;

/// `DEFAULT_PENDING_SWAP_JOBS_THRESHOLD`.
pub const SWAP_JOBS_THRESHOLD: usize = 1024;

// ---------------------------------------------------------------------------------------------
// libstdc++ containers

/// `std::priority_queue` over a vector: `push_heap` / `pop_heap` exactly, so the array (which
/// the module iterates) holds what the module's would.
#[derive(Clone, Debug)]
pub struct StdHeap<T> {
    pub v: Vec<T>,
    less: fn(&T, &T) -> bool,
}

impl<T: Copy> StdHeap<T> {
    pub fn new(less: fn(&T, &T) -> bool) -> Self {
        StdHeap { v: Vec::new(), less }
    }

    /// `priority_queue(first, last)`: the elements in order, then `std::make_heap`.
    pub fn from_vec(v: Vec<T>, less: fn(&T, &T) -> bool) -> Self {
        let mut h = StdHeap { v, less };
        let len = h.v.len();
        if len >= 2 {
            let mut parent = (len - 2) / 2;
            loop {
                let value = h.v[parent];
                h.adjust(parent, len, value);
                if parent == 0 {
                    break;
                }
                parent -= 1;
            }
        }
        h
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn top(&self) -> T {
        self.v[0]
    }

    pub fn push(&mut self, x: T) {
        self.v.push(x);
        let hole = self.v.len() - 1;
        self.push_up(hole, 0, x);
    }

    /// `__push_heap`.
    fn push_up(&mut self, mut hole: usize, top: usize, value: T) {
        while hole > top {
            let parent = (hole - 1) / 2;
            if !(self.less)(&self.v[parent], &value) {
                break;
            }
            self.v[hole] = self.v[parent];
            hole = parent;
        }
        self.v[hole] = value;
    }

    pub fn pop(&mut self) -> T {
        let n = self.v.len();
        let top = self.v[0];
        if n > 1 {
            let value = self.v[n - 1];
            self.v[n - 1] = self.v[0];
            self.adjust(0, n - 1, value);
        }
        self.v.pop();
        top
    }

    /// `__adjust_heap`.
    fn adjust(&mut self, mut hole: usize, len: usize, value: T) {
        let top = hole;
        let mut child = hole;
        while child < (len - 1) / 2 {
            child = 2 * (child + 1);
            if (self.less)(&self.v[child], &self.v[child - 1]) {
                child -= 1;
            }
            self.v[hole] = self.v[child];
            hole = child;
        }
        if len & 1 == 0 && child == (len - 2) / 2 {
            child = 2 * (child + 1);
            self.v[hole] = self.v[child - 1];
            hole = child - 1;
        }
        self.push_up(hole, top, value);
    }
}

/// `std::less<std::pair<DistType, T>>` (NaN distances compare as C++ compares them).
#[allow(clippy::neg_cmp_op_on_partial_ord)]
fn pair_lt<T: PartialOrd>(a: &(f64, T), b: &(f64, T)) -> bool {
    a.0 < b.0 || (!(b.0 < a.0) && a.1 < b.1)
}

fn max_id(a: &(f64, Id), b: &(f64, Id)) -> bool {
    pair_lt(a, b)
}

fn min_id(a: &(f64, Id), b: &(f64, Id)) -> bool {
    pair_lt(b, a)
}

fn max_label(a: &(f64, u64), b: &(f64, u64)) -> bool {
    pair_lt(a, b)
}

fn min_label(a: &(f64, u64), b: &(f64, u64)) -> bool {
    pair_lt(b, a)
}

/// libstdc++'s `_Prime_rehash_policy` for one hash table: the bucket count and the next resize.
#[derive(Clone, Copy, Debug)]
pub struct Buckets {
    pub n: usize,
    next_resize: usize,
}

impl Default for Buckets {
    fn default() -> Self {
        Buckets { n: 1, next_resize: 0 }
    }
}

impl Buckets {
    /// A table constructed with a bucket count hint.
    pub fn with_hint(hint: usize) -> Self {
        let mut b = Buckets::default();
        let n = b.next_bkt(hint);
        if n > b.n {
            b.n = n;
        }
        b
    }

    /// `_M_next_bkt`.
    fn next_bkt(&mut self, n: usize) -> usize {
        const FAST: [usize; 14] = [2, 2, 2, 3, 5, 5, 7, 7, 11, 11, 11, 11, 13, 13];
        if n < 14 {
            if n == 0 {
                return 1;
            }
            self.next_resize = FAST[n];
            return FAST[n];
        }
        let p = next_bkt(n);
        self.next_resize = p;
        p
    }

    /// `_M_need_rehash` before inserting one element into a table of `elems`: whether the table
    /// rehashed.
    pub fn insert(&mut self, elems: usize) -> bool {
        if elems + 1 > self.next_resize {
            let min = (elems + 1).max(if self.next_resize == 0 { 11 } else { 0 });
            if min >= self.n {
                let want = (min + 1).max(self.n * 2);
                self.n = self.next_bkt(want);
                return true;
            }
            self.next_resize = self.n;
        }
        false
    }

    /// `reserve(n)` (`rehash`) on a table of `elems`.
    pub fn reserve(&mut self, elems: usize, n: usize) -> bool {
        let saved = self.next_resize;
        let b = self.next_bkt((elems + 1).max(n));
        if b != self.n {
            self.n = b;
            true
        } else {
            self.next_resize = saved;
            false
        }
    }

    /// The bucket array's allocation.
    pub fn bytes(&self) -> usize {
        bucket_bytes(self.n)
    }
}

/// An `unordered_map` keyed by an integer (identity hash) laid out as libstdc++'s
/// `_Hashtable`: one singly linked list of nodes, each bucket pointing at the node before its
/// first one (or the list's head); a new node goes to the front of its bucket, or to the front
/// of the list when its bucket was empty; a rehash re-links the nodes in list order. Iteration
/// follows the list.
#[derive(Clone, Debug)]
pub struct UMap<V> {
    nodes: Vec<Option<(u64, V, Option<usize>)>>,
    free: Vec<usize>,
    head: Option<usize>,
    /// Per bucket: `None` empty, `Some(None)` the list's head precedes it, `Some(Some(n))` node
    /// `n` does.
    buckets: Vec<Option<Option<usize>>>,
    keys: HashMap<u64, usize>,
    pub b: Buckets,
}

impl<V> Default for UMap<V> {
    fn default() -> Self {
        UMap::with_hint(0)
    }
}

impl<V> UMap<V> {
    pub fn with_hint(hint: usize) -> Self {
        let b = Buckets::with_hint(hint);
        UMap { nodes: Vec::new(), free: Vec::new(), head: None, buckets: vec![None; b.n], keys: HashMap::new(), b }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn bucket(&self, k: u64) -> usize {
        (k % self.b.n as u64) as usize
    }

    fn next_of(&self, before: Option<usize>) -> Option<usize> {
        match before {
            None => self.head,
            Some(n) => self.nodes[n].as_ref().expect("live node").2,
        }
    }

    fn set_next(&mut self, before: Option<usize>, next: Option<usize>) {
        match before {
            None => self.head = next,
            Some(n) => self.nodes[n].as_mut().expect("live node").2 = next,
        }
    }

    fn key_of(&self, n: usize) -> u64 {
        self.nodes[n].as_ref().expect("live node").0
    }

    /// `_M_insert_bucket_begin`.
    fn link(&mut self, n: usize) {
        let bkt = self.bucket(self.key_of(n));
        match self.buckets[bkt] {
            Some(before) => {
                let next = self.next_of(before);
                self.nodes[n].as_mut().expect("live node").2 = next;
                self.set_next(before, Some(n));
            }
            None => {
                let next = self.head;
                self.nodes[n].as_mut().expect("live node").2 = next;
                self.head = Some(n);
                if let Some(nx) = next {
                    let nb = self.bucket(self.key_of(nx));
                    self.buckets[nb] = Some(Some(n));
                }
                self.buckets[bkt] = Some(None);
            }
        }
    }

    pub fn get(&self, k: u64) -> Option<&V> {
        self.keys.get(&k).map(|&n| &self.nodes[n].as_ref().expect("live node").1)
    }

    pub fn get_mut(&mut self, k: u64) -> Option<&mut V> {
        let n = *self.keys.get(&k)?;
        Some(&mut self.nodes[n].as_mut().expect("live node").1)
    }

    /// `insert` / `emplace` of a new key (false, unchanged, when it exists).
    pub fn insert(&mut self, k: u64, v: V) -> bool {
        if self.keys.contains_key(&k) {
            return false;
        }
        if self.b.insert(self.keys.len()) {
            // `_M_rehash_aux`: the nodes re-linked in list order
            let mut order = Vec::with_capacity(self.keys.len());
            let mut cur = self.head;
            while let Some(n) = cur {
                order.push(n);
                cur = self.nodes[n].as_ref().expect("live node").2;
            }
            self.head = None;
            self.buckets = vec![None; self.b.n];
            for n in order {
                self.link(n);
            }
        }
        let n = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some((k, v, None));
                i
            }
            None => {
                self.nodes.push(Some((k, v, None)));
                self.nodes.len() - 1
            }
        };
        self.keys.insert(k, n);
        self.link(n);
        true
    }

    /// `map[k] = v`.
    pub fn set(&mut self, k: u64, v: V) {
        match self.get_mut(k) {
            Some(x) => *x = v,
            None => {
                self.insert(k, v);
            }
        }
    }

    /// `erase(k)` (`_M_erase` with `_M_remove_bucket_begin`).
    pub fn erase(&mut self, k: u64) -> Option<V> {
        let n = self.keys.remove(&k)?;
        let bkt = self.bucket(k);
        let first_before = self.buckets[bkt].expect("bucket of a live key");
        let mut prev = first_before;
        while self.next_of(prev) != Some(n) {
            prev = self.next_of(prev).map(Some).expect("node in its bucket");
        }
        let next = self.nodes[n].as_ref().expect("live node").2;
        let next_bkt = next.map(|x| self.bucket(self.key_of(x)));
        if prev == first_before {
            if next.is_none() || next_bkt != Some(bkt) {
                if let Some(nb) = next_bkt {
                    self.buckets[nb] = self.buckets[bkt];
                }
                self.buckets[bkt] = None;
            }
        } else if let Some(nb) = next_bkt
            && nb != bkt
        {
            self.buckets[nb] = Some(prev);
        }
        self.set_next(prev, next);
        let (_, v, _) = self.nodes[n].take().expect("live node");
        self.free.push(n);
        Some(v)
    }

    /// The nodes in iteration order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &V)> {
        let mut cur = self.head;
        std::iter::from_fn(move || {
            let n = cur?;
            let (k, v, next) = self.nodes[n].as_ref().expect("live node");
            cur = *next;
            Some((*k, v))
        })
    }

    /// The keys in iteration order.
    pub fn keys_in_order(&self) -> Vec<u64> {
        self.iter().map(|(k, _)| k).collect()
    }
}

/// A vector's capacity as libstdc++ grows it (`push_back` doubles, `shrink_to_fit` trims).
fn grown(cap: usize, len: usize) -> usize {
    if len < cap { cap } else if cap == 0 { 1 } else { 2 * cap }
}

/// The label results heap: `max_priority_queue` (single value) or `updatable_max_heap` (multi:
/// a label's best score kept).
#[derive(Clone, Debug)]
enum LabelHeap {
    Single(StdHeap<(f64, u64)>),
    Multi { set: BTreeSet<(Score, u64)>, at: HashMap<u64, f64> },
}

impl LabelHeap {
    fn new(multi: bool) -> Self {
        if multi { LabelHeap::Multi { set: BTreeSet::new(), at: HashMap::new() } } else { LabelHeap::Single(StdHeap::new(max_label)) }
    }

    fn emplace(&mut self, d: f64, label: u64) {
        match self {
            LabelHeap::Single(h) => h.push((d, label)),
            LabelHeap::Multi { set, at } => match at.get(&label).copied() {
                None => {
                    set.insert((key(d), label));
                    at.insert(label, d);
                }
                Some(old) if old > d => {
                    set.remove(&(key(old), label));
                    set.insert((key(d), label));
                    at.insert(label, d);
                }
                Some(_) => {}
            },
        }
    }

    fn len(&self) -> usize {
        match self {
            LabelHeap::Single(h) => h.len(),
            LabelHeap::Multi { at, .. } => at.len(),
        }
    }

    fn top(&self) -> (f64, u64) {
        match self {
            LabelHeap::Single(h) => h.top(),
            LabelHeap::Multi { set, at } => {
                let &(_, l) = set.iter().next_back().expect("non-empty heap");
                (at[&l], l)
            }
        }
    }

    fn pop(&mut self) -> (f64, u64) {
        match self {
            LabelHeap::Single(h) => h.pop(),
            LabelHeap::Multi { set, at } => {
                let (s, l) = set.pop_last().expect("non-empty heap");
                at.remove(&l);
                let _ = s;
                (0.0, l)
            }
        }
    }

    /// Every result, best first (the heap popped into a reply backwards).
    fn into_sorted(mut self) -> Vec<(u64, f64)> {
        let mut out = Vec::with_capacity(self.len());
        while self.len() > 0 {
            let (d, l) = self.top();
            self.pop();
            out.push((l, d));
        }
        out.reverse();
        out
    }
}

/// The candidates sets a graph scan fills.
trait Cands {
    fn put(&mut self, d: f64, id: Id, label: u64);
    fn size(&self) -> usize;
    fn drop_top(&mut self);
    fn top_d(&self) -> f64;
}

impl Cands for StdHeap<(f64, Id)> {
    fn put(&mut self, d: f64, id: Id, _label: u64) {
        self.push((d, id));
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn drop_top(&mut self) {
        self.pop();
    }
    fn top_d(&self) -> f64 {
        self.top().0
    }
}

impl Cands for LabelHeap {
    fn put(&mut self, d: f64, _id: Id, label: u64) {
        self.emplace(d, label);
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn drop_top(&mut self) {
        self.pop();
    }
    fn top_d(&self) -> f64 {
        self.top().0
    }
}

// ---------------------------------------------------------------------------------------------
// The graph

/// An element's links at one level, and the nodes pointing at it without a link back.
#[derive(Clone, Debug, Default)]
struct Level {
    links: Vec<Id>,
    incoming: Vec<Id>,
    in_cap: usize,
}

impl Level {
    fn push_incoming(&mut self, id: Id) {
        self.in_cap = grown(self.in_cap, self.incoming.len());
        self.incoming.push(id);
    }

    /// `removeIncomingUnidirectionalEdgeIfExists`: swapped with the last, shrunk past half.
    fn remove_incoming(&mut self, id: Id) -> bool {
        let Some(p) = self.incoming.iter().position(|&x| x == id) else { return false };
        let last = *self.incoming.last().expect("found");
        self.incoming[p] = last;
        self.incoming.pop();
        if self.in_cap > 2 * self.incoming.len() {
            self.in_cap = self.incoming.len();
        }
        true
    }

    fn swap_incoming(&mut self, before: Id, after: Id) {
        if let Some(p) = self.incoming.iter().position(|&x| x == before) {
            self.incoming[p] = after;
        }
    }

    /// `removeLink`: the last link takes its place.
    fn remove_link(&mut self, id: Id) {
        if let Some(i) = self.links.iter().position(|&x| x == id) {
            let last = *self.links.last().expect("found");
            self.links[i] = last;
            self.links.pop();
        }
    }
}

#[derive(Clone, Debug)]
struct Elem {
    top: usize,
    levels: Vec<Level>,
    label: u64,
    flags: u8,
}

/// A pending swap job (`HNSWSwapJob`).
#[derive(Clone, Copy, Debug)]
struct SwapJob {
    deleted_id: Id,
    pending: usize,
}

/// A pending repair job (`HNSWRepairJob`).
#[derive(Clone, Debug)]
struct RepairJob {
    node: Id,
    level: usize,
    swaps: Vec<usize>,
}

/// Whether writes go through the flat buffer and the workers (`VecSim_WriteAsync`) or straight
/// into the graph (`VecSim_WriteInPlace`, no worker threads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Async,
    InPlace,
}

/// A tiered HNSW index (the graph and the tiered layer's state).
#[derive(Clone, Debug)]
pub struct Hnsw {
    ty: VecType,
    metric: Metric,
    dim: usize,
    stored: usize,
    multi: bool,
    f32d: bool,
    pub m: usize,
    m0: usize,
    pub ef_c: usize,
    pub ef: usize,
    pub eps: f64,
    pub block: usize,
    rng: u64,
    data: Vec<u8>,
    elems: Vec<Elem>,
    lookup: HashMap<u64, (Vec<Id>, usize)>,
    lookup_b: Buckets,
    entry: Id,
    max_level: isize,
    pub marked: usize,
    max_elements: usize,
    meta_cap: usize,
    graph_blocks: usize,
    graph_blocks_cap: usize,
    data_blocks: usize,
    data_blocks_cap: usize,
    visited_n: usize,
    visited_handlers: std::cell::Cell<usize>,
    // the tiered layer
    swap_jobs: UMap<usize>,
    jobs: Vec<SwapJob>,
    repairs: UMap<Vec<usize>>,
    pending_repairs: Vec<RepairJob>,
    queue: Vec<usize>,
    ready_swaps: usize,
    insert_jobs_b: Option<Buckets>,
    pub flat_used: bool,
    pub direct: usize,
}

impl Hnsw {
    #[allow(clippy::too_many_arguments)]
    pub fn new(ty: VecType, metric: Metric, dim: usize, stored: usize, multi: bool, m: usize, ef_c: usize, ef: usize, eps: f64, block: usize) -> Self {
        let m = if m == 0 { 16 } else { m };
        let ef_c = if ef_c == 0 { 200 } else { ef_c }.max(m);
        Hnsw {
            ty,
            metric,
            dim,
            stored,
            multi,
            f32d: ty != VecType::Float64,
            m,
            m0: 2 * m,
            ef_c,
            ef: if ef == 0 { 10 } else { ef },
            eps: if eps > 0.0 { eps } else { 0.01 },
            block: block.max(1),
            rng: 100,
            data: Vec::new(),
            elems: Vec::new(),
            lookup: HashMap::new(),
            lookup_b: Buckets::default(),
            entry: INVALID,
            max_level: -1,
            marked: 0,
            max_elements: 0,
            meta_cap: 0,
            graph_blocks: 0,
            graph_blocks_cap: 0,
            data_blocks: 0,
            data_blocks_cap: 0,
            visited_n: 0,
            visited_handlers: std::cell::Cell::new(0),
            swap_jobs: UMap::default(),
            jobs: Vec::new(),
            repairs: UMap::default(),
            pending_repairs: Vec::new(),
            queue: Vec::new(),
            ready_swaps: 0,
            insert_jobs_b: None,
            flat_used: false,
            direct: 0,
        }
    }

    /// `indexSize`: every element, marked deleted ones too.
    pub fn size(&self) -> usize {
        self.elems.len()
    }

    /// `indexLabelCount`.
    pub fn label_count(&self) -> usize {
        self.lookup.len()
    }

    /// `maxLevel` and the entry point's label (None while the graph is empty).
    pub fn entry_point(&self) -> Option<(usize, u64)> {
        (self.entry != INVALID).then(|| (self.max_level as usize, self.elems[self.entry as usize].label))
    }

    fn vec(&self, id: Id) -> &[u8] {
        &self.data[id as usize * self.stored..(id as usize + 1) * self.stored]
    }

    fn calc(&self, a: &[u8], b: &[u8]) -> f64 {
        distance(self.ty, self.metric, self.dim, a, b)
    }

    fn dist(&self, q: &[u8], id: Id) -> f64 {
        self.calc(q, self.vec(id))
    }

    /// `std::numeric_limits<DistType>::max()`.
    fn dmax(&self) -> f64 {
        if self.f32d { f32::MAX as f64 } else { f64::MAX }
    }

    /// A double narrowed to `DistType`.
    fn nd(&self, x: f64) -> f64 {
        if self.f32d { x as f32 as f64 } else { x }
    }

    fn deleted(&self, id: Id) -> bool {
        self.elems[id as usize].flags & DELETE_MARK != 0
    }

    fn lvl(&self, id: Id, level: usize) -> &Level {
        &self.elems[id as usize].levels[level]
    }

    fn lvl_mut(&mut self, id: Id, level: usize) -> &mut Level {
        &mut self.elems[id as usize].levels[level]
    }

    fn label(&self, id: Id) -> u64 {
        self.elems[id as usize].label
    }

    /// A graph scan took a visited-nodes handler from the pool.
    fn use_visited(&self) {
        if self.visited_handlers.get() == 0 {
            self.visited_handlers.set(1);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Searching

    /// `processCandidate`.
    #[allow(clippy::too_many_arguments)]
    fn process_candidate(&self, cur: Id, q: &[u8], layer: usize, ef: usize, visited: &mut [bool], top: &mut impl Cands, cands: &mut StdHeap<(f64, Id)>, lower: &mut f64) {
        for &c in &self.lvl(cur, layer).links {
            if visited[c as usize] {
                continue;
            }
            visited[c as usize] = true;
            let d = self.dist(q, c);
            if *lower > d || top.size() < ef {
                cands.push((-d, c));
                if !self.deleted(c) {
                    top.put(d, c, self.label(c));
                }
                if top.size() > ef {
                    top.drop_top();
                }
                if top.size() > 0 {
                    *lower = top.top_d();
                }
            }
        }
    }

    /// `searchLayer`: the EF closest candidates found from `ep` at `layer`, as a heap.
    fn search_layer(&self, ep: Id, q: &[u8], layer: usize, ef: usize) -> StdHeap<(f64, Id)> {
        let mut visited = vec![false; self.elems.len()];
        let mut top = StdHeap::new(max_id);
        let mut cands = StdHeap::new(max_id);
        let mut lower;
        if !self.deleted(ep) {
            let d = self.dist(q, ep);
            lower = d;
            top.push((d, ep));
            cands.push((-d, ep));
        } else {
            lower = self.dmax();
            cands.push((-lower, ep));
        }
        visited[ep as usize] = true;
        while !cands.is_empty() {
            let (nd, cur) = cands.top();
            if -nd > lower && top.len() >= ef {
                break;
            }
            cands.pop();
            self.process_candidate(cur, q, layer, ef, &mut visited, &mut top, &mut cands, &mut lower);
        }
        top
    }

    /// `greedySearchLevel`.
    fn greedy(&self, q: &[u8], level: usize, best: &mut Id, cur_dist: &mut f64, query: bool) {
        let mut best_nd = *best;
        loop {
            let mut changed = false;
            let locked = *best;
            for &c in &self.lvl(locked, level).links {
                let d = self.dist(q, c);
                if d < *cur_dist {
                    *cur_dist = d;
                    *best = c;
                    changed = true;
                    if !query && !self.deleted(c) {
                        best_nd = *best;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        if !query {
            *best = best_nd;
        }
    }

    /// `searchBottomLayerEP`.
    fn bottom_ep(&self, q: &[u8]) -> Id {
        let mut cur = self.entry;
        if cur == INVALID {
            return cur;
        }
        let mut d = self.dist(q, cur);
        let mut level = self.max_level as usize;
        while level > 0 && cur != INVALID {
            self.greedy(q, level, &mut cur, &mut d, true);
            level -= 1;
        }
        cur
    }

    /// `topKQuery` on a preprocessed query: (label, distance) best first.
    pub fn top_k(&self, q: &[u8], k: usize, ef_rt: usize) -> Vec<(u64, f64)> {
        if self.elems.is_empty() || k == 0 {
            return Vec::new();
        }
        let ef = if ef_rt != 0 { ef_rt } else { self.ef };
        let ep = self.bottom_ep(q);
        if ep == INVALID {
            return Vec::new();
        }
        self.use_visited();
        let ef = ef.max(k);
        let mut visited = vec![false; self.elems.len()];
        let mut top = LabelHeap::new(self.multi);
        let mut cands = StdHeap::new(max_id);
        let mut lower;
        if !self.deleted(ep) {
            let d = self.dist(q, ep);
            lower = d;
            top.emplace(d, self.label(ep));
            cands.push((-d, ep));
        } else {
            lower = self.dmax();
            cands.push((-lower, ep));
        }
        visited[ep as usize] = true;
        while !cands.is_empty() {
            let (nd, cur) = cands.top();
            if -nd > lower && top.len() >= ef {
                break;
            }
            cands.pop();
            self.process_candidate(cur, q, 0, ef, &mut visited, &mut top, &mut cands, &mut lower);
        }
        while top.len() > k {
            top.pop();
        }
        top.into_sorted()
    }

    /// `rangeQuery` on a preprocessed query: (label, distance) in the results container's
    /// order (found order; a multi-value index's `unordered_map`).
    pub fn range(&self, q: &[u8], radius: f64, eps_rt: f64) -> Vec<(u64, f64)> {
        if self.elems.is_empty() {
            return Vec::new();
        }
        let eps = if eps_rt != 0.0 { eps_rt } else { self.eps };
        let ep = self.bottom_ep(q);
        if ep == INVALID {
            return Vec::new();
        }
        self.use_visited();
        let radius = self.nd(radius);
        let mut single: Vec<(u64, f64)> = Vec::new();
        let mut multi: UMap<f64> = UMap::with_hint(10);
        let multi_v = self.multi;
        let mut emplace = |label: u64, d: f64| {
            if multi_v {
                match multi.get_mut(label) {
                    None => {
                        multi.insert(label, d);
                    }
                    Some(s) => {
                        if *s > d {
                            *s = d;
                        }
                    }
                }
            } else {
                single.push((label, d));
            }
        };
        let mut visited = vec![false; self.elems.len()];
        let mut cands = StdHeap::new(max_id);
        let (ep_dist, mut dyn_range, mut bounds);
        if self.deleted(ep) {
            ep_dist = self.dmax();
            dyn_range = ep_dist;
            bounds = ep_dist;
        } else {
            ep_dist = self.dist(q, ep);
            dyn_range = ep_dist;
            if ep_dist <= radius {
                emplace(self.label(ep), ep_dist);
                dyn_range = radius;
            }
            bounds = self.nd(dyn_range * (1.0 + eps));
        }
        cands.push((-ep_dist, ep));
        visited[ep as usize] = true;
        while !cands.is_empty() {
            let (nd, cur) = cands.top();
            if -nd > bounds {
                break;
            }
            cands.pop();
            if -nd < dyn_range && -nd >= radius {
                dyn_range = -nd;
                bounds = self.nd(dyn_range * (1.0 + eps));
            }
            for &c in &self.lvl(cur, 0).links {
                if visited[c as usize] {
                    continue;
                }
                visited[c as usize] = true;
                let d = self.dist(q, c);
                if d < bounds {
                    cands.push((-d, c));
                    if d <= radius && !self.deleted(c) {
                        emplace(self.label(c), d);
                    }
                }
            }
        }
        if multi_v { multi.iter().map(|(l, &d)| (l, d)).collect() } else { single }
    }

    /// `getDistanceFrom_Unsafe`: a label's (closest) distance, NaN without one.
    pub fn distance_from(&self, label: u64, q: &[u8]) -> f64 {
        let Some((ids, _)) = self.lookup.get(&label) else { return f64::NAN };
        let mut dist = f64::NAN;
        for &id in ids {
            let d = self.calc(self.vec(id), q);
            dist = dist.min(d);
        }
        dist
    }

    // -----------------------------------------------------------------------------------------
    // Building

    /// `getNeighborsByHeuristic2`: the next entry point.
    fn heuristic(&self, list: &mut Vec<(f64, Id)>, m: usize) -> Id {
        if list.len() < m {
            let mut best = 0;
            for i in 1..list.len() {
                if list[i].0 < list[best].0 {
                    best = i;
                }
            }
            return list[best].1;
        }
        self.heuristic_internal(list, m, None);
        list[0].1
    }

    /// `getNeighborsByHeuristic2_internal`: the candidates sorted by distance, each kept unless
    /// a kept one is closer to it than the query is; the rest recorded as removed.
    fn heuristic_internal(&self, list: &mut Vec<(f64, Id)>, m: usize, mut removed: Option<&mut Vec<Id>>) {
        if list.len() < m {
            return;
        }
        super::vec_iters::std_sort(list, |a, b| a.0 < b.0);
        let mut ret: Vec<(f64, Id)> = Vec::with_capacity(m);
        let mut i = 0;
        while i < list.len() && ret.len() < m {
            let (cd, cid) = list[i];
            let cv = self.vec(cid);
            let mut good = true;
            for &(_, sel) in &ret {
                if self.calc(self.vec(sel), cv) < cd {
                    if let Some(r) = removed.as_deref_mut() {
                        r.push(cid);
                    }
                    good = false;
                    break;
                }
            }
            if good {
                ret.push(list[i]);
            }
            i += 1;
        }
        if let Some(r) = removed {
            for &(_, id) in &list[i..] {
                r.push(id);
            }
        }
        *list = ret;
    }

    /// `mutuallyRemoveNeighborAtPos`: `node` drops its link to `removed`.
    fn mutually_remove(&mut self, node: Id, level: usize, removed: Id) {
        if !self.lvl_mut(removed, level).remove_incoming(node) {
            self.lvl_mut(node, level).push_incoming(removed);
        }
    }

    /// `revisitNeighborConnections`: a full neighbour re-selects its links with the new node.
    fn revisit(&mut self, level: usize, new: Id, (d, nb): (f64, Id)) {
        let nb_links = self.lvl(nb, level).links.clone();
        let mut cands: Vec<(f64, Id)> = Vec::with_capacity(nb_links.len() + 1);
        cands.push((d, new));
        let nv = self.vec(nb).to_vec();
        for &l in &nb_links {
            cands.push((self.calc(self.vec(l), &nv), l));
        }
        let max_m = if level > 0 { self.m } else { self.m0 };
        let mut update: Vec<Id> = Vec::new();
        self.heuristic_internal(&mut cands, max_m, Some(&mut update));
        let chosen = if update.contains(&new) {
            false
        } else {
            update.push(new);
            true
        };
        update.push(nb);
        update.sort_unstable();
        let mut out: Vec<Id> = Vec::with_capacity(nb_links.len());
        let mut update_cur = true;
        for &l in &nb_links {
            if update.binary_search(&l).is_err() {
                out.push(l);
                continue;
            }
            if l == new {
                out.push(l);
                update_cur = false;
                continue;
            }
            self.mutually_remove(nb, level, l);
        }
        if update_cur && self.lvl(new, level).links.len() < max_m && !self.deleted(new) && !self.deleted(nb) {
            self.lvl_mut(new, level).links.push(nb);
            if chosen && out.len() < max_m {
                out.push(new);
            } else {
                self.lvl_mut(nb, level).push_incoming(new);
            }
        }
        self.lvl_mut(nb, level).links = out;
    }

    /// `mutuallyConnectNewElement`: the next closest entry point.
    fn connect_new(&mut self, new: Id, top: StdHeap<(f64, Id)>, level: usize) -> Id {
        let max_m = if level > 0 { self.m } else { self.m0 };
        let mut list = top.v;
        let next = self.heuristic(&mut list, self.m);
        for &(d, nb) in &list {
            if self.lvl(new, level).links.len() == max_m {
                break;
            }
            if self.deleted(new) || self.deleted(nb) {
                continue;
            }
            if self.lvl(nb, level).links.len() < max_m {
                self.lvl_mut(new, level).links.push(nb);
                self.lvl_mut(nb, level).links.push(new);
                continue;
            }
            self.revisit(level, new, (d, nb));
        }
        next
    }

    /// `insertElementToGraph`.
    fn insert_to_graph(&mut self, id: Id, level: usize, ep: Id, global_max: usize, q: &[u8]) {
        let mut cur = ep;
        let mut cur_dist;
        let max_common;
        if level < global_max {
            max_common = level;
            cur_dist = self.dist(q, cur);
            let mut l = global_max;
            while l > level {
                self.greedy(q, l, &mut cur, &mut cur_dist, false);
                l -= 1;
            }
        } else {
            max_common = global_max;
        }
        for l in (0..=max_common).rev() {
            self.use_visited();
            let top = self.search_layer(cur, q, l, self.ef_c);
            if !top.is_empty() {
                cur = self.connect_new(id, top, l);
            }
        }
    }

    /// `resizeIndexCommon`.
    fn resize_common(&mut self, n: usize) {
        let labels = self.lookup.len();
        self.lookup_b.reserve(labels, n);
        self.visited_n = n;
        self.meta_cap = n;
    }

    /// `growByBlock`.
    fn grow_by_block(&mut self) {
        self.max_elements += self.block;
        self.graph_blocks_cap = grown(self.graph_blocks_cap, self.graph_blocks);
        self.graph_blocks += 1;
        if self.meta_cap == self.elems.len() {
            self.resize_common(self.max_elements);
        }
    }

    /// `shrinkByBlock` (after an element left).
    fn shrink_by_block(&mut self) {
        let n = self.elems.len();
        if !n.is_multiple_of(self.block) {
            return;
        }
        self.graph_blocks -= 1;
        if n == 0 {
            self.resize_common(0);
        } else if self.meta_cap >= n + 2 * self.block {
            self.resize_common(self.meta_cap - self.block);
        }
        self.max_elements -= self.block;
    }

    /// `storeNewElement` and `indexVector`: the element stored, then connected.
    fn append(&mut self, v: &[u8], label: u64) {
        if self.elems.len() == self.max_elements {
            self.grow_by_block();
        }
        let level = hnsw_level(&mut self.rng, self.m);
        let id = self.elems.len() as Id;
        if self.elems.len().is_multiple_of(self.block) {
            self.data_blocks_cap = grown(self.data_blocks_cap, self.data_blocks);
            self.data_blocks += 1;
        }
        self.data.extend_from_slice(v);
        self.elems.push(Elem { top: level, levels: vec![Level::default(); level + 1], label, flags: 0 });
        // `setVectorId`
        let n = self.lookup.len();
        match self.lookup.get_mut(&label) {
            Some((ids, cap)) => {
                *cap = grown(*cap, ids.len());
                ids.push(id);
            }
            None => {
                self.lookup_b.insert(n);
                self.lookup.insert(label, (vec![id], 1));
            }
        }
        let prev_ep = self.entry;
        let prev_max = self.max_level;
        if level as isize > self.max_level {
            self.entry = id;
            self.max_level = level as isize;
        }
        if prev_ep != INVALID {
            self.insert_to_graph(id, level, prev_ep, prev_max as usize, v);
        }
    }

    /// `replaceEntryPoint` (the old entry point is being deleted).
    fn replace_entry_point(&mut self) {
        let old = self.entry;
        while old == self.entry {
            let level = self.max_level as usize;
            for &l in &self.lvl(old, level).links {
                if !self.deleted(l) {
                    self.entry = l;
                    return;
                }
            }
            for cur in 0..self.elems.len() as Id {
                if self.elems[cur as usize].top == level && cur != old && !self.deleted(cur) {
                    self.entry = cur;
                    return;
                }
            }
            self.max_level -= 1;
            if self.max_level < 0 {
                self.max_level = -1;
                self.entry = INVALID;
            }
        }
    }

    /// `markDeletedInternal`.
    fn mark_deleted(&mut self, id: Id) {
        if !self.deleted(id) {
            if id == self.entry {
                self.replace_entry_point();
            }
            self.elems[id as usize].flags |= DELETE_MARK;
            self.marked += 1;
        }
    }

    /// `repairConnectionsForDeletion`: `nb` (pointing at the deleted `elem`) re-selects its links
    /// from its own and the deleted element's.
    fn repair_for_deletion(&mut self, elem: Id, nb: Id, level: usize, bitmap: &HashSet<Id>) {
        if self.deleted(nb) {
            self.lvl_mut(nb, level).remove_link(elem);
            return;
        }
        let nb_links = self.lvl(nb, level).links.clone();
        let orig: HashSet<Id> = nb_links.iter().copied().collect();
        let mut cand_ids: Vec<Id> = Vec::new();
        for &c in &nb_links {
            if c != elem && !bitmap.contains(&c) {
                cand_ids.push(c);
            }
        }
        for &c in &self.lvl(elem, level).links {
            if c != nb && (!self.deleted(c) || orig.contains(&c)) {
                cand_ids.push(c);
            }
        }
        let max_m = if level > 0 { self.m } else { self.m0 };
        if cand_ids.len() > max_m {
            let nv = self.vec(nb).to_vec();
            let mut cands: Vec<(f64, Id)> = cand_ids.iter().map(|&c| (self.calc(self.vec(c), &nv), c)).collect();
            let mut not_chosen = Vec::new();
            self.heuristic_internal(&mut cands, max_m, Some(&mut not_chosen));
            self.lvl_mut(nb, level).links = cands.iter().map(|x| x.1).collect();
            for n in not_chosen {
                if orig.contains(&n) && !self.lvl_mut(n, level).remove_incoming(nb) {
                    self.lvl_mut(nb, level).push_incoming(n);
                }
            }
        } else {
            self.lvl_mut(nb, level).links = cand_ids;
        }
        let new_links = self.lvl(nb, level).links.clone();
        for n in new_links {
            if orig.contains(&n) {
                continue;
            }
            if self.lvl(n, level).links.contains(&nb) {
                self.lvl_mut(nb, level).remove_incoming(n);
            } else {
                self.lvl_mut(n, level).push_incoming(nb);
            }
        }
    }

    /// `removeVectorInPlace`.
    fn remove_in_place(&mut self, id: Id) {
        for level in 0..=self.elems[id as usize].top {
            let links = self.lvl(id, level).links.clone();
            let bitmap: HashSet<Id> = links.iter().copied().collect();
            for &nb in &links {
                if self.lvl(nb, level).links.contains(&id) {
                    self.repair_for_deletion(id, nb, level, &bitmap);
                } else {
                    self.lvl_mut(nb, level).remove_incoming(id);
                }
            }
            let incoming = self.lvl(id, level).incoming.clone();
            for inc in incoming {
                self.repair_for_deletion(id, inc, level, &bitmap);
            }
        }
        if self.entry == id {
            self.replace_entry_point();
        }
        self.remove_and_swap(id);
    }

    /// `removeAndSwap`: the last element moves into the hole.
    fn remove_and_swap(&mut self, id: Id) {
        for level in 0..=self.elems[id as usize].top {
            let links = self.lvl(id, level).links.clone();
            for l in links {
                self.lvl_mut(l, level).remove_incoming(id);
            }
        }
        let last = self.elems.len() as Id - 1;
        if last != id {
            self.swap_last(id, last);
        }
        self.elems.pop();
        self.data.truncate(self.elems.len() * self.stored);
        if self.elems.len().is_multiple_of(self.block) {
            self.data_blocks -= 1;
        }
        self.shrink_by_block();
    }

    /// `SwapLastIdWithDeletedId`.
    fn swap_last(&mut self, id: Id, last: Id) {
        if !self.deleted(last) {
            let label = self.label(last);
            if let Some((ids, _)) = self.lookup.get_mut(&label)
                && let Some(p) = ids.iter().rposition(|&x| x == last)
            {
                ids[p] = id;
            }
        }
        for level in 0..=self.elems[last as usize].top {
            let links = self.lvl(last, level).links.clone();
            for nb in links {
                let lv = self.lvl_mut(nb, level);
                match lv.links.iter().position(|&x| x == last) {
                    Some(j) => lv.links[j] = id,
                    None => lv.swap_incoming(last, id),
                }
            }
            let incoming = self.lvl(last, level).incoming.clone();
            for inc in incoming {
                let lv = self.lvl_mut(inc, level);
                if let Some(j) = lv.links.iter().position(|&x| x == last) {
                    lv.links[j] = id;
                }
            }
        }
        self.elems[id as usize] = self.elems[last as usize].clone();
        let s = self.stored;
        self.data.copy_within(last as usize * s..(last as usize + 1) * s, id as usize * s);
        if self.entry == last {
            self.entry = id;
        }
    }

    /// `safeCollectAllNodeIncomingNeighbors`.
    fn incoming_of(&self, id: Id) -> Vec<(Id, usize)> {
        let mut out = Vec::new();
        for level in 0..=self.elems[id as usize].top {
            for &nb in &self.lvl(id, level).links {
                if self.lvl(nb, level).links.contains(&id) {
                    out.push((nb, level));
                }
            }
            for &inc in &self.lvl(id, level).incoming {
                out.push((inc, level));
            }
        }
        out
    }

    /// `repairNodeConnections`.
    fn repair_node(&mut self, node: Id, level: usize) {
        let links = self.lvl(node, level).links.clone();
        let orig: HashSet<Id> = links.iter().copied().collect();
        let mut cand_set: HashSet<Id> = HashSet::new();
        let mut cand_ids: Vec<Id> = Vec::new();
        let mut deleted_nbs: Vec<Id> = Vec::new();
        for &l in &links {
            if self.deleted(l) {
                deleted_nbs.push(l);
                continue;
            }
            cand_set.insert(l);
            cand_ids.push(l);
        }
        if deleted_nbs.is_empty() {
            return;
        }
        let mut update: Vec<Id> = Vec::new();
        let mut chosen: Vec<Id> = Vec::new();
        for &dn in &deleted_nbs {
            update.push(dn);
            for &l in &self.lvl(dn, level).links {
                if self.deleted(l) || cand_set.contains(&l) || l == node {
                    continue;
                }
                cand_set.insert(l);
                cand_ids.push(l);
            }
        }
        let max_m = if level > 0 { self.m } else { self.m0 };
        if cand_ids.len() > max_m {
            let nv = self.vec(node).to_vec();
            let mut cands: Vec<(f64, Id)> = cand_ids.iter().map(|&c| (self.calc(self.vec(c), &nv), c)).collect();
            let mut not_chosen = Vec::new();
            self.heuristic_internal(&mut cands, max_m, Some(&mut not_chosen));
            for n in not_chosen {
                if orig.contains(&n) {
                    update.push(n);
                }
            }
            for &(_, c) in &cands {
                chosen.push(c);
                update.push(c);
            }
        } else {
            chosen = cand_ids;
            update.extend_from_slice(&chosen);
        }
        self.update_repaired(node, level, update, chosen, max_m);
    }

    /// `mutuallyUpdateForRepairedNode`.
    fn update_repaired(&mut self, node: Id, level: usize, mut update: Vec<Id>, mut chosen: Vec<Id>, max_m: usize) {
        update.push(node);
        update.sort_unstable();
        let links = self.lvl(node, level).links.clone();
        let mut out: Vec<Id> = Vec::new();
        for &l in &links {
            if update.binary_search(&l).is_err() {
                out.push(l);
                continue;
            }
            // `vecsim_stl::vector::remove`: the last takes its place
            if let Some(p) = chosen.iter().position(|&x| x == l) {
                let last = *chosen.last().expect("found");
                chosen[p] = last;
                chosen.pop();
                out.push(l);
                continue;
            }
            self.mutually_remove(node, level, l);
        }
        for c in chosen {
            if out.len() == max_m {
                break;
            }
            if self.deleted(node) {
                break;
            }
            if self.deleted(c) {
                continue;
            }
            out.push(c);
            if !self.lvl_mut(node, level).remove_incoming(c) {
                self.lvl_mut(c, level).push_incoming(node);
            }
        }
        self.lvl_mut(node, level).links = out;
    }

    // -----------------------------------------------------------------------------------------
    // The tiered index

    /// `executeSwapJob`'s bookkeeping after `deleted_id` was removed (the old last id renamed).
    fn swap_job_done(&mut self, deleted_id: Id, to_remove: &mut Vec<u64>) {
        let prev_last = self.elems.len() as u64;
        let d = deleted_id as u64;
        if let Some(jobs) = self.repairs.erase(d) {
            for _ in jobs {
                // invalidated repair jobs (none pend between commands)
            }
        }
        if let Some(jobs) = self.repairs.erase(prev_last) {
            self.repairs.insert(d, jobs);
        }
        if prev_last != d
            && let Some(&j) = self.swap_jobs.get(prev_last)
            && !to_remove.contains(&prev_last)
        {
            to_remove.push(prev_last);
            self.jobs[j].deleted_id = deleted_id;
            self.swap_jobs.set(d, j);
        } else {
            to_remove.push(d);
        }
    }

    /// `executeReadySwapJobs(max)`.
    pub fn execute_ready_swaps(&mut self, max: usize) {
        let mut to_remove: Vec<u64> = Vec::new();
        // the map's order (only values change while it is walked)
        for key in self.swap_jobs.keys_in_order() {
            let Some(&j) = self.swap_jobs.get(key) else { continue };
            let job = self.jobs[j];
            if job.pending == 0 && job.deleted_id != INVALID {
                self.remove_and_swap(job.deleted_id);
                self.marked -= 1;
                self.swap_job_done(job.deleted_id, &mut to_remove);
                self.jobs[j].deleted_id = INVALID;
            }
            if max > 0 && to_remove.len() >= max {
                break;
            }
        }
        for id in &to_remove {
            self.swap_jobs.erase(*id);
        }
        self.ready_swaps = self.ready_swaps.saturating_sub(to_remove.len());
    }

    /// `TieredHNSWIndex::addVector` (the vector preprocessed for storage).
    pub fn add(&mut self, v: &[u8], label: u64, mode: WriteMode, buffer_limit: usize) {
        if mode == WriteMode::InPlace {
            if !self.multi {
                self.delete(label, mode);
            }
            self.append(v, label);
            self.direct += 1;
            return;
        }
        if buffer_limit == 0 {
            if !self.multi {
                self.delete(label, mode);
            }
            self.append(v, label);
            self.direct += 1;
            return;
        }
        // through the flat buffer: an insert job, ingested by a worker
        self.flat_used = true;
        let b = self.insert_jobs_b.get_or_insert_with(Buckets::default);
        b.insert(0);
        if !self.multi {
            self.delete_async(label);
        }
        if self.ready_swaps >= SWAP_JOBS_THRESHOLD {
            self.execute_ready_swaps(SWAP_JOBS_THRESHOLD);
        }
        self.run_repairs();
        self.append(v, label);
    }

    /// `TieredHNSWIndex::deleteVector`: the vectors deleted.
    pub fn delete(&mut self, label: u64, mode: WriteMode) -> usize {
        if mode == WriteMode::Async {
            let n = self.delete_async(label);
            if self.ready_swaps >= SWAP_JOBS_THRESHOLD {
                self.execute_ready_swaps(SWAP_JOBS_THRESHOLD);
            }
            self.run_repairs();
            return n;
        }
        // `deleteLabelFromHNSWInplace`
        let Some((ids, _)) = self.lookup.get(&label) else { return 0 };
        let n = ids.len();
        self.ready_swaps += n;
        let mut to_remove = Vec::new();
        for k in 0..n {
            let id = self.lookup[&label].0[k];
            self.remove_in_place(id);
            self.swap_job_done(id, &mut to_remove);
        }
        self.lookup.remove(&label);
        for id in &to_remove {
            self.swap_jobs.erase(*id);
        }
        self.ready_swaps = self.ready_swaps.saturating_sub(to_remove.len());
        n
    }

    /// `deleteLabelFromHNSW`: the label's elements marked deleted, repair jobs queued for the
    /// nodes pointing at them, a swap job each.
    fn delete_async(&mut self, label: u64) -> usize {
        let Some((ids, _)) = self.lookup.get(&label).cloned() else { return 0 };
        for &id in &ids {
            self.mark_deleted(id);
        }
        self.lookup.remove(&label);
        for &id in &ids {
            let j = self.jobs.len();
            self.jobs.push(SwapJob { deleted_id: id, pending: 0 });
            let incoming = self.incoming_of(id);
            for &(node, level) in &incoming {
                let existing = self.repairs.get(node as u64).and_then(|v| v.iter().copied().find(|&r| self.pending_repairs[r].level == level));
                match existing {
                    Some(r) => self.pending_repairs[r].swaps.push(j),
                    None => {
                        if self.repairs.get(node as u64).is_none() {
                            self.repairs.insert(node as u64, Vec::new());
                        }
                        let r = self.pending_repairs.len();
                        self.pending_repairs.push(RepairJob { node, level, swaps: vec![j] });
                        self.repairs.get_mut(node as u64).expect("inserted").push(r);
                        self.queue.push(r);
                    }
                }
            }
            self.jobs[j].pending = incoming.len();
            if incoming.is_empty() {
                self.ready_swaps += 1;
            }
            self.swap_jobs.set(id as u64, j);
        }
        ids.len()
    }

    /// The workers run the queued repair jobs (`executeRepairJob`), in submission order.
    fn run_repairs(&mut self) {
        let queue = std::mem::take(&mut self.queue);
        for r in queue {
            let job = self.pending_repairs[r].clone();
            if let Some(list) = self.repairs.get_mut(job.node as u64) {
                if list.len() == 1 {
                    self.repairs.erase(job.node as u64);
                } else if let Some(p) = list.iter().position(|&x| x == r) {
                    let last = *list.last().expect("found");
                    list[p] = last;
                    list.pop();
                }
            }
            for s in job.swaps {
                let sj = &mut self.jobs[s];
                sj.pending -= 1;
                if sj.pending == 0 {
                    self.ready_swaps += 1;
                }
            }
            self.repair_node(job.node, job.level);
        }
        self.pending_repairs.clear();
    }

    /// `runGC` (after a fork GC cycle).
    pub fn run_gc(&mut self) {
        self.execute_ready_swaps(SWAP_JOBS_THRESHOLD);
    }

    /// `getHNSWElementNeighbors` for a label: each level's neighbours' labels.
    pub fn neighbors(&self, label: u64) -> Option<Vec<Vec<u64>>> {
        let (ids, _) = self.lookup.get(&label)?;
        let id = *ids.first()?;
        let e = &self.elems[id as usize];
        Some(e.levels.iter().map(|l| l.links.iter().map(|&x| self.label(x)).collect()).collect())
    }

    // -----------------------------------------------------------------------------------------
    // Memory

    /// The graph index's allocations (`HNSWIndex::getAllocationSize`).
    pub fn memory(&self) -> usize {
        let elem_bytes = 32 + 8 * self.m;
        let level_bytes = 16 + 4 * self.m;
        let mut total = 896;
        total += hdr(self.graph_blocks_cap * 48) + self.graph_blocks * hdr(self.block * elem_bytes);
        total += hdr(self.data_blocks_cap * 48) + self.data_blocks * block_bytes(self.ty, self.metric, self.dim, self.block, self.stored);
        total += hdr(self.meta_cap) + hdr(self.meta_cap * 9);
        total += self.lookup_b.bytes();
        total += if self.multi { self.lookup.values().map(|(_, cap)| 88 + hdr(cap * 4)).sum::<usize>() } else { 32 * self.lookup.len() };
        let handlers = self.visited_handlers.get();
        if handlers > 0 {
            total += handlers * (56 + 2 * self.visited_n + 8) + hdr(8 * handlers);
        }
        for e in &self.elems {
            total += 72 * (e.top + 1);
            if e.top > 0 {
                total += hdr(e.top * level_bytes);
            }
            total += e.levels.iter().map(|l| hdr(l.in_cap * 4)).sum::<usize>();
        }
        total
    }

    /// The tiered layer's own allocations (`MANAGEMENT_LAYER_MEMORY`): the index object, the
    /// job maps' buckets, pending swap jobs.
    pub fn layer_memory(&self) -> usize {
        let mut total = 600;
        if let Some(b) = &self.insert_jobs_b {
            total += b.bytes();
        }
        let live: HashSet<usize> = self.swap_jobs.iter().map(|(_, &j)| j).filter(|&j| self.jobs[j].deleted_id != INVALID).collect();
        total += self.swap_jobs.b.bytes() + 32 * self.swap_jobs.len() + 40 * live.len();
        total += self.repairs.b.bytes();
        total
    }
}

// ---------------------------------------------------------------------------------------------
// Batch iterators

/// `HNSW_BatchIterator` (single or multi value).
#[derive(Clone, Debug)]
pub struct HnswBatch {
    q: Vec<u8>,
    ef: usize,
    visited: Vec<bool>,
    entry: Id,
    depleted: bool,
    lower: f64,
    extras: StdHeap<(f64, u64)>,
    cands: StdHeap<(f64, Id)>,
    count: usize,
    returned: HashSet<u64>,
}

impl HnswBatch {
    pub fn new(ix: &Hnsw, q: Vec<u8>, ef_rt: usize) -> Self {
        ix.use_visited();
        HnswBatch {
            q,
            ef: if ef_rt > 0 { ef_rt } else { ix.ef },
            visited: vec![false; ix.elems.len()],
            entry: INVALID,
            depleted: false,
            lower: f64::INFINITY,
            extras: StdHeap::new(min_label),
            cands: StdHeap::new(min_id),
            count: 0,
            returned: HashSet::new(),
        }
    }

    pub fn is_depleted(&self) -> bool {
        self.depleted && self.extras.is_empty()
    }

    fn update(&mut self, ix: &Hnsw, top: &mut LabelHeap, d: f64, id: Id) {
        if !ix.multi {
            if top.len() < self.ef {
                top.emplace(d, ix.label(id));
                self.lower = top.top().0;
            } else if self.lower > d {
                top.emplace(d, ix.label(id));
                self.extras.push(top.top());
                top.pop();
                self.lower = top.top().0;
            }
            return;
        }
        if self.lower > d || top.len() < self.ef {
            let label = ix.label(id);
            if !self.returned.contains(&label) {
                top.emplace(d, label);
                if top.len() > self.ef {
                    self.extras.push(top.top());
                    top.pop();
                }
                self.lower = top.top().0;
            }
        }
    }

    fn fill_from_extras(&mut self, ix: &Hnsw, top: &mut LabelHeap) {
        while top.len() < self.ef && !self.extras.is_empty() {
            let (d, l) = self.extras.top();
            if !ix.multi || !self.returned.contains(&l) {
                top.emplace(d, l);
            }
            self.extras.pop();
        }
    }

    fn scan(&mut self, ix: &Hnsw) -> LabelHeap {
        let mut top = LabelHeap::new(ix.multi);
        if self.entry == INVALID {
            self.depleted = true;
            return top;
        }
        if self.count == 0 && self.extras.is_empty() && self.cands.is_empty() {
            self.lower = if !ix.deleted(self.entry) { ix.dist(&self.q, self.entry) } else { ix.dmax() };
            self.visited[self.entry as usize] = true;
            self.cands.push((self.lower, self.entry));
        }
        self.fill_from_extras(ix, &mut top);
        if top.len() == self.ef {
            return top;
        }
        while !self.cands.is_empty() {
            let (d, cur) = self.cands.top();
            if d > self.lower && top.len() >= self.ef {
                break;
            }
            if !ix.deleted(cur) {
                self.update(ix, &mut top, d, cur);
            }
            self.cands.pop();
            for &c in &ix.lvl(cur, 0).links {
                if self.visited[c as usize] {
                    continue;
                }
                self.visited[c as usize] = true;
                let cd = ix.dist(&self.q, c);
                self.cands.push((cd, c));
            }
        }
        if top.len() < self.ef {
            self.depleted = true;
        }
        top
    }

    /// `getNextResults(n, BY_SCORE_THEN_ID)`.
    pub fn next(&mut self, ix: &Hnsw, n: usize) -> Vec<(u64, f64)> {
        let orig = self.ef;
        if self.ef < n {
            self.ef = n;
        }
        if self.count == 0 {
            self.entry = ix.bottom_ep(&self.q);
        }
        let mut top = self.scan(ix);
        while top.len() > n {
            self.extras.push(top.top());
            top.pop();
        }
        let out = top.into_sorted();
        if ix.multi {
            self.returned.extend(out.iter().map(|r| r.0));
        }
        self.count += out.len();
        if self.count == ix.label_count() {
            self.depleted = true;
        }
        self.ef = orig;
        out
    }
}

/// `cmpVecSimQueryResultByScoreThenId`.
fn cmp_score_id(a: &(u64, f64), b: &(u64, f64)) -> i32 {
    if (a.1 - b.1).abs() >= 1e-6 {
        if a.1 > b.1 { 1 } else { -1 }
    } else {
        a.0.wrapping_sub(b.0) as u32 as i32
    }
}

/// `merge_results<withSet>(first, second, limit)`: the merged list and how many of each list
/// it consumed.
fn merge_results(first: &[(u64, f64)], second: &[(u64, f64)], mut limit: usize, with_set: bool) -> (Vec<(u64, f64)>, usize, usize) {
    let mut out = Vec::new();
    let mut ids: HashSet<u64> = HashSet::new();
    let (mut i, mut j) = (0, 0);
    let mut append = |out: &mut Vec<(u64, f64)>, r: (u64, f64), limit: &mut usize| {
        if !with_set || ids.insert(r.0) {
            out.push(r);
            *limit -= 1;
        }
    };
    while limit > 0 && i < first.len() && j < second.len() {
        let c = cmp_score_id(&first[i], &second[j]);
        if c > 0 {
            append(&mut out, second[j], &mut limit);
            j += 1;
        } else if c < 0 {
            append(&mut out, first[i], &mut limit);
            i += 1;
        } else {
            out.push(first[i]);
            i += 1;
            j += 1;
            limit -= 1;
        }
    }
    if limit != 0 {
        if i == first.len() {
            while limit > 0 && j < second.len() {
                append(&mut out, second[j], &mut limit);
                j += 1;
            }
        } else {
            while limit > 0 && i < first.len() {
                append(&mut out, first[i], &mut limit);
                i += 1;
            }
        }
    }
    (out, i, j)
}

/// `TieredHNSW_BatchIterator` over an empty flat buffer.
#[derive(Clone, Debug)]
pub struct TieredBatch {
    q: Vec<u8>,
    ef_rt: usize,
    hnsw: Option<HnswBatch>,
    started: bool,
    hnsw_depleted: bool,
    results: Vec<(u64, f64)>,
    returned: HashSet<u64>,
}

impl TieredBatch {
    pub fn new(q: Vec<u8>, ef_rt: usize) -> Self {
        TieredBatch { q, ef_rt, hnsw: None, started: false, hnsw_depleted: false, results: Vec::new(), returned: HashSet::new() }
    }

    fn filter(&mut self) {
        let r = &self.returned;
        self.results.retain(|x| !r.contains(&x.0));
    }

    /// `getNextResults(n, BY_ID)`.
    pub fn next(&mut self, ix: &Hnsw, n: usize) -> Vec<(u64, f64)> {
        if !self.started {
            self.started = true;
            let mut it = HnswBatch::new(ix, self.q.clone(), self.ef_rt);
            self.results = it.next(ix, n);
            if it.is_depleted() {
                self.hnsw_depleted = true;
            } else {
                self.hnsw = Some(it);
            }
        } else {
            while self.results.len() < n && !self.hnsw_depleted {
                let it = self.hnsw.as_mut().expect("not depleted");
                let tail = it.next(ix, n - self.results.len());
                let (merged, _, _) = merge_results(&self.results, &tail, n, false);
                let depleted = it.is_depleted();
                self.results = merged;
                self.filter();
                if depleted {
                    self.hnsw_depleted = true;
                    self.hnsw = None;
                }
            }
        }
        let (mut batch, from_hnsw, _) = merge_results(&self.results, &[], n, ix.multi);
        if ix.multi {
            self.returned.extend(batch.iter().map(|r| r.0));
        }
        self.results.drain(..from_hnsw);
        if ix.multi {
            self.filter();
        }
        batch.sort_by_key(|r| r.0);
        batch
    }

    pub fn has_next(&self) -> bool {
        !(self.results.is_empty() && self.hnsw_depleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout's rule restated over a plain vector: a node goes before the first node of its
    /// bucket, or first when its bucket is empty; rehashes re-place the nodes in order.
    fn model_place(v: &mut Vec<u64>, n: u64, k: u64) {
        match v.iter().position(|x| x % n == k % n) {
            Some(i) => v.insert(i, k),
            None => v.insert(0, k),
        }
    }

    #[test]
    fn umap_order() {
        let mut m: UMap<u32> = UMap::default();
        for k in 1..=5 {
            m.insert(k, 0);
        }
        assert_eq!(m.b.n, 13);
        assert_eq!(m.keys_in_order(), vec![5, 4, 3, 2, 1]);
        m.insert(14, 0);
        assert_eq!(m.keys_in_order(), vec![5, 4, 3, 2, 14, 1]);
        // random inserts and erases against the model
        let mut model: Vec<u64> = Vec::new();
        let mut b = Buckets::default();
        let mut m: UMap<u32> = UMap::default();
        let mut seed = 7u64;
        for _ in 0..3000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = (seed >> 33) % 97;
            if (seed >> 20) & 3 == 0 {
                if let Some(p) = model.iter().position(|&x| x == k) {
                    model.remove(p);
                }
                m.erase(k);
            } else if !model.contains(&k) {
                if b.insert(model.len()) {
                    let old = std::mem::take(&mut model);
                    for x in old {
                        model_place(&mut model, b.n as u64, x);
                    }
                }
                model_place(&mut model, b.n as u64, k);
                m.insert(k, 0);
            }
            assert_eq!(m.keys_in_order(), model);
        }
    }

    #[test]
    fn std_heap_layout() {
        // `std::priority_queue<int>` after pushing 1..=6: [6, 4, 5, 1, 3, 2]
        let mut h: StdHeap<i32> = StdHeap::new(|a, b| a < b);
        for x in 1..=6 {
            h.push(x);
        }
        assert_eq!(h.v, vec![6, 4, 5, 1, 3, 2]);
        assert_eq!(h.pop(), 6);
        assert_eq!(h.v, vec![5, 4, 2, 1, 3]);
    }
}
