//! The HNSW graph of a vector set, as the `vectorset` module bundled with Redis 8.10 builds it
//! (`hnsw.c`): bidirectional links only, vectors L2-normalized on insert (cosine distance as
//! `1 - dot`), per-vector int8 or binary quantization, true deletion with the orphaned neighbors
//! relinked among themselves, and the same candidate queues, linking heuristics and float
//! arithmetic so searches answer what Redis answers.

use super::fp;
use crate::bloom::topk::crand;

pub const NIL: u32 = u32::MAX;

pub const QUANT_NONE: u32 = 0;
pub const QUANT_Q8: u32 = 1;
pub const QUANT_BIN: u32 = 2;

pub const DEFAULT_M: u32 = 16;
pub const MIN_M: u32 = 4;
pub const MAX_M: u32 = 4096;

/// Probability of a node reaching one level more.
const P: f64 = 0.25;
pub const MAX_LEVEL: u32 = 16;
/// The candidate list size used when relinking after a deletion.
const EF_C: u32 = 200;
/// The largest candidate queue a layer search keeps.
const MAX_CANDIDATES: u32 = 256;
const SERIALIZATION_VERSION: u64 = 1;
const WORSTLINK_MISSING: u32 = u32::MAX;

/// A stored (or query) vector: full floats, int8 quants with their range, or sign bits.
#[derive(Clone, Debug, PartialEq)]
pub enum Data {
    F32(Box<[f32]>),
    Q8(Box<[i8]>),
    Bin(Box<[u64]>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vect {
    pub data: Data,
    /// The quantization range (max abs value) of a Q8 vector.
    pub range: f32,
}

impl Vect {
    /// The bytes Redis keeps (and saves) for the vector.
    pub fn bytes(&self) -> Vec<u8> {
        match &self.data {
            Data::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Data::Q8(v) => v.iter().map(|&x| x as u8).collect(),
            Data::Bin(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub links: Vec<u32>,
    pub max_links: u32,
    pub worst_distance: f32,
    pub worst_idx: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub level: u32,
    pub id: u64,
    pub v: Vect,
    /// The L2 norm before normalization.
    pub l2: f32,
    pub layers: Vec<Layer>,
    pub prev: u32,
    pub next: u32,
    /// The element.
    pub item: Box<[u8]>,
    /// Its JSON attributes.
    pub attrib: Option<Box<[u8]>>,
    /// Whether the element / attribute string objects are raw (loaded from an RDB) rather than
    /// created from a command's arguments (`RM_MallocSizeString`).
    pub item_raw: bool,
    pub attrib_raw: bool,
}

/// A priority queue of `(node, distance)` keeping the `cap` nearest, farthest first (`pqueue`).
pub struct Pq {
    items: Vec<(u32, f32)>,
    cap: usize,
}

impl Pq {
    fn new(cap: u32) -> Self {
        Pq { items: Vec::with_capacity((cap as usize).min(1024)), cap: cap as usize }
    }

    fn push(&mut self, node: u32, distance: f32) {
        let it = &mut self.items;
        if it.len() < self.cap {
            let mut i = it.len();
            it.push((node, distance));
            while i > 0 && it[i - 1].1 < distance {
                it[i] = it[i - 1];
                i -= 1;
            }
            it[i] = (node, distance);
        } else {
            if self.cap == 0 || distance >= it[0].1 {
                return;
            }
            let mut i = 0;
            while i < self.cap - 1 && it[i + 1].1 > distance {
                it[i] = it[i + 1];
                i += 1;
            }
            it[i] = (node, distance);
        }
    }

    fn pop(&mut self) -> Option<(u32, f32)> {
        self.items.pop()
    }

    fn max_distance(&self) -> f32 {
        self.items.first().map_or(f32::INFINITY, |x| x.1)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The `i`-th nearest.
    pub fn get(&self, i: usize) -> (u32, f32) {
        self.items[self.items.len() - 1 - i]
    }
}

/// `random_level`: each level with probability 0.25, from the process' `rand()`.
pub fn random_level() -> u32 {
    let threshold = (P * crand::RAND_MAX as f64) as i32;
    let mut level = 0;
    while crand::rand() < threshold && level < MAX_LEVEL - 1 {
        level += 1;
    }
    level
}

#[derive(Clone, Debug, Default)]
struct Visited {
    epoch: u64,
    marks: Vec<u64>,
}

/// The graph proper: nodes in a slab, linked in insertion order from `head` (newest first).
#[derive(Clone, Debug)]
pub struct Graph {
    pub nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    pub enter_point: u32,
    pub m: u32,
    pub max_level: u32,
    pub dim: u32,
    pub node_count: u64,
    pub last_id: u64,
    pub head: u32,
    pub quant: u32,
}

#[derive(Clone, Debug)]
pub struct Hnsw {
    pub g: Graph,
    visited: Visited,
}

/// A filter on the nodes a search may return.
pub type Filter<'a> = &'a mut dyn FnMut(&Node) -> bool;

impl Graph {
    #[inline]
    pub fn n(&self, i: u32) -> &Node {
        self.nodes[i as usize].as_ref().expect("live node")
    }

    #[inline]
    fn nm(&mut self, i: u32) -> &mut Node {
        self.nodes[i as usize].as_mut().expect("live node")
    }

    #[inline]
    fn dist(&self, a: u32, b: u32) -> f32 {
        fp::distance(&self.n(a).v, &self.n(b).v, self.dim)
    }

    #[inline]
    fn nlinks(&self, i: u32, layer: usize) -> u32 {
        self.n(i).layers[layer].links.len() as u32
    }

    /// The nodes from the newest.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Node)> + '_ {
        let mut cur = self.head;
        std::iter::from_fn(move || {
            if cur == NIL {
                return None;
            }
            let i = cur;
            let n = self.n(i);
            cur = n.next;
            Some((i, n))
        })
    }

    fn update_worst(&mut self, node: u32, layer: usize) {
        let mut worst = 0f32;
        let mut idx = 0u32;
        for (i, &l) in self.n(node).layers[layer].links.iter().enumerate() {
            let d = self.dist(node, l);
            if d > worst {
                worst = d;
                idx = i as u32;
            }
        }
        let ly = &mut self.nm(node).layers[layer];
        ly.worst_distance = worst;
        ly.worst_idx = idx;
    }

    fn update_worst_on_add(&mut self, node: u32, layer: usize, added: u32, distance: f32) {
        let ly = &mut self.nm(node).layers[layer];
        if ly.links.len() == 1 || distance > ly.worst_distance {
            ly.worst_distance = distance;
            ly.worst_idx = added;
        }
    }

    fn update_worst_on_remove(&mut self, node: u32, layer: usize, removed: u32) {
        let ly = &self.n(node).layers[layer];
        if ly.links.is_empty() {
            let ly = &mut self.nm(node).layers[layer];
            ly.worst_distance = 0.0;
            ly.worst_idx = 0;
        } else if removed == ly.worst_idx {
            self.update_worst(node, layer);
        } else if removed < ly.worst_idx {
            self.nm(node).layers[layer].worst_idx -= 1;
        }
    }

    /// Link `a` and `b` both ways at `layer` (`b`'s side first), updating the worst links.
    fn link_pair(&mut self, nb: u32, new: u32, layer: usize, dist: f32) {
        let n = self.nlinks(nb, layer);
        self.nm(nb).layers[layer].links.push(new);
        self.update_worst_on_add(nb, layer, n, dist);
        let k = self.nlinks(new, layer);
        self.nm(new).layers[layer].links.push(nb);
        self.update_worst_on_add(new, layer, k, dist);
    }

    /// `select_neighbors`: link `new` to candidates, keeping links bidirectional.
    fn select_neighbors(&mut self, cand: &Pq, new: u32, layer: usize, required: u32, aggressive: u8) {
        let m = self.m;
        for i in 0..cand.len() {
            let (nb, dist) = cand.get(i);
            if nb == new {
                continue;
            }
            {
                let nl = &self.n(new).layers[layer];
                let n = nl.links.len() as u32;
                if n >= nl.max_links || n >= required {
                    return;
                }
            }
            if aggressive != 0 && self.n(new).layers[layer].links.contains(&nb) {
                continue;
            }
            if aggressive == 0 && self.n(new).layers[layer].links.iter().any(|&l| self.dist(nb, l) < dist) {
                continue;
            }
            let n = self.nlinks(nb, layer);
            if n < self.n(nb).layers[layer].max_links {
                self.link_pair(nb, new, layer, dist);
                continue;
            }
            // The candidate is full: replace its worst link with the new node.
            if aggressive == 0 && dist >= self.n(nb).layers[layer].worst_distance {
                continue;
            }
            let Some(&first_worst) = self.n(nb).layers[layer].links.get(self.n(nb).layers[layer].worst_idx as usize) else {
                continue;
            };
            let mut worst_node = first_worst;
            let wl = self.nlinks(worst_node, layer);
            if (aggressive == 0 && wl <= m / 2) || (aggressive == 1 && wl <= m / 4) {
                continue;
            }
            if aggressive == 2 && wl <= m / 4 {
                let mut found = NIL;
                let mut widx = 0u32;
                let mut max_dist = 0f32;
                for (j, &to_drop) in self.n(nb).layers[layer].links.iter().enumerate() {
                    if self.nlinks(to_drop, layer) <= m / 4 {
                        continue;
                    }
                    let ld = self.dist(nb, to_drop);
                    if found == NIL || ld > max_dist {
                        found = to_drop;
                        max_dist = ld;
                        widx = j as u32;
                    }
                }
                if found != NIL {
                    let ly = &mut self.nm(nb).layers[layer];
                    ly.worst_distance = max_dist;
                    ly.worst_idx = widx;
                    worst_node = found;
                } else {
                    let limit = if layer == 0 { m * 3 } else { m * 2 };
                    if self.n(nb).layers[layer].max_links >= limit {
                        continue;
                    }
                    self.nm(nb).layers[layer].max_links += 1;
                    self.link_pair(nb, new, layer, dist);
                    continue;
                }
            }
            // Drop the candidate from its worst link's links.
            if let Some(j) = self.n(worst_node).layers[layer].links.iter().position(|&x| x == nb) {
                self.nm(worst_node).layers[layer].links.remove(j);
                self.update_worst_on_remove(worst_node, layer, j as u32);
            }
            let wi = self.n(nb).layers[layer].worst_idx as usize;
            self.nm(nb).layers[layer].links[wi] = new;
            self.update_worst(nb, layer);
            let k = self.nlinks(new, layer);
            self.nm(new).layers[layer].links.push(nb);
            self.update_worst_on_add(new, layer, k, dist);
        }
    }
}

/// `search_layer_with_filter`: the `ef` nodes nearest to `q` reachable at `layer` from `entry`.
#[allow(clippy::too_many_arguments)]
fn search_layer(
    g: &Graph,
    vis: &mut Visited,
    q: &Vect,
    entry: u32,
    ef: u32,
    layer: usize,
    mut filter: Option<Filter>,
    max_candidates: u32,
) -> Pq {
    vis.epoch += 1;
    let epoch = vis.epoch;
    let mut candidates = Pq::new(MAX_CANDIDATES);
    let mut results = Pq::new(ef);
    let mut evaluated = 1u32;

    let d = fp::distance(q, &g.n(entry).v, g.dim);
    candidates.push(entry, d);
    if filter.as_mut().is_none_or(|f| f(g.n(entry))) {
        results.push(entry, d);
    }
    vis.marks[entry as usize] = epoch;

    while !candidates.is_empty() {
        if filter.is_some() && max_candidates != 0 && evaluated >= max_candidates {
            break;
        }
        let (current, cur_dist) = candidates.pop().expect("non-empty");
        evaluated = evaluated.wrapping_add(1);
        let furthest = results.max_distance();
        if results.len() >= ef as usize && cur_dist > furthest {
            break;
        }
        for &nb in &g.n(current).layers[layer].links {
            if vis.marks[nb as usize] == epoch {
                continue;
            }
            vis.marks[nb as usize] = epoch;
            let nd = fp::distance(q, &g.n(nb).v, g.dim);
            let furthest = results.max_distance();
            match filter.as_mut() {
                None => {
                    if nd < furthest || results.len() < ef as usize {
                        candidates.push(nb, nd);
                        results.push(nb, nd);
                    }
                }
                Some(f) => {
                    if nd < furthest || candidates.len() < ef as usize {
                        candidates.push(nb, nd);
                    }
                    if f(g.n(nb)) && (nd < furthest || results.len() < ef as usize) {
                        results.push(nb, nd);
                    }
                }
            }
        }
    }
    results
}

impl Hnsw {
    /// `hnsw_new`: an empty graph (`m` 0 means the default 16, capped at 4096).
    pub fn new(dim: u32, quant: u32, m: u32) -> Self {
        let m = if m == 0 { DEFAULT_M } else { m.min(MAX_M) };
        Hnsw {
            g: Graph {
                nodes: Vec::new(),
                free: Vec::new(),
                enter_point: NIL,
                m,
                max_level: 0,
                dim,
                node_count: 0,
                last_id: 0,
                head: NIL,
                quant,
            },
            visited: Visited::default(),
        }
    }

    /// `hnsw_quants_bytes`.
    pub fn quants_bytes(&self) -> usize {
        let d = self.g.dim as usize;
        match self.g.quant {
            QUANT_NONE => d * 4,
            QUANT_Q8 => d,
            _ => d.div_ceil(64) * 8,
        }
    }

    /// The vector a node keeps for `vec` (normalized when asked, then quantized), and its L2.
    pub fn make_vect(&self, vec: &[f32], normalize: bool) -> (Vect, f32) {
        let mut v = vec.to_vec();
        let mut l2 = 1.0f32;
        if normalize && let Some(x) = fp::normalize(&mut v) {
            l2 = x;
        }
        (self.quantize(v), l2)
    }

    fn quantize(&self, v: Vec<f32>) -> Vect {
        match self.g.quant {
            QUANT_NONE => Vect { data: Data::F32(v.into()), range: 0.0 },
            QUANT_Q8 => {
                let (q, range) = fp::quantize_q8(&v);
                Vect { data: Data::Q8(q.into()), range }
            }
            _ => Vect { data: Data::Bin(fp::quantize_bin(&v).into()), range: 0.0 },
        }
    }

    /// `hnsw_init_tmp_node`: a query vector, normalized then quantized like the nodes.
    pub fn probe(&self, vec: &[f32]) -> Vect {
        let mut v = vec.to_vec();
        fp::normalize(&mut v);
        self.quantize(v)
    }

    fn alloc(&mut self, node: Node) -> u32 {
        let i = match self.g.free.pop() {
            Some(i) => {
                self.g.nodes[i as usize] = Some(node);
                i
            }
            None => {
                self.g.nodes.push(Some(node));
                (self.g.nodes.len() - 1) as u32
            }
        };
        if self.visited.marks.len() < self.g.nodes.len() {
            self.visited.marks.resize(self.g.nodes.len(), 0);
        }
        self.visited.marks[i as usize] = 0;
        i
    }

    /// `hnsw_add_node`: at the head of the list.
    fn link_head(&mut self, i: u32) {
        let head = self.g.head;
        {
            let n = self.g.nm(i);
            n.next = head;
            n.prev = NIL;
        }
        if head != NIL {
            self.g.nm(head).prev = i;
        }
        self.g.head = i;
        self.g.node_count += 1;
    }

    fn new_node(&mut self, id: u64, v: Vect, l2: f32, level: u32, item: Box<[u8]>, attrib: Option<Box<[u8]>>) -> Node {
        let id = if id == 0 {
            self.g.last_id += 1;
            self.g.last_id
        } else {
            id
        };
        let m = self.g.m;
        let layers = (0..=level)
            .map(|i| {
                let max_links = if i == 0 { m * 2 } else { m };
                Layer { links: Vec::with_capacity(max_links as usize), max_links, worst_distance: 0.0, worst_idx: 0 }
            })
            .collect();
        Node { level, id, v, l2, layers, prev: NIL, next: NIL, item, attrib, item_raw: false, attrib_raw: false }
    }

    /// `hnsw_insert`: a new node for `vec` (normalized and quantized here), linked with up to M
    /// neighbors per level found with an `ef` wide search.
    pub fn insert(&mut self, vec: &[f32], ef: u32, item: Box<[u8]>, attrib: Option<Box<[u8]>>) -> u32 {
        let level = random_level();
        let (v, l2) = self.make_vect(vec, true);
        let node = self.new_node(0, v, l2, level, item, attrib);
        let new = self.alloc(node);

        if self.g.enter_point == NIL {
            self.g.enter_point = new;
            self.g.max_level = level;
            self.link_head(new);
            return new;
        }

        // Collect candidates (prepare), then link (commit).
        let mut queues: Vec<Option<Pq>> = (0..=level).map(|_| None).collect();
        {
            let g = &self.g;
            let q = &g.n(new).v;
            let mut ep = g.enter_point;
            let mut lc = g.max_level;
            while lc > level {
                let r = search_layer(g, &mut self.visited, q, ep, 1, lc as usize, None, 0);
                if !r.is_empty() {
                    ep = r.get(0).0;
                }
                lc -= 1;
            }
            for lc in (0..=level.min(g.max_level)).rev() {
                let c = search_layer(g, &mut self.visited, q, ep, ef, lc as usize, None, 0);
                if !c.is_empty() {
                    ep = c.get(0).0;
                }
                queues[lc as usize] = Some(c);
            }
        }
        let m = self.g.m;
        for lc in (0..=level.min(self.g.max_level) as usize).rev() {
            let Some(c) = queues[lc].take() else { continue };
            self.g.select_neighbors(&c, new, lc, m, 0);
            if lc == 0 && self.g.nlinks(new, 0) < m / 2 {
                self.g.select_neighbors(&c, new, lc, m, 1);
                if self.g.nlinks(new, 0) < m / 4 {
                    self.g.select_neighbors(&c, new, lc, m / 4, 2);
                }
            }
        }
        if level > self.g.max_level {
            self.g.enter_point = new;
            self.g.max_level = level;
        }
        self.link_head(new);
        new
    }

    /// `hnsw_unlink_node`: out of every neighbor's links and of the list; a new entry point when
    /// it was the entry point. The node keeps its own links.
    fn unlink(&mut self, node: u32) {
        let level = self.g.n(node).level as usize;
        for l in 0..=level {
            let links = self.g.n(node).layers[l].links.clone();
            for linked in links {
                if let Some(j) = self.g.n(linked).layers[l].links.iter().position(|&x| x == node) {
                    self.g.nm(linked).layers[l].links.remove(j);
                    self.g.update_worst_on_remove(linked, l, j as u32);
                }
            }
        }
        let (prev, next) = {
            let n = self.g.n(node);
            (n.prev, n.next)
        };
        if prev != NIL {
            self.g.nm(prev).next = next;
        } else {
            self.g.head = next;
        }
        if next != NIL {
            self.g.nm(next).prev = prev;
        }
        self.g.node_count -= 1;

        if node == self.g.enter_point {
            self.g.enter_point = NIL;
            self.g.max_level = 0;
            let n = self.g.n(node);
            for l in (0..=n.level as usize).rev() {
                if let Some(&first) = n.layers[l].links.first() {
                    self.g.enter_point = first;
                    break;
                }
            }
            if self.g.enter_point == NIL {
                let mut best_level = 0;
                let mut cur = self.g.head;
                let mut ep = NIL;
                while cur != NIL {
                    let c = self.g.n(cur);
                    if cur != node && c.level >= best_level {
                        best_level = c.level;
                        ep = cur;
                    }
                    cur = c.next;
                }
                self.g.enter_point = ep;
            }
            if self.g.enter_point != NIL {
                self.g.max_level = self.g.n(self.g.enter_point).level;
            }
        }
        let n = self.g.nm(node);
        n.prev = NIL;
        n.next = NIL;
    }

    /// `hnsw_delete_node`: unlink the node, relink its former neighbors at every level, free it.
    pub fn delete(&mut self, node: u32) -> Node {
        self.unlink(node);
        let level = self.g.n(node).level as usize;
        for j in 0..=level {
            let nodes = self.g.n(node).layers[j].links.clone();
            self.reconnect(&nodes, j);
        }
        let n = self.g.nodes[node as usize].take().expect("live node");
        self.g.free.push(node);
        n
    }

    /// `hnsw_reconnect_nodes`: pair the nodes that lost a link, best scored pairs first, then
    /// give the unpaired ones a link from the local nodes or a wider search.
    fn reconnect(&mut self, nodes: &[u32], layer: usize) {
        let count = nodes.len();
        if count == 0 {
            return;
        }
        let mut distances = vec![0f32; count * count];
        for i in 0..count {
            for j in i + 1..count {
                let d = self.g.dist(nodes[i], nodes[j]);
                distances[i * count + j] = d;
                distances[j * count + i] = d;
            }
        }
        let mut row_avgs = vec![0f32; count];
        for i in 0..count {
            let mut sum = 0f32;
            let mut valid = 0i32;
            for j in 0..count {
                if i != j {
                    sum += distances[i * count + j];
                    valid += 1;
                }
            }
            row_avgs[i] = if valid != 0 { sum / valid as f32 } else { 0.0 };
        }
        const W1: f32 = 0.7;
        const W2: f32 = 0.3;
        let mut scores = vec![0f32; count * count];
        for i in 0..count {
            for j in 0..count {
                if i == j || self.g.n(nodes[i]).layers[layer].links.contains(&nodes[j]) {
                    scores[i * count + j] = -1.0;
                    continue;
                }
                let dist = distances[i * count + j];
                let (mut ai, mut aj) = (0f32, 0f32);
                if count > 2 {
                    ai = fp::mul_sub(row_avgs[i], (count - 1) as f32, dist) / (count - 2) as f32;
                    aj = fp::mul_sub(row_avgs[j], (count - 1) as f32, dist) / (count - 2) as f32;
                }
                scores[i * count + j] = fp::mul_add(W1, 2.0 - dist, W2 * ((ai + aj) / 2.0));
            }
        }
        let mut used = vec![false; count];
        loop {
            let mut max_score = -1f32;
            let (mut bi, mut bj) = (usize::MAX, usize::MAX);
            for i in 0..count {
                if used[i] {
                    continue;
                }
                let ni = &self.g.n(nodes[i]).layers[layer];
                if ni.links.len() as u32 >= ni.max_links {
                    continue;
                }
                for j in 0..count {
                    if i == j || used[j] {
                        continue;
                    }
                    let s = scores[i * count + j];
                    if s < 0.0 {
                        continue;
                    }
                    let nj = &self.g.n(nodes[j]).layers[layer];
                    if s > max_score && (nj.links.len() as u32) < nj.max_links {
                        max_score = s;
                        bi = i;
                        bj = j;
                    }
                }
            }
            if bj == usize::MAX {
                break;
            }
            let d = distances[bi * count + bj];
            let (a, b) = (nodes[bi], nodes[bj]);
            let k = self.g.nlinks(a, layer);
            self.g.nm(a).layers[layer].links.push(b);
            self.g.update_worst_on_add(a, layer, k, d);
            let k = self.g.nlinks(b, layer);
            self.g.nm(b).layers[layer].links.push(a);
            self.g.update_worst_on_add(b, layer, k, d);
            used[bi] = true;
            used[bj] = true;
        }

        for i in 0..count {
            if used[i] {
                continue;
            }
            let ni = nodes[i];
            if self.g.nlinks(ni, layer) >= self.g.n(ni).layers[layer].max_links {
                continue;
            }
            let mut candidates = Pq::new(count as u32);
            for j in 0..count {
                let nj = &self.g.n(nodes[j]).layers[layer];
                if i != j && (nj.links.len() as u32) < nj.max_links {
                    candidates.push(nodes[j], distances[i * count + j]);
                }
            }
            let wanted = self.g.nlinks(ni, layer) + 1;
            if !candidates.is_empty() {
                self.g.select_neighbors(&candidates, ni, layer, wanted, 1);
            }
            if self.g.nlinks(ni, layer) != wanted {
                let mut ep = self.g.enter_point;
                let mut lc = self.g.max_level as usize;
                while lc > layer {
                    let r = search_layer(&self.g, &mut self.visited, &self.g.n(ni).v, ep, 1, lc, None, 0);
                    if !r.is_empty() {
                        ep = r.get(0).0;
                    }
                    lc -= 1;
                }
                if ep != NIL {
                    let c = search_layer(&self.g, &mut self.visited, &self.g.n(ni).v, ep, EF_C, layer, None, 0);
                    let aggressive = if self.g.nlinks(ni, layer) > self.g.m / 2 { 1 } else { 2 };
                    self.g.select_neighbors(&c, ni, layer, wanted, aggressive);
                }
            }
        }
    }

    /// `hnsw_search_with_filter`: up to `k` nodes nearest to the (normalized, quantized) query,
    /// nearest first, with their distances. A filter stops after `max_candidates` evaluations.
    pub fn search(&mut self, q: &Vect, k: u32, filter: Option<Filter>, max_candidates: u32) -> Vec<(u32, f32)> {
        if k == 0 || self.g.enter_point == NIL {
            return Vec::new();
        }
        let mut ep = self.g.enter_point;
        let mut lc = self.g.max_level as usize;
        while lc > 0 {
            let r = search_layer(&self.g, &mut self.visited, q, ep, 1, lc, None, 0);
            if !r.is_empty() {
                ep = r.get(0).0;
            }
            lc -= 1;
        }
        let r = search_layer(&self.g, &mut self.visited, q, ep, k, 0, filter, max_candidates);
        let found = (k as usize).min(r.len());
        (0..found).map(|i| r.get(i)).collect()
    }

    /// `hnsw_ground_truth_with_filter`: the same by a linear scan of the list.
    pub fn ground_truth(&self, q: &Vect, k: u32, mut filter: Option<Filter>) -> Vec<(u32, f32)> {
        let mut r = Pq::new(k);
        for (i, n) in self.g.iter() {
            if let Some(f) = filter.as_mut()
                && !f(n)
            {
                continue;
            }
            r.push(i, fp::distance(q, &n.v, self.g.dim));
        }
        let found = (k as usize).min(r.len());
        (0..found).map(|i| r.get(i)).collect()
    }

    /// `hnsw_random_node`: a random walk down from the entry point, then `3·log2(N+1)` (plus
    /// zero or one) steps at level 0.
    pub fn random_node(&self) -> u32 {
        let g = &self.g;
        if g.node_count == 0 || g.enter_point == NIL {
            return NIL;
        }
        let mut cur = g.enter_point;
        let mut level = g.max_level;
        while level > 0 {
            let n = g.n(cur);
            if n.level >= level && !n.layers[level as usize].links.is_empty() {
                let links = &n.layers[level as usize].links;
                cur = links[(crand::rand() as u32 % links.len() as u32) as usize];
            }
            level -= 1;
        }
        let log_n = ((g.node_count + 1) as f64).log2();
        let mut walks = (log_n * 3.0) as u32;
        walks += (crand::rand() & 1) as u32;
        for _ in 0..walks {
            let links = &g.n(cur).layers[0].links;
            if links.is_empty() {
                return cur;
            }
            cur = links[(crand::rand() as u32 % links.len() as u32) as usize];
        }
        cur
    }

    /// `hnsw_get_node_vector`: the vector as given, approximately (dequantized, denormalized).
    pub fn node_vector(&self, i: u32) -> Vec<f32> {
        let n = self.g.n(i);
        let dim = self.g.dim as usize;
        let mut out: Vec<f32> = match &n.v.data {
            Data::F32(v) => v.to_vec(),
            Data::Q8(q) => q.iter().map(|&x| (x as f32 * n.v.range) / 127.0).collect(),
            Data::Bin(b) => (0..dim).map(|j| if b[j / 64] & (1u64 << (j & 63)) != 0 { 1.0 } else { -1.0 }).collect(),
        };
        if self.g.quant != QUANT_BIN {
            for x in &mut out {
                *x *= n.l2;
            }
        }
        out
    }

    /// `hnsw_distance` between two nodes.
    pub fn distance(&self, a: u32, b: u32) -> f32 {
        self.g.dist(a, b)
    }

    // --- serialization ---

    /// `hnsw_serialize_node`: the node's parameters (id, version and level, per layer its link
    /// counts, linked ids and packed worst link, then the packed L2 and range).
    pub fn serialize(&self, i: u32) -> Vec<u64> {
        let n = self.g.n(i);
        let mut p = Vec::with_capacity(3 + n.layers.iter().map(|l| l.links.len() + 3).sum::<usize>());
        p.push(n.id);
        p.push((n.level as u64 & 0xff) | (SERIALIZATION_VERSION << 24));
        for l in &n.layers {
            p.push(l.links.len() as u64);
            p.push(l.max_links as u64);
            p.extend(l.links.iter().map(|&x| self.g.n(x).id));
            p.push(((l.worst_distance.to_bits() as u64) << 32) | l.worst_idx as u64);
        }
        p.push(((n.v.range.to_bits() as u64) << 32) | n.l2.to_bits() as u64);
        p
    }

    /// `hnsw_insert_serialized`: a node from its saved vector bytes and parameters. Its links
    /// are ids until [`Hnsw::deserialize`] resolves them.
    pub fn insert_serialized(&mut self, vector: &[u8], params: &[u64], item: Box<[u8]>, attrib: Option<Box<[u8]>>, pending: &mut Vec<(u32, Vec<Vec<u64>>)>) -> Option<u32> {
        if params.len() < 2 {
            return None;
        }
        let id = params[0];
        let level = (params[1] & 0xff) as u32;
        let version = (params[1] & 0xff00_0000) >> 24;
        if version > SERIALIZATION_VERSION || level >= MAX_LEVEL {
            return None;
        }
        let has_worst = version > 0;
        if id >= self.g.last_id {
            self.g.last_id = id;
        }
        if vector.len() != self.quants_bytes() {
            return None;
        }
        let data = match self.g.quant {
            QUANT_NONE => Data::F32(vector.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()),
            QUANT_Q8 => Data::Q8(vector.iter().map(|&b| b as i8).collect()),
            _ => Data::Bin(vector.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect()),
        };
        let mut node = self.new_node(id, Vect { data, range: 0.0 }, 1.0, level, item, attrib);
        let mut ids = Vec::with_capacity(level as usize + 1);
        let mut at = 2usize;
        let extra = has_worst as usize;
        for l in node.layers.iter_mut() {
            if at + 2 + extra > params.len() {
                return None;
            }
            let num = params[at] as u32;
            let max = params[at + 1] as u32;
            at += 2;
            if num > max || max > MAX_M * 4 {
                return None;
            }
            if max > l.max_links {
                l.max_links = max;
            }
            if at + num as usize + extra > params.len() {
                return None;
            }
            ids.push(params[at..at + num as usize].to_vec());
            at += num as usize;
            if has_worst {
                let wi = params[at];
                at += 1;
                l.worst_idx = wi as u32;
                l.worst_distance = f32::from_bits((wi >> 32) as u32);
                if num > 0 && l.worst_idx >= num {
                    return None;
                }
            } else {
                l.worst_idx = WORSTLINK_MISSING;
                l.worst_distance = 0.0;
            }
        }
        if at >= params.len() {
            return None;
        }
        let packed = params[at];
        node.l2 = f32::from_bits(packed as u32);
        node.v.range = f32::from_bits((packed >> 32) as u32);
        let i = self.alloc(node);
        self.link_head(i);
        if self.g.enter_point == NIL || level > self.g.max_level {
            self.g.max_level = level;
            self.g.enter_point = i;
        }
        pending.push((i, ids));
        Some(i)
    }

    /// `hnsw_deserialize_index`: resolve the loaded links (sorted by id, no duplicates, no
    /// self links, every target present at that level, every link reciprocated).
    pub fn deserialize(&mut self, pending: Vec<(u32, Vec<Vec<u64>>)>) -> bool {
        let by_id: std::collections::HashMap<u64, u32> = self.g.iter().map(|(i, n)| (n.id, i)).collect();
        let mut pairs: std::collections::HashMap<(u64, u64, usize), u32> = std::collections::HashMap::new();
        let mut order: std::collections::HashMap<u32, Vec<Vec<u64>>> = pending.into_iter().collect();
        let list: Vec<u32> = self.g.iter().map(|(i, _)| i).collect();
        for i in list {
            let this_id = self.g.n(i).id;
            let Some(mut layers) = order.remove(&i) else { return false };
            for (l, ids) in layers.iter_mut().enumerate() {
                ids.sort_unstable();
                if ids.windows(2).any(|w| w[0] == w[1]) {
                    return false;
                }
                let mut links = Vec::with_capacity(ids.len());
                for &lid in ids.iter() {
                    if lid == this_id {
                        return false;
                    }
                    *pairs.entry((this_id.min(lid), this_id.max(lid), l)).or_default() += 1;
                    match by_id.get(&lid) {
                        Some(&t) if self.g.n(t).level as usize >= l => links.push(t),
                        _ => return false,
                    }
                }
                self.g.nm(i).layers[l].links = links;
                if self.g.n(i).layers[l].worst_idx == WORSTLINK_MISSING {
                    self.g.update_worst(i, l);
                }
            }
        }
        pairs.values().all(|c| c % 2 == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vecs(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                (0..dim)
                    .map(|_| {
                        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        ((s >> 40) as f32 / (1u64 << 24) as f32) - 0.5
                    })
                    .collect()
            })
            .collect()
    }

    fn graph(quant: u32, n: usize) -> (Hnsw, Vec<Vec<f32>>) {
        let vs = vecs(n, 12, 7);
        let mut h = Hnsw::new(12, quant, 0);
        for (i, v) in vs.iter().enumerate() {
            h.insert(v, 200, format!("e{i}").into_bytes().into(), None);
        }
        (h, vs)
    }

    fn check_links(h: &Hnsw) {
        for (i, n) in h.g.iter() {
            for (l, ly) in n.layers.iter().enumerate() {
                assert!(ly.links.len() as u32 <= ly.max_links);
                for &t in &ly.links {
                    assert!(h.g.n(t).layers[l].links.contains(&i), "link {i}->{t} at {l} not reciprocal");
                }
            }
        }
    }

    #[test]
    fn finds_exact_neighbors_in_small_sets() {
        for quant in [QUANT_NONE, QUANT_Q8, QUANT_BIN] {
            let (mut h, vs) = graph(quant, 60);
            assert_eq!(h.g.node_count, 60);
            check_links(&h);
            let q = h.probe(&vs[5]);
            let r = h.search(&q, 60, None, 0);
            let truth = h.ground_truth(&q, 60, None);
            assert_eq!(r.len(), 60);
            assert_eq!(r[0].1, truth[0].1);
            if quant != QUANT_BIN {
                assert_eq!(&*h.g.n(r[0].0).item, b"e5");
            }
        }
    }

    #[test]
    fn deletes_keep_links_reciprocal() {
        let (mut h, _) = graph(QUANT_Q8, 80);
        let ids: Vec<u32> = h.g.iter().map(|(i, _)| i).collect();
        for &i in ids.iter().step_by(3) {
            h.delete(i);
            check_links(&h);
        }
        assert_eq!(h.g.node_count as usize, 80 - ids.iter().step_by(3).count());
        assert!(h.g.enter_point != NIL);
    }

    #[test]
    fn serialization_round_trips() {
        let (h, _) = graph(QUANT_Q8, 40);
        let mut l = Hnsw::new(12, QUANT_Q8, 16);
        let mut pending = Vec::new();
        for (i, n) in h.g.iter() {
            l.insert_serialized(&n.v.bytes(), &h.serialize(i), n.item.clone(), None, &mut pending).unwrap();
        }
        assert!(l.deserialize(pending));
        check_links(&l);
        assert_eq!(l.g.node_count, 40);
        assert_eq!(l.g.last_id, h.g.last_id);
        // The list comes back reversed.
        assert_eq!(l.g.n(l.g.head).item, h.g.iter().last().unwrap().1.item);
    }
}
