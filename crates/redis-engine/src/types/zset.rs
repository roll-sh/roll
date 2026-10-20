//! The `zset` type, with Redis' two encodings and conversion rules:
//!
//! - `listpack`: a [`Pack`] of `member, score` pairs ordered by score, then member. Scores are
//!   stored as 8 raw bytes (`f64`, little endian), and `-0` is stored as `0`, like Redis'
//!   listpack (which stores integral scores as integers).
//! - `skiplist`: a skiplist with spans ported from Redis' `zskiplist` (O(log n) insert, delete,
//!   update, rank, rank lookup and range search) whose nodes live in an arena, plus an intrusive
//!   chained hash index member → node with incremental rehashing and Redis-exact SCAN cursors
//!   (the same reverse-binary walk as [`crate::dict::Dict`]). Each member is stored once, in its
//!   node: about 64 bytes per element plus 4–8 bytes of hash buckets.
//!
//! Redis converts listpack → skiplist when an insert would exceed `zset-max-listpack-entries` or
//! a member is longer than `zset-max-listpack-value`, and never converts back, except for the
//! results of STORE commands ([`ZSet::from_sorted`]).

use crate::config::Config;
use crate::pack::{Pack, PackIter};
use crate::types::list::lp_entry_size;
use crate::util::Rng;
use crate::value::Str;
use foldhash::fast::RandomState;
use std::cell::Cell;
use std::hash::BuildHasher;

/// `zset-max-listpack-entries` / `zset-max-listpack-value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub entries: usize,
    pub value: usize,
}

impl Limits {
    pub fn from_cfg(cfg: &Config) -> Self {
        Self { entries: cfg.zset_max_listpack_entries, value: cfg.zset_max_listpack_value }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self { entries: 128, value: 64 }
    }
}

/// ZADD options (`zsetAdd` input flags).
#[derive(Clone, Copy, Debug, Default)]
pub struct AddFlags {
    pub nx: bool,
    pub xx: bool,
    pub gt: bool,
    pub lt: bool,
    pub incr: bool,
}

/// What [`ZSet::add`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddOut {
    Added,
    Updated,
    /// The element exists and its score was already the requested one.
    Same,
    /// Nothing done because of NX / XX / GT / LT.
    Nop,
}

/// [`ZSet::add`] failed: the score is (or the increment produces) NaN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NanScore;

/// A score interval (`zrangespec`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreRange {
    pub min: f64,
    pub max: f64,
    pub minex: bool,
    pub maxex: bool,
}

impl ScoreRange {
    #[inline]
    pub fn gte_min(&self, v: f64) -> bool {
        if self.minex { v > self.min } else { v >= self.min }
    }
    #[inline]
    pub fn lte_max(&self, v: f64) -> bool {
        if self.maxex { v < self.max } else { v <= self.max }
    }
    /// False for ranges that can never match anything.
    pub fn nonempty(&self) -> bool {
        !(self.min > self.max || (self.min == self.max && (self.minex || self.maxex)))
    }
}

/// One end of a lex range: `-`, `+` or a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LexBound {
    Min,
    Max,
    Str(Vec<u8>),
}

/// A lex interval (`zlexrangespec`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LexRange {
    pub min: LexBound,
    pub max: LexBound,
    pub minex: bool,
    pub maxex: bool,
}

/// `sdscmplex`: `-` sorts before everything, `+` after everything.
fn cmplex(a: &LexBound, b: &LexBound) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (LexBound::Min, LexBound::Min) | (LexBound::Max, LexBound::Max) => Equal,
        (LexBound::Min, _) | (_, LexBound::Max) => Less,
        (LexBound::Max, _) | (_, LexBound::Min) => Greater,
        (LexBound::Str(x), LexBound::Str(y)) => x.as_slice().cmp(y.as_slice()),
    }
}

fn cmp_member(m: &[u8], b: &LexBound) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match b {
        LexBound::Min => Greater,
        LexBound::Max => Less,
        LexBound::Str(s) => m.cmp(s.as_slice()),
    }
}

impl LexRange {
    #[inline]
    pub fn gte_min(&self, m: &[u8]) -> bool {
        let c = cmp_member(m, &self.min);
        if self.minex { c.is_gt() } else { c.is_ge() }
    }
    #[inline]
    pub fn lte_max(&self, m: &[u8]) -> bool {
        let c = cmp_member(m, &self.max);
        if self.maxex { c.is_lt() } else { c.is_le() }
    }
    pub fn nonempty(&self) -> bool {
        let c = cmplex(&self.min, &self.max);
        !(c.is_gt() || (c.is_eq() && (self.minex || self.maxex)))
    }
}

// ---------------------------------------------------------------------------------------------
// listpack helpers

#[inline]
fn enc_score(s: f64) -> [u8; 8] {
    // Listpack stores integral scores as integers, which loses the sign of zero.
    let s = if s == 0.0 { 0.0 } else { s };
    s.to_le_bytes()
}

#[inline]
fn dec_score(b: &[u8]) -> f64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    f64::from_le_bytes(a)
}

fn lp_find(p: &Pack, m: &[u8]) -> Option<(usize, f64)> {
    p.iter_pairs().enumerate().find(|(_, (k, _))| *k == m).map(|(i, (_, s))| (i, dec_score(s)))
}

/// `zzlInsert`: insert keeping (score, member) order.
fn lp_insert(p: &mut Pack, m: &[u8], score: f64) {
    let mut pos = p.pairs();
    for (i, (k, s)) in p.iter_pairs().enumerate() {
        let s = dec_score(s);
        if s > score || (s == score && k > m) {
            pos = i;
            break;
        }
    }
    p.pair_insert(pos, m, &enc_score(score));
}

// ---------------------------------------------------------------------------------------------
// skiplist

const NIL: u32 = u32::MAX;
const HEAD: u32 = u32::MAX - 1;
const MAX_LEVEL: usize = 32;
const MIN_BUCKETS: usize = 4;

#[derive(Clone, Copy, Debug)]
struct Level {
    fwd: u32,
    span: u32,
}

const EMPTY_LEVEL: Level = Level { fwd: NIL, span: 0 };

/// A level-2+ (or head) link: also where the forward node's own level-2+ links are, so a search
/// step can fetch the next node and its links together instead of one after the other.
#[derive(Clone, Copy, Debug)]
struct Link {
    fwd: u32,
    span: u32,
    /// `up` of `fwd` (0 when it is NIL).
    fup: u32,
}

const EMPTY_LINK: Link = Link { fwd: NIL, span: 0, fup: 0 };

#[derive(Clone, Debug)]
struct Node {
    score: f64,
    ele: Str,
    back: u32,
    /// Level-0 forward link (its span is always 1).
    next: u32,
    /// Hash chain.
    hnext: u32,
    /// Level 1 (if `height > 1`): inline, as the busiest one after level 0, so a step there reads
    /// the node it is on rather than another cache line.
    l1: Level,
    /// Offset in `SkipList::levels` of levels `2..height`.
    up: u32,
    /// 0 marks a free slot.
    height: u8,
}

thread_local! {
    static LEVEL_RNG: Cell<u64> = Cell::new((crate::util::now_us() as u64) ^ 0x9E37_79B9_7F4A_7C15 | 1);
}

/// `zslCreateNode`: one allocation for the node, its levels and the member as an sds.
fn node_alloc(height: usize, len: usize) -> usize {
    crate::client::jemalloc_size(16 + height * 16 + crate::object::sds_req(len))
}

/// `zslRandomLevel`: P = 1/4, at most 32.
fn random_level() -> usize {
    LEVEL_RNG.with(|c| {
        let mut x = c.get();
        let mut level = 1;
        loop {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if (x & 0xffff) < 0x4000 && level < MAX_LEVEL {
                level += 1;
            } else {
                break;
            }
        }
        c.set(x);
        level
    })
}

/// A search's place ([`SkipList::descend`]).
struct Descent {
    i: usize,
    x: u32,
    xup: u32,
    r: u32,
    score: f64,
    done: bool,
}

pub struct SkipList {
    nodes: Vec<Node>,
    /// Free node slots, chained through `next`.
    free: u32,
    levels: Vec<Link>,
    /// Free level blocks by size (index = extra levels - 1), chained through the first `fwd`.
    free_levels: [u32; MAX_LEVEL - 1],
    head: [Link; MAX_LEVEL],
    tail: u32,
    len: usize,
    level: usize,
    // Hash index: bucket heads; `t1` is non-empty while rehashing from `t0` into it.
    t0: Vec<u32>,
    t1: Vec<u32>,
    rehash_idx: usize,
    hasher: RandomState,
    /// Heap bytes of spilled members.
    heap: usize,
    /// What Redis allocates for the nodes (each with its levels and member).
    node_alloc: usize,
}

/// A copy (COPY, RESTORE of a cached value…): the members' clones may hold other capacities
/// than the originals, so the heap count is taken again (freeing a node subtracts its own).
impl Clone for SkipList {
    fn clone(&self) -> Self {
        let nodes = self.nodes.clone();
        let heap = nodes.iter().filter(|n| n.ele.spilled()).map(|n| n.ele.capacity()).sum();
        SkipList {
            nodes,
            free: self.free,
            levels: self.levels.clone(),
            free_levels: self.free_levels,
            head: self.head,
            tail: self.tail,
            len: self.len,
            level: self.level,
            t0: self.t0.clone(),
            t1: self.t1.clone(),
            rehash_idx: self.rehash_idx,
            hasher: self.hasher.clone(),
            heap,
            node_alloc: self.node_alloc,
        }
    }
}

impl std::fmt::Debug for SkipList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut l = f.debug_list();
        let mut x = self.head[0].fwd;
        while x != NIL {
            let n = &self.nodes[x as usize];
            l.entry(&(String::from_utf8_lossy(&n.ele), n.score));
            x = n.next;
        }
        l.finish()
    }
}

impl SkipList {
    fn new(cap: usize, hasher: RandomState) -> Self {
        let mut sl = SkipList {
            nodes: Vec::with_capacity(cap),
            free: NIL,
            levels: Vec::new(),
            free_levels: [NIL; MAX_LEVEL - 1],
            head: [EMPTY_LINK; MAX_LEVEL],
            tail: NIL,
            len: 0,
            level: 1,
            t0: Vec::new(),
            t1: Vec::new(),
            rehash_idx: 0,
            hasher,
            heap: 0,
            node_alloc: 0,
        };
        if cap > 0 {
            sl.t0 = vec![NIL; cap.next_power_of_two().max(MIN_BUCKETS)];
        }
        sl
    }

    /// Build from elements already in (score, member) order, in O(n).
    fn from_sorted<'a>(items: impl Iterator<Item = (&'a [u8], f64)>, cap: usize, hasher: RandomState) -> Self {
        let mut sl = SkipList::new(cap, hasher);
        let mut last = [HEAD; MAX_LEVEL];
        let mut last_rank = [0u32; MAX_LEVEL];
        for (ele, score) in items {
            let h = random_level();
            let n = sl.alloc(score, ele, h);
            sl.index_insert(n);
            if h > sl.level {
                sl.level = h;
            }
            let rank = sl.len as u32 + 1;
            for i in 0..h {
                let p = last[i];
                sl.set_fwd(p, i, n);
                sl.set_span(p, i, rank - last_rank[i]);
                last[i] = n;
                last_rank[i] = rank;
            }
            sl.nodes[n as usize].back = sl.tail;
            sl.tail = n;
            sl.len += 1;
        }
        // Spans of links to the end count the remaining nodes, like Redis keeps them.
        for i in 1..sl.level {
            let s = sl.len as u32 - last_rank[i];
            sl.set_span(last[i], i, s);
        }
        sl
    }

    // ---- node / level access ----

    #[inline]
    fn fwd(&self, x: u32, i: usize) -> u32 {
        if x == HEAD {
            self.head[i].fwd
        } else if i == 0 {
            self.nodes[x as usize].next
        } else if i == 1 {
            self.nodes[x as usize].l1.fwd
        } else {
            self.levels[self.nodes[x as usize].up as usize + i - 2].fwd
        }
    }

    #[inline]
    fn set_fwd(&mut self, x: u32, i: usize, v: u32) {
        let fup = if v == NIL || v == HEAD { 0 } else { self.nodes[v as usize].up };
        if x == HEAD {
            self.head[i].fwd = v;
            self.head[i].fup = fup;
        } else if i == 0 {
            self.nodes[x as usize].next = v;
        } else if i == 1 {
            self.nodes[x as usize].l1.fwd = v;
        } else {
            let up = self.nodes[x as usize].up as usize;
            let l = &mut self.levels[up + i - 2];
            l.fwd = v;
            l.fup = fup;
        }
    }

    #[inline]
    fn span(&self, x: u32, i: usize) -> u32 {
        if i == 0 {
            1
        } else if x == HEAD {
            self.head[i].span
        } else if i == 1 {
            self.nodes[x as usize].l1.span
        } else {
            self.levels[self.nodes[x as usize].up as usize + i - 2].span
        }
    }

    #[inline]
    fn set_span(&mut self, x: u32, i: usize, v: u32) {
        if i == 0 {
            return;
        }
        if x == HEAD {
            self.head[i].span = v;
        } else if i == 1 {
            self.nodes[x as usize].l1.span = v;
        } else {
            let up = self.nodes[x as usize].up as usize;
            self.levels[up + i - 2].span = v;
        }
    }

    #[inline]
    fn node(&self, x: u32) -> &Node {
        &self.nodes[x as usize]
    }

    /// Node `a` sorts before node `b`.
    #[inline]
    fn before(&self, a: u32, b: u32) -> bool {
        let (na, nb) = (self.node(a), self.node(b));
        na.score < nb.score || (na.score == nb.score && na.ele.as_slice() < nb.ele.as_slice())
    }

    fn alloc(&mut self, score: f64, ele: &[u8], height: usize) -> u32 {
        let extra = height.saturating_sub(2);
        let up = if extra == 0 { 0 } else { self.alloc_levels(extra) };
        let ele = Str::from_slice(ele);
        if ele.spilled() {
            self.heap += ele.capacity();
        }
        self.node_alloc += node_alloc(height, ele.len());
        let node = Node { score, ele, back: NIL, next: NIL, hnext: NIL, l1: EMPTY_LEVEL, up, height: height as u8 };
        if self.free != NIL {
            let i = self.free;
            self.free = self.nodes[i as usize].next;
            self.nodes[i as usize] = node;
            i
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as u32
        }
    }

    fn alloc_levels(&mut self, extra: usize) -> u32 {
        let f = self.free_levels[extra - 1];
        if f != NIL {
            self.free_levels[extra - 1] = self.levels[f as usize].fwd;
            for l in &mut self.levels[f as usize..f as usize + extra] {
                *l = EMPTY_LINK;
            }
            f
        } else {
            let at = self.levels.len();
            self.levels.resize(at + extra, EMPTY_LINK);
            at as u32
        }
    }

    fn free_node(&mut self, x: u32) {
        let n = &mut self.nodes[x as usize];
        if n.ele.spilled() {
            self.heap -= n.ele.capacity();
        }
        self.node_alloc -= node_alloc(n.height as usize, n.ele.len());
        n.ele = Str::new();
        let extra = (n.height as usize).saturating_sub(2);
        let up = n.up;
        n.height = 0;
        n.next = self.free;
        self.free = x;
        if extra > 0 {
            self.levels[up as usize].fwd = self.free_levels[extra - 1];
            self.free_levels[extra - 1] = up;
        }
    }

    // ---- skiplist algorithms (t_zset.c) ----

    /// Node `f` sorts before `(score, ele)`.
    #[inline]
    fn sorts_before(&self, f: u32, score: f64, ele: &[u8]) -> bool {
        let nf = self.node(f);
        nf.score < score || (nf.score == score && nf.ele.as_slice() < ele)
    }

    /// The last node before `(score, ele)` at every level, and their ranks. Each step reads the
    /// link it follows for where the next node's own links are, so their loads overlap.
    fn insert_path(&self, score: f64, ele: &[u8]) -> ([u32; MAX_LEVEL], [u32; MAX_LEVEL]) {
        let mut update = [HEAD; MAX_LEVEL];
        let mut rank = [0u32; MAX_LEVEL];
        let mut d = Descent { i: self.level - 1, x: HEAD, xup: 0, r: 0, score, done: false };
        while !d.done {
            self.descend(&mut d, ele, &mut update, &mut rank);
        }
        (update, rank)
    }
    /// `zslInsertNode`: link an allocated node at its position.
    fn link(&mut self, n: u32) {
        let nd = self.node(n);
        let (update, rank) = self.insert_path(nd.score, nd.ele.as_slice());
        self.link_at(n, update, rank);
    }

    /// Link node `n` after `update` (the last node before it at every level, of these ranks).
    fn link_at(&mut self, n: u32, mut update: [u32; MAX_LEVEL], mut rank: [u32; MAX_LEVEL]) {
        let height = self.node(n).height as usize;
        if height > self.level {
            for i in self.level..height {
                rank[i] = 0;
                update[i] = HEAD;
                self.head[i].span = self.len as u32;
            }
            self.level = height;
        }
        for i in 0..height {
            let u = update[i];
            let uf = self.fwd(u, i);
            self.set_fwd(n, i, uf);
            self.set_fwd(u, i, n);
            let us = self.span(u, i);
            self.set_span(n, i, us.wrapping_sub(rank[0] - rank[i]));
            self.set_span(u, i, rank[0] - rank[i] + 1);
        }
        for (i, &u) in update.iter().enumerate().take(self.level).skip(height) {
            let s = self.span(u, i);
            self.set_span(u, i, s.wrapping_add(1));
        }
        self.nodes[n as usize].back = if update[0] == HEAD { NIL } else { update[0] };
        let nf = self.node(n).next;
        if nf != NIL {
            self.nodes[nf as usize].back = n;
        } else {
            self.tail = n;
        }
        self.len += 1;
    }

    /// The `update` vector for node `x` (the last node before it at every level).
    fn path_to(&self, x: u32) -> [u32; MAX_LEVEL] {
        let n = self.node(x);
        let (update, _) = self.insert_path(n.score, n.ele.as_slice());
        debug_assert_eq!(self.fwd(update[0], 0), x);
        update
    }

    /// `zslUnlinkNode`.
    fn unlink(&mut self, x: u32, update: &[u32; MAX_LEVEL]) {
        for (i, &u) in update.iter().enumerate().take(self.level) {
            if self.fwd(u, i) == x {
                let s = self.span(u, i).wrapping_add(self.span(x, i)).wrapping_sub(1);
                self.set_span(u, i, s);
                let xf = self.fwd(x, i);
                self.set_fwd(u, i, xf);
            } else {
                let s = self.span(u, i).wrapping_sub(1);
                self.set_span(u, i, s);
            }
        }
        let (next, back) = (self.node(x).next, self.node(x).back);
        if next != NIL {
            self.nodes[next as usize].back = back;
        } else {
            self.tail = back;
        }
        while self.level > 1 && self.head[self.level - 1].fwd == NIL {
            self.head[self.level - 1].span = 0;
            self.level -= 1;
        }
        self.len -= 1;
    }

    fn insert(&mut self, score: f64, ele: &[u8]) -> u32 {
        let n = self.alloc(score, ele, random_level());
        self.index_insert(n);
        self.link(n);
        n
    }

    fn delete(&mut self, x: u32) {
        let update = self.path_to(x);
        self.unlink(x, &update);
        self.index_remove(x);
        self.free_node(x);
    }

    /// `zslUpdateScore`. Where Redis searches the list twice (for the node, then for its new
    /// place), both searches run interleaved, one step each in turn: their pointer chases are
    /// independent, so their cache misses overlap.
    fn update_score(&mut self, x: u32, score: f64) {
        let n = self.node(x);
        if (n.back == NIL || self.node(n.back).score < score) && (n.next == NIL || self.node(n.next).score > score) {
            self.nodes[x as usize].score = score;
            return;
        }
        let (old, new, rank) = self.move_paths(x, score);
        self.unlink(x, &old);
        self.nodes[x as usize].score = score;
        self.link_at(x, new, rank);
    }

    /// For moving node `x` to `score`: the `update` vector to unlink it, and the `update` vector
    /// and ranks of its new place as they will be once it is unlinked.
    fn move_paths(&self, x: u32, score: f64) -> ([u32; MAX_LEVEL], [u32; MAX_LEVEL], [u32; MAX_LEVEL]) {
        let ele = self.node(x).ele.as_slice();
        let (mut uo, mut ro) = ([HEAD; MAX_LEVEL], [0u32; MAX_LEVEL]);
        let (mut un, mut rn) = ([HEAD; MAX_LEVEL], [0u32; MAX_LEVEL]);
        let top = self.level - 1;
        let mut a = Descent { i: top, x: HEAD, xup: 0, r: 0, score: self.node(x).score, done: false };
        let mut b = Descent { i: top, x: HEAD, xup: 0, r: 0, score, done: false };
        while !(a.done && b.done) {
            if !a.done {
                self.descend(&mut a, ele, &mut uo, &mut ro);
            }
            if !b.done {
                self.descend(&mut b, ele, &mut un, &mut rn);
            }
        }
        debug_assert_eq!(self.fwd(uo[0], 0), x);
        // The new place was searched with `x` still linked: where `x` is the last node before
        // it, its own predecessor takes its place; past `x`, ranks lose one.
        let rank_x = ro[0] + 1;
        for i in 0..self.level {
            if un[i] == x {
                un[i] = uo[i];
                rn[i] = ro[i];
            } else if rn[i] > rank_x {
                rn[i] -= 1;
            }
        }
        (uo, un, rn)
    }

    /// One step of a search, resumable: follow the link at the current level, or record the
    /// level's last node and go down. (Branchy on purpose: a predicted step lets the next load
    /// start before the comparison resolves; selects would wait for it.)
    #[inline(always)]
    fn descend(&self, d: &mut Descent, ele: &[u8], update: &mut [u32; MAX_LEVEL], rank: &mut [u32; MAX_LEVEL]) {
        let (x, i) = (d.x, d.i);
        // `fup` only matters on levels 2 and up, which only levels 2 and up lead to.
        let (f, span, fup) = if i == 0 {
            (if x == HEAD { self.head[0].fwd } else { self.nodes[x as usize].next }, 1, 0)
        } else if x == HEAD {
            let l = &self.head[i];
            (l.fwd, l.span, l.fup)
        } else if i == 1 {
            let l = &self.nodes[x as usize].l1;
            (l.fwd, l.span, 0)
        } else {
            let l = &self.levels[d.xup as usize + i - 2];
            (l.fwd, l.span, l.fup)
        };
        if f != NIL && self.sorts_before(f, d.score, ele) {
            d.r += span;
            d.x = f;
            d.xup = fup;
        } else {
            update[i] = x;
            rank[i] = d.r;
            if i == 0 {
                d.done = true;
            } else {
                d.i -= 1;
            }
        }
    }
    /// 1-based rank of node `x`.
    fn rank_of(&self, x: u32) -> usize {
        let mut rank = 0usize;
        let mut y = HEAD;
        for i in (0..self.level).rev() {
            loop {
                let f = self.fwd(y, i);
                if f != NIL && (f == x || self.before(f, x)) {
                    rank += self.span(y, i) as usize;
                    y = f;
                } else {
                    break;
                }
            }
            if y == x {
                return rank;
            }
        }
        rank
    }

    /// Node at 1-based `rank`.
    fn by_rank(&self, rank: usize) -> u32 {
        let mut t = 0usize;
        let mut x = HEAD;
        for i in (0..self.level).rev() {
            loop {
                let f = self.fwd(x, i);
                if f != NIL && t + self.span(x, i) as usize <= rank {
                    t += self.span(x, i) as usize;
                    x = f;
                } else {
                    break;
                }
            }
            if t == rank {
                return x;
            }
        }
        NIL
    }

    /// Number of leading nodes for which `pred` holds (assuming it holds for a prefix).
    fn lead(&self, pred: impl Fn(f64, &[u8]) -> bool) -> usize {
        let mut x = HEAD;
        let mut r = 0usize;
        for i in (0..self.level).rev() {
            loop {
                let f = self.fwd(x, i);
                if f != NIL && {
                    let n = self.node(f);
                    pred(n.score, &n.ele)
                } {
                    r += self.span(x, i) as usize;
                    x = f;
                } else {
                    break;
                }
            }
        }
        r
    }

    /// Delete the run of nodes starting after the leading `!gte_min` prefix while `lte_max`
    /// holds (`zslDeleteRangeByScore` / `ByLex`).
    fn delete_run(&mut self, gte_min: impl Fn(f64, &[u8]) -> bool, lte_max: impl Fn(f64, &[u8]) -> bool) -> usize {
        let mut update = [HEAD; MAX_LEVEL];
        let mut x = HEAD;
        for i in (0..self.level).rev() {
            loop {
                let f = self.fwd(x, i);
                if f != NIL && {
                    let n = self.node(f);
                    !gte_min(n.score, &n.ele)
                } {
                    x = f;
                } else {
                    break;
                }
            }
            update[i] = x;
        }
        let mut x = self.fwd(x, 0);
        let mut removed = 0;
        while x != NIL && {
            let n = self.node(x);
            lte_max(n.score, &n.ele)
        } {
            let next = self.node(x).next;
            self.unlink(x, &update);
            self.index_remove(x);
            self.free_node(x);
            removed += 1;
            x = next;
        }
        removed
    }

    /// `zslDeleteRangeByRank` (1-based, inclusive).
    fn delete_ranks(&mut self, start: usize, end: usize) -> usize {
        let mut update = [HEAD; MAX_LEVEL];
        let mut traversed = 0usize;
        let mut x = HEAD;
        for i in (0..self.level).rev() {
            loop {
                let f = self.fwd(x, i);
                if f != NIL && traversed + (self.span(x, i) as usize) < start {
                    traversed += self.span(x, i) as usize;
                    x = f;
                } else {
                    break;
                }
            }
            update[i] = x;
        }
        traversed += 1;
        let mut x = self.fwd(x, 0);
        let mut removed = 0;
        while x != NIL && traversed <= end {
            let next = self.node(x).next;
            self.unlink(x, &update);
            self.index_remove(x);
            self.free_node(x);
            removed += 1;
            traversed += 1;
            x = next;
        }
        removed
    }

    // ---- hash index ----

    #[inline]
    fn hash(&self, ele: &[u8]) -> u64 {
        self.hasher.hash_one(ele)
    }

    #[inline]
    fn is_rehashing(&self) -> bool {
        !self.t1.is_empty()
    }

    /// The member index as `dictGetStats` sees it.
    /// `zsetAllocSize` of the skiplist encoding: the zset, the skiplist with its 32 level header
    /// and nodes, and the member dict.
    fn alloc_size(&self) -> usize {
        // `sizeof(zset)`, `zskiplist` and the header node, as the allocator sizes them.
        const ZSET: usize = 16;
        const ZSKIPLIST: usize = 40;
        let header = crate::client::jemalloc_size(16 + MAX_LEVEL * 16);
        ZSET + ZSKIPLIST + header + self.node_alloc + crate::object::dict_usage(self.len, self.t0.len() + self.t1.len())
    }

    fn index_stats(&self, full: bool) -> Vec<crate::dict::DictStats> {
        let chains = |t: &[u32]| {
            t.iter()
                .map(|&b| {
                    let (mut n, mut c) = (0, b);
                    while c != NIL {
                        n += 1;
                        c = self.node(c).hnext;
                    }
                    n
                })
                .collect::<Vec<usize>>()
        };
        let target: usize = chains(&self.t1).iter().sum();
        let stats = |htidx, t: &[u32], used| crate::dict::DictStats::new(htidx, t.len(), used, full.then(|| chains(t).into_iter()));
        let mut out = vec![stats(0, &self.t0, self.len - target)];
        if !self.t1.is_empty() {
            out.push(stats(1, &self.t1, target));
        }
        out
    }

    fn find(&self, ele: &[u8]) -> u32 {
        if self.len == 0 {
            return NIL;
        }
        let h = self.hash(ele) as usize;
        for t in [&self.t0, &self.t1] {
            if t.is_empty() {
                continue;
            }
            let mut c = t[h & (t.len() - 1)];
            while c != NIL {
                let n = self.node(c);
                if n.ele.as_slice() == ele {
                    return c;
                }
                c = n.hnext;
            }
        }
        NIL
    }

    /// Add node `x` to the index (before it is counted in `len`).
    fn index_insert(&mut self, x: u32) {
        self.rehash_step();
        if !self.is_rehashing() {
            if self.t0.is_empty() {
                self.t0 = vec![NIL; MIN_BUCKETS];
            } else if self.len >= self.t0.len() {
                self.start_resize((self.len + 1).next_power_of_two());
            }
        }
        let h = self.hash(&self.node(x).ele) as usize;
        let t = if self.is_rehashing() { &mut self.t1 } else { &mut self.t0 };
        let b = h & (t.len() - 1);
        self.nodes[x as usize].hnext = t[b];
        t[b] = x;
    }

    /// Remove node `x` from the index (after it was unlinked from the list).
    fn index_remove(&mut self, x: u32) {
        self.rehash_step();
        let h = self.hash(&self.node(x).ele) as usize;
        let SkipList { t0, t1, nodes, .. } = self;
        'tables: for t in [t0, t1] {
            if t.is_empty() {
                continue;
            }
            let b = h & (t.len() - 1);
            if t[b] == x {
                t[b] = nodes[x as usize].hnext;
                break;
            }
            let mut c = t[b];
            while c != NIL {
                let next = nodes[c as usize].hnext;
                if next == x {
                    nodes[c as usize].hnext = nodes[x as usize].hnext;
                    break 'tables;
                }
                c = next;
            }
        }
        self.shrink_if_needed();
    }

    fn start_resize(&mut self, size: usize) {
        if self.is_rehashing() || size == self.t0.len() {
            return;
        }
        self.t1 = vec![NIL; size];
        self.rehash_idx = 0;
    }

    fn shrink_if_needed(&mut self) {
        if self.is_rehashing() || self.t0.len() <= MIN_BUCKETS {
            return;
        }
        if self.len * 8 <= self.t0.len() {
            self.start_resize(self.len.next_power_of_two().max(MIN_BUCKETS));
        }
    }

    #[inline]
    fn rehash_step(&mut self) {
        if self.is_rehashing() {
            self.rehash(1);
        }
    }

    /// Move up to `n` buckets from `t0` to `t1`.
    fn rehash(&mut self, n: usize) {
        let mut empty_visits = n * 10;
        let mut n = n;
        while n > 0 && self.rehash_idx < self.t0.len() {
            n -= 1;
            while self.t0[self.rehash_idx] == NIL {
                self.rehash_idx += 1;
                if self.rehash_idx >= self.t0.len() {
                    break;
                }
                empty_visits -= 1;
                if empty_visits == 0 {
                    return;
                }
            }
            if self.rehash_idx >= self.t0.len() {
                break;
            }
            let mask = self.t1.len() - 1;
            let mut c = self.t0[self.rehash_idx];
            self.t0[self.rehash_idx] = NIL;
            while c != NIL {
                let next = self.node(c).hnext;
                let b = (self.hash(&self.node(c).ele) as usize) & mask;
                self.nodes[c as usize].hnext = self.t1[b];
                self.t1[b] = c;
                c = next;
            }
            self.rehash_idx += 1;
        }
        if self.rehash_idx >= self.t0.len() {
            self.t0 = std::mem::take(&mut self.t1);
            self.rehash_idx = 0;
            self.shrink_if_needed();
        }
    }

    /// One SCAN step (`dictScan`): emit the bucket(s) at `cursor`, return the next cursor.
    fn scan(&self, cursor: u64, f: &mut dyn FnMut(&[u8], f64)) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let emit = |t: &Vec<u32>, idx: u64, f: &mut dyn FnMut(&[u8], f64)| {
            let mut c = t[idx as usize];
            while c != NIL {
                let n = self.node(c);
                f(&n.ele, n.score);
                c = n.hnext;
            }
        };
        let mut v = cursor;
        if !self.is_rehashing() {
            let m0 = (self.t0.len() - 1) as u64;
            emit(&self.t0, v & m0, f);
            v |= !m0;
            v = v.reverse_bits().wrapping_add(1).reverse_bits();
        } else {
            let (small, large) = if self.t0.len() <= self.t1.len() { (&self.t0, &self.t1) } else { (&self.t1, &self.t0) };
            let m0 = (small.len() - 1) as u64;
            let m1 = (large.len() - 1) as u64;
            emit(small, v & m0, f);
            loop {
                emit(large, v & m1, f);
                v |= !m1;
                v = v.reverse_bits().wrapping_add(1).reverse_bits();
                if v & (m0 ^ m1) == 0 {
                    break;
                }
            }
        }
        v
    }

    fn mem(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.levels.capacity() * std::mem::size_of::<Link>()
            + (self.t0.capacity() + self.t1.capacity()) * 4
            + self.heap
    }

    fn iter_sorted(&self) -> impl Iterator<Item = (&[u8], f64)> {
        let mut x = self.head[0].fwd;
        std::iter::from_fn(move || {
            if x == NIL {
                return None;
            }
            let n = self.node(x);
            x = n.next;
            Some((n.ele.as_slice(), n.score))
        })
    }

    /// Reclaim arena slots after many deletions (keeps the hash seed, so SCAN cursors stay
    /// valid: this is just another table resize for them).
    fn maybe_compact(&mut self) {
        let slots = self.nodes.len();
        if slots > 256 && self.len * 4 < slots {
            let rebuilt = SkipList::from_sorted(self.iter_sorted(), self.len, self.hasher.clone());
            *self = rebuilt;
        }
    }

    #[cfg(test)]
    fn check(&self) {
        // Order, back links, spans and the index agree with a plain walk.
        let mut x = self.head[0].fwd;
        let mut prev = NIL;
        let mut n = 0;
        while x != NIL {
            n += 1;
            assert_eq!(self.node(x).back, prev);
            if prev != NIL {
                assert!(self.before(prev, x));
            }
            assert_eq!(self.find(&self.node(x).ele), x);
            assert_eq!(self.rank_of(x), n);
            assert_eq!(self.by_rank(n), x);
            prev = x;
            x = self.node(x).next;
        }
        assert_eq!(n, self.len);
        assert_eq!(self.tail, prev);
        for i in 1..self.level {
            let mut x = HEAD;
            let mut rank = 0usize;
            while self.fwd(x, i) != NIL {
                rank += self.span(x, i) as usize;
                // Links past level 1 know where their target's links are.
                let link = if x == HEAD { Some(self.head[i]) } else if i >= 2 { Some(self.levels[self.node(x).up as usize + i - 2]) } else { None };
                x = self.fwd(x, i);
                if let Some(l) = link {
                    assert_eq!(l.fup, self.node(x).up, "fup at level {i}");
                }
                if x != HEAD {
                    assert_eq!(self.rank_of(x), rank, "span at level {i}");
                }
            }
        }
        let indexed: usize = [&self.t0, &self.t1]
            .iter()
            .map(|t| {
                t.iter()
                    .map(|&b| {
                        let mut c = b;
                        let mut k = 0;
                        while c != NIL {
                            k += 1;
                            c = self.node(c).hnext;
                        }
                        k
                    })
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(indexed, self.len);
    }
}

// ---------------------------------------------------------------------------------------------
// ZSet

#[derive(Clone, Debug)]
enum Enc {
    Pack(Pack),
    Skip(Box<SkipList>),
}

#[derive(Clone, Debug)]
pub struct ZSet {
    enc: Enc,
}

impl Default for ZSet {
    fn default() -> Self {
        Self { enc: Enc::Pack(Pack::new()) }
    }
}

/// Iterator over `(member, score)` from some rank, forwards or backwards.
pub struct Iter<'a> {
    inner: IterInner<'a>,
}

enum IterInner<'a> {
    Pack(PackIter<'a>),
    PackRev(std::vec::IntoIter<(&'a [u8], f64)>),
    Skip { sl: &'a SkipList, x: u32, rev: bool },
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], f64);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            IterInner::Pack(it) => {
                let m = it.next()?;
                let s = it.next()?;
                Some((m, dec_score(s)))
            }
            IterInner::PackRev(it) => it.next(),
            IterInner::Skip { sl, x, rev } => {
                if *x == NIL || *x == HEAD {
                    return None;
                }
                let n = &sl.nodes[*x as usize];
                *x = if *rev { n.back } else { n.next };
                Some((n.ele.as_slice(), n.score))
            }
        }
    }
}

impl ZSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// `zsetTypeCreate`: listpack when the hints fit, else a skiplist presized for `size_hint`.
    pub fn with_hint(size_hint: usize, value_len_hint: usize, lim: Limits) -> Self {
        if size_hint <= lim.entries && value_len_hint <= lim.value {
            Self::new()
        } else {
            Self { enc: Enc::Skip(Box::new(SkipList::new(size_hint.min(1 << 24), RandomState::default()))) }
        }
    }

    /// A zset from elements in ascending (score, member) order, listpack-encoded when it fits
    /// (`zsetConvertToListpackIfNeeded`, used for STORE results).
    pub fn from_sorted<'a, I>(items: I, lim: Limits) -> Self
    where
        I: IntoIterator<Item = (&'a [u8], f64)>,
        I::IntoIter: Clone + ExactSizeIterator,
    {
        let it = items.into_iter();
        let n = it.len();
        let maxlen = it.clone().map(|(m, _)| m.len()).max().unwrap_or(0);
        if n <= lim.entries && maxlen <= lim.value {
            let mut p = Pack::new();
            for (m, s) in it {
                p.push_back(m);
                p.push_back(&enc_score(s));
            }
            Self { enc: Enc::Pack(p) }
        } else {
            Self { enc: Enc::Skip(Box::new(SkipList::from_sorted(it, n, RandomState::default()))) }
        }
    }

    pub fn len(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.pairs(),
            Enc::Skip(s) => s.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.mem(),
            Enc::Skip(s) => std::mem::size_of::<SkipList>() + s.mem(),
        }
    }

    /// `zsetAllocSize`: the listpack (scores as integers when integral), or the skiplist and dict.
    pub fn alloc_size(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => crate::object::lp_with(
                p.iter()
                    .enumerate()
                    .map(|(i, b)| {
                        if i % 2 == 0 {
                            lp_entry_size(b)
                        } else {
                            let s = dec_score(b);
                            match crate::rdb::double2ll(s) {
                                Some(n) => lp_entry_size(itoa::Buffer::new().format(n).as_bytes()),
                                None => lp_entry_size(redis_proto::fmt_double(s).as_bytes()),
                            }
                        }
                    })
                    .sum(),
            ),
            Enc::Skip(s) => s.alloc_size(),
        }
    }

    /// DEBUG HTSTATS-KEY: the member index's statistics (`None` for a listpack).
    pub fn dict_stats(&self, full: bool) -> Option<Vec<crate::dict::DictStats>> {
        match &self.enc {
            Enc::Skip(s) => Some(s.index_stats(full)),
            Enc::Pack(_) => None,
        }
    }

    pub fn encoding(&self) -> &'static str {
        match &self.enc {
            Enc::Pack(_) => "listpack",
            Enc::Skip(_) => "skiplist",
        }
    }

    pub fn is_listpack(&self) -> bool {
        matches!(self.enc, Enc::Pack(_))
    }

    // ---- reads ----

    pub fn score(&self, member: &[u8]) -> Option<f64> {
        match &self.enc {
            Enc::Pack(p) => lp_find(p, member).map(|(_, s)| s),
            Enc::Skip(sl) => {
                let x = sl.find(member);
                (x != NIL).then(|| sl.node(x).score)
            }
        }
    }

    pub fn contains(&self, member: &[u8]) -> bool {
        self.score(member).is_some()
    }

    /// 0-based ascending rank and score of `member`.
    pub fn rank(&self, member: &[u8]) -> Option<(usize, f64)> {
        match &self.enc {
            Enc::Pack(p) => lp_find(p, member),
            Enc::Skip(sl) => {
                let x = sl.find(member);
                (x != NIL).then(|| (sl.rank_of(x) - 1, sl.node(x).score))
            }
        }
    }

    /// Element at 0-based ascending `rank`.
    pub fn get(&self, rank: usize) -> Option<(&[u8], f64)> {
        if rank >= self.len() {
            return None;
        }
        match &self.enc {
            Enc::Pack(p) => p.iter_pairs().nth(rank).map(|(m, s)| (m, dec_score(s))),
            Enc::Skip(sl) => {
                let n = sl.node(sl.by_rank(rank + 1));
                Some((n.ele.as_slice(), n.score))
            }
        }
    }

    /// All elements in ascending order.
    pub fn iter(&self) -> Iter<'_> {
        self.iter_from(0, false)
    }

    /// Elements from 0-based ascending `rank`, going up (or down when `rev`).
    pub fn iter_from(&self, rank: usize, rev: bool) -> Iter<'_> {
        let inner = match &self.enc {
            Enc::Pack(p) => {
                if rev {
                    let mut v: Vec<(&[u8], f64)> =
                        p.iter_pairs().take(rank.saturating_add(1)).map(|(m, s)| (m, dec_score(s))).collect();
                    if rank >= p.pairs() {
                        v.clear();
                    }
                    v.reverse();
                    IterInner::PackRev(v.into_iter())
                } else {
                    let mut it = p.iter();
                    for _ in 0..rank.saturating_mul(2).min(p.len()) {
                        it.next();
                    }
                    IterInner::Pack(it)
                }
            }
            Enc::Skip(sl) => {
                let x = if rank >= sl.len {
                    NIL
                } else if rank == 0 {
                    sl.head[0].fwd
                } else if rank == sl.len - 1 {
                    sl.tail
                } else {
                    sl.by_rank(rank + 1)
                };
                IterInner::Skip { sl, x, rev }
            }
        };
        Iter { inner }
    }

    /// Number of leading elements for which `pred` holds. For the listpack this is a linear scan
    /// (`zzlFirstInRange`); for the skiplist a descent that assumes `pred` holds on a prefix.
    pub fn lead(&self, pred: impl Fn(f64, &[u8]) -> bool) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.iter_pairs().take_while(|(m, s)| pred(dec_score(s), m)).count(),
            Enc::Skip(sl) => sl.lead(pred),
        }
    }

    /// Index of the last element for which `pred` holds (`zzlLastInRange` scans from the tail).
    pub fn last_true(&self, pred: impl Fn(f64, &[u8]) -> bool) -> Option<usize> {
        match &self.enc {
            Enc::Pack(p) => {
                let mut last = None;
                for (i, (m, s)) in p.iter_pairs().enumerate() {
                    if pred(dec_score(s), m) {
                        last = Some(i);
                    }
                }
                last
            }
            Enc::Skip(sl) => sl.lead(pred).checked_sub(1),
        }
    }

    /// `zsl/zzlIsInRange`: the first element is `<= max` and the last `>= min`.
    fn touches(&self, gte: &impl Fn(f64, &[u8]) -> bool, lte: &impl Fn(f64, &[u8]) -> bool) -> bool {
        let n = self.len();
        if n == 0 {
            return false;
        }
        let (lm, ls) = self.get(n - 1).unwrap();
        if !gte(ls, lm) {
            return false;
        }
        let (fm, fs) = self.get(0).unwrap();
        lte(fs, fm)
    }

    /// Rank of the first element in range (`zzlFirstInRange` / `zslNthInRange(0)`).
    pub fn first_in(
        &self,
        nonempty: bool,
        gte: impl Fn(f64, &[u8]) -> bool,
        lte: impl Fn(f64, &[u8]) -> bool,
    ) -> Option<usize> {
        if !nonempty || !self.touches(&gte, &lte) {
            return None;
        }
        let first = self.lead(|s, m| !gte(s, m));
        let (m, s) = self.get(first)?;
        lte(s, m).then_some(first)
    }

    /// Rank of the last element in range (`zzlLastInRange` / `zslNthInRange(-1)`).
    pub fn last_in(
        &self,
        nonempty: bool,
        gte: impl Fn(f64, &[u8]) -> bool,
        lte: impl Fn(f64, &[u8]) -> bool,
    ) -> Option<usize> {
        if !nonempty || !self.touches(&gte, &lte) {
            return None;
        }
        let last = self.last_true(&lte)?;
        let (m, s) = self.get(last)?;
        gte(s, m).then_some(last)
    }

    /// ZCOUNT / ZLEXCOUNT: O(log n) for the skiplist.
    pub fn count_in(&self, nonempty: bool, gte: impl Fn(f64, &[u8]) -> bool, lte: impl Fn(f64, &[u8]) -> bool) -> usize {
        let Some(first) = self.first_in(nonempty, &gte, &lte) else { return 0 };
        match &self.enc {
            Enc::Pack(_) => self.iter_from(first, false).take_while(|(m, s)| lte(*s, m)).count(),
            Enc::Skip(_) => {
                let len = self.len();
                let mut count = len - first;
                if let Some(last) = self.last_in(nonempty, &gte, &lte) {
                    count -= len - (last + 1);
                }
                count
            }
        }
    }

    /// A random element, uniformly.
    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], f64)> {
        let n = self.len();
        if n == 0 { None } else { self.get(rng.below(n)) }
    }

    /// One ZSCAN step: `f(member, score)` for each element of the visited bucket(s). The listpack
    /// returns everything at once (cursor 0).
    pub fn scan(&self, cursor: u64, count: usize, mut f: impl FnMut(&[u8], f64)) -> u64 {
        match &self.enc {
            Enc::Pack(p) => {
                for (m, s) in p.iter_pairs() {
                    f(m, dec_score(s));
                }
                0
            }
            Enc::Skip(sl) => {
                let mut c = cursor;
                let mut found = 0usize;
                let mut iterations = count.saturating_mul(10);
                loop {
                    c = sl.scan(c, &mut |m, s| {
                        found += 1;
                        f(m, s)
                    });
                    if c == 0 || iterations == 0 || found >= count {
                        break;
                    }
                    iterations -= 1;
                }
                c
            }
        }
    }

    // ---- writes ----

    fn convert_to_skiplist(&mut self, cap: usize) {
        if let Enc::Pack(p) = &self.enc {
            let sl = SkipList::from_sorted(p.iter_pairs().map(|(m, s)| (m, dec_score(s))), cap, RandomState::default());
            self.enc = Enc::Skip(Box::new(sl));
        }
    }

    /// `zsetTypeMaybeConvert`: an existing listpack about to receive `size_hint` elements.
    pub fn maybe_convert(&mut self, size_hint: usize, lim: Limits) {
        if self.is_listpack() && size_hint > lim.entries {
            self.convert_to_skiplist(size_hint);
        }
    }

    /// `zsetAdd`. Returns what happened and the resulting score, or [`NanScore`] when the score
    /// is (or the increment produces) NaN.
    pub fn add(&mut self, member: &[u8], score: f64, f: AddFlags, lim: Limits) -> Result<(AddOut, f64), NanScore> {
        if score.is_nan() {
            return Err(NanScore);
        }
        let decide = |cur: f64| -> Result<Option<f64>, NanScore> {
            if f.nx {
                return Ok(None);
            }
            let mut s = score;
            if f.incr {
                s += cur;
                if s.is_nan() {
                    return Err(NanScore);
                }
            }
            if (f.lt && s >= cur) || (f.gt && s <= cur) {
                return Ok(None);
            }
            Ok(Some(s))
        };
        if let Enc::Pack(p) = &mut self.enc {
            if let Some((i, cur)) = lp_find(p, member) {
                let Some(s) = decide(cur)? else { return Ok((AddOut::Nop, cur)) };
                if s != cur {
                    p.pair_remove_at(i);
                    lp_insert(p, member, s);
                    return Ok((AddOut::Updated, s));
                }
                return Ok((AddOut::Same, s));
            } else if f.xx {
                return Ok((AddOut::Nop, 0.0));
            } else if p.pairs() + 1 > lim.entries || member.len() > lim.value {
                let cap = p.pairs() + 1;
                self.convert_to_skiplist(cap);
            } else {
                lp_insert(p, member, score);
                return Ok((AddOut::Added, score));
            }
        }
        let Enc::Skip(sl) = &mut self.enc else { unreachable!() };
        let x = sl.find(member);
        if x != NIL {
            let cur = sl.node(x).score;
            let Some(s) = decide(cur)? else { return Ok((AddOut::Nop, cur)) };
            if s != cur {
                sl.update_score(x, s);
                return Ok((AddOut::Updated, s));
            }
            Ok((AddOut::Same, s))
        } else if f.xx {
            Ok((AddOut::Nop, 0.0))
        } else {
            sl.insert(score, member);
            Ok((AddOut::Added, score))
        }
    }

    /// Set `member`'s score (insert or update). Returns true if it was added.
    pub fn insert(&mut self, member: &[u8], score: f64, lim: Limits) -> bool {
        matches!(self.add(member, score, AddFlags::default(), lim), Ok((AddOut::Added, _)))
    }

    pub fn remove(&mut self, member: &[u8]) -> bool {
        match &mut self.enc {
            Enc::Pack(p) => match lp_find(p, member) {
                Some((i, _)) => {
                    p.pair_remove_at(i);
                    true
                }
                None => false,
            },
            Enc::Skip(sl) => {
                let x = sl.find(member);
                if x == NIL {
                    return false;
                }
                sl.delete(x);
                sl.maybe_compact();
                true
            }
        }
    }

    /// Remove and return the lowest (or highest when `max`) element.
    pub fn pop(&mut self, max: bool) -> Option<(Str, f64)> {
        match &mut self.enc {
            Enc::Pack(p) => {
                let n = p.pairs();
                if n == 0 {
                    return None;
                }
                let i = if max { n - 1 } else { 0 };
                let (m, s) = p.iter_pairs().nth(i).map(|(m, s)| (Str::from_slice(m), dec_score(s)))?;
                p.pair_remove_at(i);
                Some((m, s))
            }
            Enc::Skip(sl) => {
                let x = if max { sl.tail } else { sl.head[0].fwd };
                if x == NIL {
                    return None;
                }
                let out = (sl.node(x).ele.clone(), sl.node(x).score);
                sl.delete(x);
                sl.maybe_compact();
                Some(out)
            }
        }
    }

    /// Remove 0-based ranks `start..=end` (already clamped). Returns how many were removed.
    pub fn remove_ranks(&mut self, start: usize, end: usize) -> usize {
        let n = end + 1 - start;
        match &mut self.enc {
            Enc::Pack(p) => {
                p.remove_range(2 * start, 2 * n);
                n
            }
            Enc::Skip(sl) => {
                let r = sl.delete_ranks(start + 1, end + 1);
                sl.maybe_compact();
                r
            }
        }
    }

    /// ZREMRANGEBYSCORE / ZREMRANGEBYLEX: remove the run starting at the first element in range
    /// while it stays `<= max`.
    pub fn remove_in(
        &mut self,
        nonempty: bool,
        gte: impl Fn(f64, &[u8]) -> bool,
        lte: impl Fn(f64, &[u8]) -> bool,
    ) -> usize {
        if self.is_listpack() {
            let Some(first) = self.first_in(nonempty, &gte, &lte) else { return 0 };
            let k = self.iter_from(first, false).take_while(|(m, s)| lte(*s, m)).count();
            if let Enc::Pack(p) = &mut self.enc {
                p.remove_range(2 * first, 2 * k);
            }
            return k;
        }
        let Enc::Skip(sl) = &mut self.enc else { unreachable!() };
        let r = sl.delete_run(gte, lte);
        sl.maybe_compact();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// `cargo test --release -p redis-engine zset_add_speed -- --ignored --nocapture`: ZADD's
    /// workload in redis-benchmark (random scores for members of a 100k set).
    #[test]
    #[ignore]
    fn zset_add_speed() {
        let mut z = ZSet::new();
        let mut x = 88172645463325252u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let member = |n: u64| format!("element:{:012}", n % 100_000).into_bytes();
        // Built from random scores, as the server's set is (nodes not laid out in order).
        for n in 0..100_000 {
            let s = (next() % 100_000) as f64;
            z.add(&member(n), s, AddFlags::default(), Limits::default()).unwrap();
        }
        let ops: Vec<(Vec<u8>, f64)> = (0..4_000_000).map(|_| (member(next()), (next() % 100_000) as f64)).collect();
        let t = std::time::Instant::now();
        for (m, s) in &ops {
            z.add(m, *s, AddFlags::default(), Limits::default()).unwrap();
        }
        let ns = t.elapsed().as_nanos() as f64 / ops.len() as f64;
        eprintln!("zadd: {ns:.0} ns/op, node {} bytes", std::mem::size_of::<Node>());
    }

    /// The same workload through `Engine::call` (dispatch included, no network).
    #[test]
    #[ignore]
    fn zadd_engine_speed() {
        use bytes::Bytes;
        let mut eng = crate::engine::Engine::standalone();
        let mut c = crate::client::Client::new(1);
        let mut x = 88172645463325252u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let cmd = |m: u64, s: u64| vec![Bytes::from_static(b"ZADD"), Bytes::from_static(b"myzset"), Bytes::from(format!("{:012}", s)), Bytes::from(format!("element:{:012}", m))];
        for n in 0..100_000 {
            let s = next() % 100_000;
            eng.call(&mut c, &cmd(n, s));
        }
        let ops: Vec<Vec<Bytes>> = (0..2_000_000).map(|_| cmd(next() % 100_000, next() % 100_000)).collect();
        let t = std::time::Instant::now();
        for a in &ops {
            eng.call(&mut c, a);
        }
        eprintln!("engine zadd: {:.0} ns/op", t.elapsed().as_nanos() as f64 / ops.len() as f64);
    }

    /// The naive model: members with scores, ordered by (score, member).
    fn model_sorted(m: &BTreeMap<Vec<u8>, f64>) -> Vec<(Vec<u8>, f64)> {
        let mut v: Vec<_> = m.iter().map(|(k, s)| (k.clone(), *s)).collect();
        v.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.cmp(&b.0)));
        v
    }

    fn contents(z: &ZSet) -> Vec<(Vec<u8>, f64)> {
        z.iter().map(|(m, s)| (m.to_vec(), s)).collect()
    }

    fn check(z: &ZSet, m: &BTreeMap<Vec<u8>, f64>) {
        let want = model_sorted(m);
        assert_eq!(z.len(), want.len());
        assert_eq!(contents(z), want);
        if let Enc::Skip(sl) = &z.enc {
            sl.check();
        }
        for (i, (k, s)) in want.iter().enumerate() {
            assert_eq!(z.rank(k), Some((i, *s)));
            assert_eq!(z.get(i), Some((k.as_slice(), *s)));
        }
        let rev: Vec<_> = if want.is_empty() {
            vec![]
        } else {
            z.iter_from(want.len() - 1, true).map(|(m, s)| (m.to_vec(), s)).collect()
        };
        let mut w = want.clone();
        w.reverse();
        assert_eq!(rev, w);
    }

    #[test]
    fn randomized_against_model() {
        for lim in [Limits { entries: 16, value: 64 }, Limits { entries: 0, value: 64 }, Limits::default()] {
            let mut rng = Rng::seeded(42 + lim.entries as u64);
            let mut z = ZSet::new();
            let mut m: BTreeMap<Vec<u8>, f64> = BTreeMap::new();
            for step in 0..6000 {
                let key = format!("m{}", rng.below(300)).into_bytes();
                let score = (rng.below(40) as f64) - 20.0;
                match rng.below(10) {
                    0..=4 => {
                        let added = z.insert(&key, score, lim);
                        assert_eq!(added, m.insert(key, score).is_none());
                    }
                    5 => {
                        assert_eq!(z.remove(&key), m.remove(&key).is_some());
                    }
                    6 => {
                        let r = z.add(&key, score, AddFlags { incr: true, ..Default::default() }, lim).unwrap();
                        let e = m.entry(key).or_insert(0.0);
                        if r.0 != AddOut::Added {
                            *e += score;
                        } else {
                            *e = score;
                        }
                        assert_eq!(r.1, *e);
                    }
                    7 => {
                        let want = model_sorted(&m);
                        let max = rng.below(2) == 1;
                        let got = z.pop(max);
                        let w = if max { want.last() } else { want.first() };
                        assert_eq!(got.as_ref().map(|(k, s)| (k.to_vec(), *s)), w.cloned());
                        if let Some((k, _)) = w {
                            m.remove(k);
                        }
                    }
                    8 => {
                        let lo = (rng.below(40) as f64) - 20.0;
                        let r = ScoreRange { min: lo, max: lo + 3.0, minex: rng.below(2) == 1, maxex: false };
                        let want = m.values().filter(|s| r.gte_min(**s) && r.lte_max(**s)).count();
                        assert_eq!(z.count_in(r.nonempty(), |s, _| r.gte_min(s), |s, _| r.lte_max(s)), want);
                        if step % 3 == 0 {
                            let n = z.remove_in(r.nonempty(), |s, _| r.gte_min(s), |s, _| r.lte_max(s));
                            assert_eq!(n, want);
                            m.retain(|_, s| !(r.gte_min(*s) && r.lte_max(*s)));
                        }
                    }
                    _ => {
                        if !m.is_empty() && step % 4 == 0 {
                            let n = m.len();
                            let a = rng.below(n);
                            let b = (a + rng.below(5)).min(n - 1);
                            let want = model_sorted(&m);
                            assert_eq!(z.remove_ranks(a, b), b - a + 1);
                            for (k, _) in &want[a..=b] {
                                m.remove(k);
                            }
                        }
                    }
                }
                if step % 97 == 0 {
                    check(&z, &m);
                }
            }
            check(&z, &m);
        }
    }

    /// Score changes that move members (the two interleaved searches of `update_score`), on a
    /// skiplist tall enough for every level case, against the model.
    #[test]
    fn moves_against_model() {
        let lim = Limits { entries: 0, value: 64 };
        let mut rng = Rng::seeded(7);
        let mut z = ZSet::new();
        let mut m: BTreeMap<Vec<u8>, f64> = BTreeMap::new();
        for i in 0..2000 {
            let key = format!("m{i}").into_bytes();
            let score = rng.below(1000) as f64;
            z.insert(&key, score, lim);
            m.insert(key, score);
        }
        for step in 0..20_000 {
            let key = format!("m{}", rng.below(2000)).into_bytes();
            // Ties on purpose (order by member), near and far moves.
            let score = if step % 3 == 0 { m[&key] + (rng.below(5) as f64) - 2.0 } else { rng.below(1000) as f64 };
            z.insert(&key, score, lim);
            m.insert(key, score);
            if step % 500 == 0 {
                check(&z, &m);
            }
        }
        check(&z, &m);
    }

    #[test]
    fn encodings_and_conversion() {
        let lim = Limits::default();
        let mut z = ZSet::new();
        for i in 0..128 {
            z.insert(format!("{i}").as_bytes(), i as f64, lim);
        }
        assert_eq!(z.encoding(), "listpack");
        z.insert(b"x", 0.5, lim);
        assert_eq!(z.encoding(), "skiplist");
        assert_eq!(z.len(), 129);
        let mut z = ZSet::new();
        z.insert(&[b'a'; 65], 1.0, lim);
        assert_eq!(z.encoding(), "skiplist");
        // -0 becomes 0 in a listpack, stays -0 in a skiplist.
        let mut z = ZSet::new();
        z.insert(b"a", -0.0, lim);
        assert!(z.score(b"a").unwrap().is_sign_positive());
        let mut z = ZSet::with_hint(200, 0, lim);
        z.insert(b"a", -0.0, lim);
        assert!(z.score(b"a").unwrap().is_sign_negative());
        let items: Vec<(Vec<u8>, f64)> = (0..10).map(|i| (format!("{i}").into_bytes(), i as f64)).collect();
        let z = ZSet::from_sorted(items.iter().map(|(m, s)| (m.as_slice(), *s)), lim);
        assert_eq!(z.encoding(), "listpack");
        assert_eq!(z.len(), 10);
    }

    #[test]
    fn big_skiplist_scan_and_compaction() {
        let lim = Limits::default();
        let mut z = ZSet::new();
        for i in 0..5000 {
            z.insert(format!("k{i}").as_bytes(), (i % 50) as f64, lim);
        }
        let mut seen = std::collections::HashSet::new();
        let mut c = 0;
        let mut round = 0;
        loop {
            c = z.scan(c, 10, |m, _| {
                seen.insert(m.to_vec());
            });
            round += 1;
            if round == 20 {
                // Shrink a lot mid-scan; survivors must still be returned.
                for i in 0..4500 {
                    z.remove(format!("k{i}").as_bytes());
                }
            }
            if c == 0 {
                break;
            }
        }
        for i in 4500..5000 {
            assert!(seen.contains(format!("k{i}").as_bytes()), "missing k{i}");
        }
        if let Enc::Skip(sl) = &z.enc {
            sl.check();
            assert!(sl.nodes.len() < 2000, "compacted: {}", sl.nodes.len());
        }
        assert_eq!(z.len(), 500);
    }

    #[test]
    fn lex_ranges() {
        let lim = Limits { entries: 0, value: 64 };
        for lim in [lim, Limits::default()] {
            let mut z = ZSet::new();
            for m in ["a", "b", "c", "d", "e"] {
                z.insert(m.as_bytes(), 0.0, lim);
            }
            let r = LexRange { min: LexBound::Str(b"b".to_vec()), max: LexBound::Str(b"d".to_vec()), minex: true, maxex: false };
            assert_eq!(z.count_in(r.nonempty(), |_, m| r.gte_min(m), |_, m| r.lte_max(m)), 2);
            let r = LexRange { min: LexBound::Min, max: LexBound::Max, minex: true, maxex: true };
            assert_eq!(z.count_in(r.nonempty(), |_, m| r.gte_min(m), |_, m| r.lte_max(m)), 5);
            let r = LexRange { min: LexBound::Max, max: LexBound::Min, minex: true, maxex: true };
            assert!(!r.nonempty());
        }
    }
}
