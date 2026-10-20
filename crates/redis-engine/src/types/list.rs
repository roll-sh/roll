//! The `list` type: Redis' listpack / quicklist pair.
//!
//! A small list is a single compact node (`listpack`). When a push would take it past
//! `list-max-listpack-size` it becomes a deque of nodes (`quicklist`), and it converts back when it
//! is down to one node under half the limit. Node splits and merges follow Redis' quicklist rules,
//! driven by Redis' own listpack byte sizes, so the shape (and `OBJECT ENCODING`) changes exactly
//! when Redis' would.
//!
//! Node encoding, per entry: `varint(len) bytes rvarint(len)`. The trailing reversed varint makes
//! the last entry readable backwards, so both ends push and pop without scanning; a node is a
//! single allocation with two to four bytes of overhead per element.

use crate::util::parse_i64;
use std::collections::VecDeque;

/// `lpBytes` of an empty listpack: 6-byte header plus the EOF byte.
const LP_EMPTY: usize = 7;
/// Largest node when the fill is an entry count (`SIZE_SAFETY_LIMIT`).
const SIZE_SAFETY_LIMIT: usize = 8192;
/// Per-entry overhead Redis assumes when deciding whether a node can take one more element.
const SIZE_ESTIMATE_OVERHEAD: usize = 8;
/// Node byte limits for fill -1 … -5.
const OPTIMIZATION_LEVEL: [usize; 5] = [4096, 8192, 16384, 32768, 65536];
const NODE_SIZE: usize = std::mem::size_of::<Node>();

#[inline]
fn neg_fill_limit(fill: i64) -> usize {
    OPTIMIZATION_LEVEL[((-(fill + 1)) as usize).min(OPTIMIZATION_LEVEL.len() - 1)]
}

/// `quicklistNodeLimit`: (byte limit, entry limit).
fn node_limit(fill: i64) -> (usize, u64) {
    if fill >= 0 { (usize::MAX, if fill == 0 { 1 } else { fill as u64 }) } else { (neg_fill_limit(fill), u64::MAX) }
}

/// `quicklistNodeExceedsLimit`.
fn exceeds_limit(fill: i64, sz: usize, count: usize) -> bool {
    let (sz_limit, count_limit) = node_limit(fill);
    if sz_limit != usize::MAX { sz > sz_limit } else { sz > SIZE_SAFETY_LIMIT || count as u64 > count_limit }
}

/// How a list packs its nodes: `list-max-listpack-size`, the element size from which an
/// element gets a node of its own (DEBUG QUICKLIST-PACKED-THRESHOLD; 0 decides by the fill), and
/// `list-compress-depth` (nodes kept uncompressed at each end; 0 compresses nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fill {
    pub size: i64,
    pub packed_threshold: usize,
    pub compress: usize,
}

impl From<i64> for Fill {
    fn from(size: i64) -> Self {
        Fill { size, packed_threshold: 0, compress: 0 }
    }
}

/// What a new end node allocates up front: a byte-limited fill's limit (count-limited nodes
/// grow as they fill).
fn node_room(fill: Fill) -> usize {
    if fill.size < 0 { neg_fill_limit(fill.size) } else { 0 }
}

/// Elements this big get a node of their own (a "plain" node in Redis).
fn is_large(sz: usize, fill: Fill) -> bool {
    if fill.packed_threshold != 0 {
        sz >= fill.packed_threshold
    } else if fill.size >= 0 {
        sz > SIZE_SAFETY_LIMIT
    } else {
        sz > neg_fill_limit(fill.size)
    }
}

/// Bytes Redis' listpack spends on `v`: integer-looking strings are stored as integers.
pub fn lp_entry_size(v: &[u8]) -> usize {
    let enc = match parse_i64(v) {
        Some(0..=127) => 1,
        Some(-4096..=4095) => 2,
        Some(-32768..=32767) => 3,
        Some(-8388608..=8388607) => 4,
        Some(-2147483648..=2147483647) => 5,
        Some(_) => 9,
        None if v.len() < 64 => 1 + v.len(),
        None if v.len() < 4096 => 2 + v.len(),
        None => 5 + v.len(),
    };
    let backlen = match enc {
        0..=127 => 1,
        128..=16383 => 2,
        16384..=2097151 => 3,
        2097152..=268435455 => 4,
        _ => 5,
    };
    enc + backlen
}

#[inline]
fn varint_len(mut n: usize) -> usize {
    let mut l = 1;
    while n >= 0x80 {
        n >>= 7;
        l += 1;
    }
    l
}

#[inline]
fn encoded_len(n: usize) -> usize {
    2 * varint_len(n) + n
}

/// Write `v` as an entry into `out` (exactly `encoded_len(v.len())` bytes).
#[inline]
fn write_entry(out: &mut [u8], v: &[u8]) {
    let mut tmp = [0u8; 10];
    let mut n = v.len();
    let mut k = 0;
    while n >= 0x80 {
        tmp[k] = (n as u8) | 0x80;
        n >>= 7;
        k += 1;
    }
    tmp[k] = n as u8;
    k += 1;
    out[..k].copy_from_slice(&tmp[..k]);
    out[k..k + v.len()].copy_from_slice(v);
    let t = &mut out[k + v.len()..];
    for i in 0..k {
        t[i] = tmp[k - 1 - i];
    }
}

/// The entry at byte offset `at`: (data start, data length, offset after the entry).
#[inline]
fn read_fwd(buf: &[u8], at: usize) -> (usize, usize, usize) {
    let mut n = 0usize;
    let mut shift = 0;
    let mut p = at;
    loop {
        let b = buf[p];
        n |= ((b & 0x7f) as usize) << shift;
        p += 1;
        if b < 0x80 {
            break;
        }
        shift += 7;
    }
    let k = p - at;
    (p, n, p + n + k)
}

/// The entry ending at byte offset `end`: (entry start, data start, data length).
#[inline]
fn read_back(buf: &[u8], end: usize) -> (usize, usize, usize) {
    let mut n = 0usize;
    let mut shift = 0;
    let mut p = end;
    loop {
        p -= 1;
        let b = buf[p];
        n |= ((b & 0x7f) as usize) << shift;
        if b < 0x80 {
            break;
        }
        shift += 7;
    }
    let k = end - p;
    let data = p - n;
    (data - k, data, n)
}

/// A node's bytes, with room kept before them as well as after: entries pushed or popped at the
/// front move nothing (Redis' listpack moves the whole node).
#[derive(Clone, Debug, Default)]
struct GapBuf {
    v: Vec<u8>,
    /// Where the bytes start in `v`.
    start: usize,
}

impl PartialEq for GapBuf {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for GapBuf {}

impl std::ops::Deref for GapBuf {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        &self.v[self.start..]
    }
}

impl std::ops::DerefMut for GapBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.v[self.start..]
    }
}

impl GapBuf {
    /// Bytes allocated, both gaps included.
    fn capacity(&self) -> usize {
        self.v.capacity()
    }

    /// Room after the bytes.
    fn back_room(&self) -> usize {
        self.v.capacity() - self.v.len()
    }

    fn reserve_exact(&mut self, add: usize) {
        self.v.reserve_exact(add);
    }

    /// Drop the room before the bytes.
    fn compact(&mut self) {
        if self.start > 0 {
            self.v.drain(..self.start);
            self.start = 0;
        }
    }

    fn shrink_to(&mut self, n: usize) {
        self.compact();
        self.v.shrink_to(n);
    }

    fn shrink_to_fit(&mut self) {
        self.compact();
        self.v.shrink_to_fit();
    }

    fn resize(&mut self, len: usize, b: u8) {
        self.v.resize(self.start + len, b);
    }

    fn truncate(&mut self, len: usize) {
        self.v.truncate(self.start + len);
    }

    fn extend_from_slice(&mut self, b: &[u8]) {
        self.v.extend_from_slice(b);
    }

    /// Remove bytes `r` (relative to the start): from the front by moving the start.
    fn drain(&mut self, r: std::ops::Range<usize>) {
        if r.start == 0 {
            self.start += r.end;
            if self.start == self.v.len() {
                self.v.clear();
                self.start = 0;
            }
        } else {
            self.v.drain(self.start + r.start..self.start + r.end);
        }
    }

    fn split_off(&mut self, at: usize) -> GapBuf {
        GapBuf { v: self.v.split_off(self.start + at), start: 0 }
    }

    /// Make `n` bytes of room at the front (their content unspecified): keeps a quarter of the
    /// length spare there when it has to move the bytes.
    fn prepend(&mut self, n: usize) {
        if self.start < n {
            let len = self.len();
            let gap = n + (len / 4).max(16);
            let mut v = Vec::with_capacity(gap + len + self.back_room());
            v.resize(gap, 0);
            v.extend_from_slice(&self.v[self.start..]);
            self.v = v;
            self.start = gap;
        }
        self.start -= n;
    }
}

/// One node: a run of entries in a single buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    buf: GapBuf,
    count: u32,
    /// What Redis reports as the node size: `lpBytes` of the equivalent listpack, or the element
    /// length for a plain node.
    sz: usize,
    /// Redis' `QUICKLIST_NODE_CONTAINER_PLAIN`: a lone oversized element.
    plain: bool,
}

impl Node {
    fn packed() -> Self {
        Node { buf: GapBuf::default(), count: 0, sz: LP_EMPTY, plain: false }
    }

    fn single(v: &[u8]) -> Self {
        let mut n = Self::packed();
        n.push_back(v);
        n
    }

    /// A new end node with its whole room allocated now (`room` bytes; 0 grows as it fills):
    /// filling it then never reallocates, so nothing is left behind in the allocator. At the
    /// front the room is before the entry, for the pushes to come there.
    fn at_end(v: &[u8], front: bool, room: usize) -> Self {
        let mut n = Self::packed();
        let need = encoded_len(v.len());
        if room > need {
            n.buf = if front { GapBuf { v: vec![0; room], start: room } } else { GapBuf { v: Vec::with_capacity(room), start: 0 } };
        }
        if front {
            n.push_front(v);
        } else {
            n.push_back(v);
        }
        n
    }

    /// Done growing at this end: give back what is more than an eighth spare.
    fn settle(&mut self) {
        let (len, cap) = (self.buf.len(), self.buf.capacity());
        if cap - len > len / 8 {
            self.buf.shrink_to_fit();
        }
    }

    fn plain(v: &[u8]) -> Self {
        let mut n = Self::single(v);
        n.plain = true;
        n.sz = v.len();
        n
    }

    #[inline]
    fn len(&self) -> usize {
        self.count as usize
    }

    #[inline]
    fn esz(&self, v: &[u8]) -> usize {
        if self.plain { v.len() } else { lp_entry_size(v) }
    }

    fn iter(&self) -> NodeIter<'_> {
        NodeIter { buf: &self.buf, front: 0, back: self.buf.len(), left: self.len() }
    }

    /// Byte offset of entry `i` (`i == len` is the end), scanning from the nearer end.
    fn offset(&self, i: usize) -> usize {
        if i * 2 <= self.len() {
            let mut pos = 0;
            for _ in 0..i {
                pos = read_fwd(&self.buf, pos).2;
            }
            pos
        } else {
            let mut pos = self.buf.len();
            for _ in i..self.len() {
                pos = read_back(&self.buf, pos).0;
            }
            pos
        }
    }

    fn get(&self, i: usize) -> &[u8] {
        let (d, n, _) = read_fwd(&self.buf, self.offset(i));
        &self.buf[d..d + n]
    }

    fn first(&self) -> Option<&[u8]> {
        if self.count == 0 {
            return None;
        }
        let (d, n, _) = read_fwd(&self.buf, 0);
        Some(&self.buf[d..d + n])
    }

    fn last(&self) -> Option<&[u8]> {
        if self.count == 0 {
            return None;
        }
        let (_, d, n) = read_back(&self.buf, self.buf.len());
        Some(&self.buf[d..d + n])
    }

    /// Room for `add` more bytes, growing by at least a quarter to keep pushes amortised O(1)
    /// without doubling the slack.
    fn reserve(&mut self, add: usize) {
        if self.buf.back_room() < add {
            self.buf.reserve_exact(add.max(self.buf.len() / 4).max(16));
        }
    }

    /// Give back memory after removals once more than half the buffer is unused.
    fn trim(&mut self) {
        let (len, cap) = (self.buf.len(), self.buf.capacity());
        if cap > 64 && cap > len * 2 {
            self.buf.shrink_to(len + len / 4);
        }
    }

    /// Open a gap of `n` bytes at `at`.
    fn open(&mut self, at: usize, n: usize) {
        if at == 0 {
            self.buf.prepend(n);
            return;
        }
        self.reserve(n);
        let old = self.buf.len();
        self.buf.resize(old + n, 0);
        self.buf.copy_within(at..old, at + n);
    }

    fn insert(&mut self, i: usize, v: &[u8]) {
        let at = self.offset(i);
        let n = encoded_len(v.len());
        self.open(at, n);
        write_entry(&mut self.buf[at..at + n], v);
        self.count += 1;
        self.sz += self.esz(v);
    }

    fn push_back(&mut self, v: &[u8]) {
        let n = encoded_len(v.len());
        self.reserve(n);
        let at = self.buf.len();
        self.buf.resize(at + n, 0);
        write_entry(&mut self.buf[at..], v);
        self.count += 1;
        self.sz += self.esz(v);
    }

    fn push_front(&mut self, v: &[u8]) {
        let n = encoded_len(v.len());
        self.open(0, n);
        write_entry(&mut self.buf[..n], v);
        self.count += 1;
        self.sz += self.esz(v);
    }

    fn pop_front(&mut self) -> Option<Vec<u8>> {
        if self.count == 0 {
            return None;
        }
        let (d, n, end) = read_fwd(&self.buf, 0);
        let v = self.buf[d..d + n].to_vec();
        self.buf.drain(0..end);
        self.removed(&v, 1);
        Some(v)
    }

    fn pop_back(&mut self) -> Option<Vec<u8>> {
        if self.count == 0 {
            return None;
        }
        let (start, d, n) = read_back(&self.buf, self.buf.len());
        let v = self.buf[d..d + n].to_vec();
        self.buf.truncate(start);
        self.removed(&v, 1);
        Some(v)
    }

    fn removed(&mut self, v: &[u8], times: usize) {
        self.count -= times as u32;
        self.sz -= self.esz(v) * times;
        self.trim();
    }

    fn replace(&mut self, i: usize, v: &[u8]) {
        let at = self.offset(i);
        let (d, n, end) = read_fwd(&self.buf, at);
        let old_sz = self.esz(&self.buf[d..d + n]);
        let new_len = encoded_len(v.len());
        let old_len = end - at;
        if new_len > old_len {
            self.open(end, new_len - old_len);
        } else if new_len < old_len {
            self.buf.drain(at + new_len..end);
        }
        write_entry(&mut self.buf[at..at + new_len], v);
        self.sz = self.sz - old_sz + self.esz(v);
        if self.plain {
            self.sz = v.len();
        }
        self.trim();
    }

    /// Remove `n` entries starting at `i`.
    fn remove_range(&mut self, i: usize, n: usize) {
        let start = self.offset(i);
        let mut end = start;
        let mut freed = 0;
        for _ in 0..n {
            let (d, len, next) = read_fwd(&self.buf, end);
            freed += self.esz(&self.buf[d..d + len]);
            end = next;
        }
        self.buf.drain(start..end);
        self.count -= n as u32;
        self.sz -= freed;
        self.trim();
    }

    /// Size (Redis') of the entries in a byte range.
    fn span_sz(&self, from: usize, to: usize) -> usize {
        let mut pos = from;
        let mut sz = 0;
        while pos < to {
            let (d, n, next) = read_fwd(&self.buf, pos);
            sz += lp_entry_size(&self.buf[d..d + n]);
            pos = next;
        }
        sz
    }

    /// Move entries `[at..]` into a new node.
    fn split_off(&mut self, at: usize) -> Node {
        let off = self.offset(at);
        let moved = self.span_sz(off, self.buf.len());
        let tail = self.buf.split_off(off);
        let n = self.len() - at;
        self.count = at as u32;
        self.sz -= moved;
        self.buf.shrink_to_fit();
        Node { buf: tail, count: n as u32, sz: LP_EMPTY + moved, plain: false }
    }

    /// Move entries `[..at]` into a new node.
    fn split_front(&mut self, at: usize) -> Node {
        let mut head = self.split_off(at);
        std::mem::swap(self, &mut head);
        head
    }

    fn append(&mut self, other: Node) {
        self.reserve(other.buf.len());
        self.buf.extend_from_slice(&other.buf);
        self.count += other.count;
        self.sz = self.sz + other.sz - LP_EMPTY;
    }

    /// Remove up to `limit` entries equal to `v` (0 = all), scanning from the back if asked.
    /// Returns how many were removed.
    fn remove_matching(&mut self, v: &[u8], limit: usize, from_back: bool) -> usize {
        let mut spans: Vec<(usize, usize)> = Vec::new();
        if from_back {
            let mut end = self.buf.len();
            while end > 0 && (limit == 0 || spans.len() < limit) {
                let (start, d, n) = read_back(&self.buf, end);
                if &self.buf[d..d + n] == v {
                    spans.push((start, end));
                }
                end = start;
            }
            spans.reverse();
        } else {
            let mut pos = 0;
            while pos < self.buf.len() && (limit == 0 || spans.len() < limit) {
                let (d, n, next) = read_fwd(&self.buf, pos);
                if &self.buf[d..d + n] == v {
                    spans.push((pos, next));
                }
                pos = next;
            }
        }
        if spans.is_empty() {
            return 0;
        }
        let (mut w, mut r) = spans[0];
        for &(s, e) in &spans[1..] {
            self.buf.copy_within(r..s, w);
            w += s - r;
            r = e;
        }
        let len = self.buf.len();
        self.buf.copy_within(r..len, w);
        self.buf.truncate(w + len - r);
        self.removed(v, spans.len());
        spans.len()
    }
}

struct NodeIter<'a> {
    buf: &'a [u8],
    front: usize,
    back: usize,
    left: usize,
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 {
            return None;
        }
        let (d, n, next) = read_fwd(self.buf, self.front);
        self.front = next;
        self.left -= 1;
        Some(&self.buf[d..d + n])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

impl DoubleEndedIterator for NodeIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let (start, d, n) = read_back(self.buf, self.back);
        self.back = start;
        self.left -= 1;
        Some(&self.buf[d..d + n])
    }
}

#[derive(Clone, Debug)]
enum Nodes {
    /// `listpack`: the whole list in one node.
    Pack(Node),
    /// `quicklist` with the fill it was created with (Redis keeps a quicklist's own fill).
    Quick(VecDeque<Node>, Fill),
}

#[derive(Debug)]
pub struct List {
    nodes: Nodes,
    len: usize,
    /// Heap bytes: node buffers plus the node deque.
    mem: usize,
}

/// A copy's buffers and deque may have other capacities than the original's: `mem` is
/// counted again for it.
impl Clone for List {
    fn clone(&self) -> Self {
        let mut l = List { nodes: self.nodes.clone(), len: self.len, mem: 0 };
        let mut mem: usize = l.node_iter().map(|n| n.buf.capacity()).sum();
        if let Nodes::Quick(q, _) = &l.nodes {
            mem += q.capacity() * NODE_SIZE;
        }
        l.mem = mem;
        l
    }
}

impl Default for List {
    fn default() -> Self {
        List { nodes: Nodes::Pack(Node::packed()), len: 0, mem: 0 }
    }
}

type NodesIter<'a> = std::iter::Chain<std::slice::Iter<'a, Node>, std::slice::Iter<'a, Node>>;

impl List {
    /// Build a list the way SORT … STORE does: append everything, then pick the encoding.
    pub fn from_items<I: IntoIterator<Item = T>, T: AsRef<[u8]>>(items: I, fill: impl Into<Fill>) -> Self {
        let fill = fill.into();
        let mut l = List { nodes: Nodes::Quick(VecDeque::new(), clamp_fill(fill)), len: 0, mem: 0 };
        for v in items {
            l.push_back(v.as_ref());
        }
        l.convert_quicklist(fill.size, false);
        l
    }

    /// RDB load: an empty quicklist to fill with [`List::load_node`], then [`List::finish_load`].
    pub fn new_quicklist(fill: impl Into<Fill>) -> Self {
        let fill = fill.into();
        List { nodes: Nodes::Quick(VecDeque::new(), clamp_fill(fill)), len: 0, mem: 0 }
    }

    /// RDB load: append a node as stored (`quicklistAppendListpack` / `quicklistAppendPlainNode`);
    /// `fill` pushes its elements. A plain node holds one element; empty nodes are dropped.
    pub fn load_node(&mut self, plain: bool, fill: impl FnOnce(&mut dyn FnMut(&[u8]))) {
        let mut n = Node::packed();
        fill(&mut |v: &[u8]| n.push_back(v));
        if n.count == 0 {
            return;
        }
        if plain {
            n.plain = true;
            n.sz = n.first().map_or(0, <[u8]>::len);
        }
        n.buf.shrink_to_fit();
        self.len += n.len();
        let at = self.nodes();
        self.insert_node(at, n);
    }

    /// RDB load done: a single small packed node becomes a listpack (`listTypeTryConversion`).
    pub fn finish_load(&mut self, fill: impl Into<Fill>) {
        self.convert_quicklist(fill.into().size, false);
    }

    /// RDB save: every node as (plain, elements), in order (a listpack is one packed node).
    pub fn raw_nodes(&self) -> impl Iterator<Item = (bool, impl Iterator<Item = &[u8]>)> {
        self.node_iter().map(|n| (n.plain, n.iter()))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        self.mem
    }

    /// `listTypeAllocSize`: the listpack's bytes, or the quicklist with its nodes and their
    /// listpacks (plain nodes: the element). With a compress depth, the nodes past it at both ends
    /// are compressed (as Redis leaves them once operations have passed them).
    pub fn alloc_size(&self) -> usize {
        // `sizeof(quicklist)` and `sizeof(quicklistNode)`, as the allocator sizes them.
        const QUICKLIST: usize = 48;
        const QUICKLIST_NODE: usize = 40;
        match &self.nodes {
            Nodes::Pack(n) => n.sz,
            Nodes::Quick(nodes, fill) => {
                let d = fill.compress;
                let inner = |i: usize| d > 0 && nodes.len() >= 2 * d && i >= d && i < nodes.len() - d;
                QUICKLIST + nodes.iter().enumerate().map(|(i, n)| QUICKLIST_NODE + node_bytes(n, inner(i))).sum::<usize>()
            }
        }
    }

    pub fn encoding(&self) -> &'static str {
        match self.nodes {
            Nodes::Pack(_) => "listpack",
            Nodes::Quick(..) => "quicklist",
        }
    }

    pub fn is_quicklist(&self) -> bool {
        matches!(self.nodes, Nodes::Quick(..))
    }

    /// DEBUG OBJECT of a quicklist: its options and the nodes' listpack (or plain element) bytes.
    pub fn quicklist_info(&self) -> Option<(Fill, impl Iterator<Item = usize> + '_)> {
        match &self.nodes {
            Nodes::Quick(q, fill) => Some((*fill, q.iter().map(|n| n.sz))),
            Nodes::Pack(_) => None,
        }
    }

    /// Number of nodes (1 for a listpack).
    pub fn nodes(&self) -> usize {
        match &self.nodes {
            Nodes::Pack(_) => 1,
            Nodes::Quick(q, _) => q.len(),
        }
    }

    fn node_iter(&self) -> NodesIter<'_> {
        match &self.nodes {
            Nodes::Pack(n) => std::slice::from_ref(n).iter().chain([].iter()),
            Nodes::Quick(q, _) => {
                let (a, b) = q.as_slices();
                a.iter().chain(b.iter())
            }
        }
    }

    fn node(&self, i: usize) -> &Node {
        match &self.nodes {
            Nodes::Pack(n) => n,
            Nodes::Quick(q, _) => &q[i],
        }
    }

    /// Mutate node `i`, keeping the memory total in step.
    fn edit<R>(&mut self, i: usize, f: impl FnOnce(&mut Node) -> R) -> R {
        let n = match &mut self.nodes {
            Nodes::Pack(n) => n,
            Nodes::Quick(q, _) => &mut q[i],
        };
        let before = n.buf.capacity();
        let r = f(n);
        let after = n.buf.capacity();
        self.mem = self.mem + after - before;
        r
    }

    fn quick(&mut self) -> (&mut VecDeque<Node>, Fill) {
        match &mut self.nodes {
            Nodes::Quick(q, fill) => (q, *fill),
            Nodes::Pack(_) => unreachable!("quicklist operation on a listpack"),
        }
    }

    fn insert_node(&mut self, at: usize, node: Node) {
        let add = node.buf.capacity();
        let (q, _) = self.quick();
        let before = q.capacity();
        q.insert(at, node);
        let after = q.capacity();
        self.mem = self.mem + add + (after - before) * NODE_SIZE;
    }

    fn remove_node(&mut self, at: usize) -> Node {
        let (q, _) = self.quick();
        let n = q.remove(at).expect("node index");
        self.mem -= n.buf.capacity();
        n
    }

    /// Node and in-node index of element `i` (< len).
    fn locate(&self, i: usize) -> (usize, usize) {
        match &self.nodes {
            Nodes::Pack(_) => (0, i),
            Nodes::Quick(q, _) => {
                if i * 2 < self.len {
                    let mut acc = 0;
                    for (ni, n) in q.iter().enumerate() {
                        if acc + n.len() > i {
                            return (ni, i - acc);
                        }
                        acc += n.len();
                    }
                } else {
                    let mut acc = self.len;
                    for (ni, n) in q.iter().enumerate().rev() {
                        acc -= n.len();
                        if acc <= i {
                            return (ni, i - acc);
                        }
                    }
                }
                unreachable!("index {i} out of range")
            }
        }
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &[u8]> + '_ {
        self.node_iter().flat_map(Node::iter)
    }

    /// Iterate forward from element `start`.
    pub fn iter_from(&self, start: usize) -> impl Iterator<Item = &[u8]> + '_ {
        let (ni, off) = if start < self.len { self.locate(start) } else { (self.nodes(), 0) };
        let mut nodes = self.node_iter().skip(ni);
        let first = nodes.next().map(|n| n.iter().skip(off));
        first.into_iter().flatten().chain(nodes.flat_map(Node::iter))
    }

    pub fn get(&self, i: usize) -> Option<&[u8]> {
        if i >= self.len {
            return None;
        }
        let (ni, off) = self.locate(i);
        Some(self.node(ni).get(off))
    }

    pub fn first(&self) -> Option<&[u8]> {
        self.node_iter().next()?.first()
    }

    pub fn last(&self) -> Option<&[u8]> {
        self.node_iter().next_back()?.last()
    }

    // ---- encoding conversions ----

    /// Before adding `add_count` elements totalling `add_bytes`: convert a listpack that would
    /// outgrow `fill` (`list-max-listpack-size`) into a quicklist.
    /// A quicklist keeps its fill but follows the current packed threshold.
    pub fn grow(&mut self, fill: impl Into<Fill>, add_bytes: usize, add_count: usize) {
        let fill = fill.into();
        let n = match &mut self.nodes {
            Nodes::Pack(n) => n,
            Nodes::Quick(_, f) => {
                f.packed_threshold = fill.packed_threshold;
                return;
            }
        };
        if !exceeds_limit(fill.size, n.sz + add_bytes, n.len() + add_count) {
            return;
        }
        let n = std::mem::replace(n, Node::packed());
        let mut q = VecDeque::new();
        if n.count > 0 {
            q.reserve_exact(1);
            self.mem += q.capacity() * NODE_SIZE;
            q.push_back(n);
        } else {
            self.mem -= n.buf.capacity();
        }
        self.nodes = Nodes::Quick(q, clamp_fill(fill));
    }

    /// After removing elements: back to a listpack once a single node is under half the limit.
    pub fn shrink(&mut self, fill: impl Into<Fill>) {
        self.convert_quicklist(fill.into().size, true);
    }

    fn convert_quicklist(&mut self, fill: i64, shrinking: bool) {
        let Nodes::Quick(q, _) = &mut self.nodes else { return };
        if q.len() != 1 || q[0].plain {
            return;
        }
        let (mut sz_limit, mut count_limit) = node_limit(fill);
        if shrinking {
            sz_limit /= 2;
            count_limit /= 2;
        }
        if q[0].sz > sz_limit || q[0].count as u64 > count_limit {
            return;
        }
        self.mem -= q.capacity() * NODE_SIZE;
        let n = q.pop_front().expect("one node");
        self.nodes = Nodes::Pack(n);
    }

    // ---- quicklist node rules ----

    /// `_quicklistNodeAllowInsert`.
    fn allow_insert(n: Option<&Node>, fill: Fill, sz: usize) -> bool {
        let Some(n) = n else { return false };
        if n.plain || is_large(sz, fill) {
            return false;
        }
        !exceeds_limit(fill.size, n.sz + sz + SIZE_ESTIMATE_OVERHEAD, n.len() + 1)
    }

    /// `_quicklistNodeAllowMerge`.
    fn allow_merge(a: &Node, b: &Node, fill: Fill) -> bool {
        !a.plain && !b.plain && !exceeds_limit(fill.size, a.sz + b.sz - LP_EMPTY, a.len() + b.len())
    }

    /// Merge node `i + 1` into node `i`.
    fn merge(&mut self, i: usize) {
        let b = self.remove_node(i + 1);
        self.edit(i, |a| a.append(b));
    }

    fn qnode(&self, i: usize) -> Option<&Node> {
        match &self.nodes {
            Nodes::Quick(q, _) => q.get(i),
            Nodes::Pack(_) => None,
        }
    }

    /// `_quicklistMergeNodes`: try to merge the neighbourhood of node `c`.
    fn merge_around(&mut self, mut c: usize) {
        let fill = self.quick().1;
        let q = Self::qnode;
        if c >= 2 && Self::allow_merge(q(self, c - 2).unwrap(), q(self, c - 1).unwrap(), fill) {
            self.merge(c - 2);
            c -= 1;
        }
        if let (Some(a), Some(b)) = (q(self, c + 1), q(self, c + 2))
            && Self::allow_merge(a, b, fill)
        {
            self.merge(c + 1);
        }
        let mut t = c;
        if c >= 1 && Self::allow_merge(q(self, c - 1).unwrap(), q(self, c).unwrap(), fill) {
            self.merge(c - 1);
            t = c - 1;
        }
        if let Some(b) = q(self, t + 1)
            && Self::allow_merge(q(self, t).unwrap(), b, fill)
        {
            self.merge(t);
        }
    }

    /// `_quicklistSplitNode`: with `after`, node `i` keeps `[..=off]` and the result holds the
    /// rest; otherwise the result holds `[..off]` and node `i` keeps the rest.
    /// The returned node is detached: `insert_node` accounts for it.
    fn split(&mut self, i: usize, off: usize, after: bool) -> Node {
        self.edit(i, |n| if after { n.split_off(off + 1) } else { n.split_front(off) })
    }

    /// `_quicklistInsert`: insert `v` before or after element `off` of node `ni`.
    fn quick_insert(&mut self, ni: usize, off: usize, after: bool, v: &[u8]) {
        let (q, fill) = self.quick();
        let sz = v.len();
        let node = &q[ni];
        let full = !Self::allow_insert(Some(node), fill, sz);
        let at_tail = after && off + 1 == node.len();
        let at_head = !after && off == 0;
        let avail_next = at_tail && Self::allow_insert(q.get(ni + 1), fill, sz);
        let avail_prev = at_head && ni > 0 && Self::allow_insert(q.get(ni - 1), fill, sz);
        let plain = node.plain;
        let side = if after { ni + 1 } else { ni };
        self.len += 1;

        if is_large(sz, fill) {
            if plain || at_tail || at_head {
                self.insert_node(side, Node::plain(v));
            } else {
                let rest = self.split(ni, off, after);
                let p = Node::plain(v);
                if after {
                    self.insert_node(ni + 1, p);
                    self.insert_node(ni + 2, rest);
                } else {
                    self.insert_node(ni, p);
                    self.insert_node(ni, rest);
                }
            }
            return;
        }
        if !full {
            self.edit(ni, |n| n.insert(off + after as usize, v));
        } else if at_tail && avail_next {
            self.edit(ni + 1, |n| n.push_front(v));
        } else if at_head && avail_prev {
            self.edit(ni - 1, |n| n.push_back(v));
        } else if at_tail || at_head {
            self.insert_node(side, Node::single(v));
        } else {
            let mut rest = self.split(ni, off, after);
            if after {
                rest.push_front(v);
            } else {
                rest.push_back(v);
            }
            self.insert_node(side, rest);
            self.merge_around(if after { ni } else { ni + 1 });
        }
    }

    // ---- element operations ----

    pub fn push_front(&mut self, v: &[u8]) {
        if let Nodes::Pack(_) = self.nodes {
            self.edit(0, |n| n.push_front(v));
            self.len += 1;
            return;
        }
        let (q, fill) = self.quick();
        if is_large(v.len(), fill) {
            self.insert_node(0, Node::plain(v));
        } else if Self::allow_insert(q.front(), fill, v.len()) {
            self.edit(0, |n| n.push_front(v));
        } else {
            let filled = !q.is_empty();
            self.insert_node(0, Node::at_end(v, true, node_room(fill)));
            // The node this end filled won't grow there again: no slack kept in it (Redis'
            // listpacks are exactly sized), only the end nodes have room.
            if filled {
                self.edit(1, Node::settle);
            }
        }
        self.len += 1;
    }

    pub fn push_back(&mut self, v: &[u8]) {
        if let Nodes::Pack(_) = self.nodes {
            self.edit(0, |n| n.push_back(v));
            self.len += 1;
            return;
        }
        let (q, fill) = self.quick();
        let last = q.len().wrapping_sub(1);
        if is_large(v.len(), fill) {
            let at = q.len();
            self.insert_node(at, Node::plain(v));
        } else if Self::allow_insert(q.back(), fill, v.len()) {
            self.edit(last, |n| n.push_back(v));
        } else {
            let at = q.len();
            self.insert_node(at, Node::at_end(v, false, node_room(fill)));
            if at > 0 {
                self.edit(at - 1, Node::settle);
            }
        }
        self.len += 1;
    }

    pub fn push(&mut self, front: bool, v: &[u8]) {
        if front { self.push_front(v) } else { self.push_back(v) }
    }

    pub fn pop_front(&mut self) -> Option<Vec<u8>> {
        if self.len == 0 {
            return None;
        }
        let v = self.edit(0, Node::pop_front)?;
        self.len -= 1;
        if let Nodes::Quick(q, _) = &self.nodes
            && q[0].count == 0
        {
            self.remove_node(0);
        }
        Some(v)
    }

    pub fn pop_back(&mut self) -> Option<Vec<u8>> {
        if self.len == 0 {
            return None;
        }
        let last = self.nodes() - 1;
        let v = self.edit(last, Node::pop_back)?;
        self.len -= 1;
        if let Nodes::Quick(q, _) = &self.nodes
            && q[last].count == 0
        {
            self.remove_node(last);
        }
        Some(v)
    }

    pub fn pop(&mut self, front: bool) -> Option<Vec<u8>> {
        if front { self.pop_front() } else { self.pop_back() }
    }

    /// Pop up to `n` elements from one end, in pop order.
    pub fn pop_n(&mut self, front: bool, n: usize) -> Vec<Vec<u8>> {
        let n = n.min(self.len);
        let out: Vec<Vec<u8>> =
            if front { self.iter().take(n).map(<[u8]>::to_vec).collect() } else { self.iter().rev().take(n).map(<[u8]>::to_vec).collect() };
        if front {
            self.remove_range(0, n);
        } else {
            self.remove_range(self.len - n, n);
        }
        out
    }

    /// Replace element `i` (`quicklistReplaceEntry`). False if out of range.
    pub fn set(&mut self, i: usize, v: &[u8]) -> bool {
        if i >= self.len {
            return false;
        }
        let (ni, off) = self.locate(i);
        let Nodes::Quick(q, fill) = &self.nodes else {
            self.edit(0, |n| n.replace(off, v));
            return true;
        };
        let fill = *fill;
        let node = &q[ni];
        let large = is_large(v.len(), fill);
        if !node.plain && !large {
            self.edit(ni, |n| n.replace(off, v));
        } else if node.plain {
            if large {
                self.edit(ni, |n| n.replace(0, v));
            } else {
                self.quick_insert(ni, 0, true, v);
                self.remove_node(ni);
                self.len -= 1;
            }
        } else {
            let tail = node.len() - 1;
            let rest = if off != tail { Some(self.split(ni, off, true)) } else { None };
            self.insert_node(ni + 1, Node::plain(v));
            if let Some(rest) = rest {
                self.insert_node(ni + 2, rest);
            }
            if self.node(ni).count == 1 {
                self.remove_node(ni);
            } else {
                self.edit(ni, Node::pop_back);
                self.merge_around(ni + 1);
            }
        }
        true
    }

    /// Insert `v` before or after element `i`.
    pub fn insert(&mut self, i: usize, after: bool, v: &[u8]) {
        let (ni, off) = self.locate(i);
        if let Nodes::Pack(_) = self.nodes {
            self.edit(0, |n| n.insert(off + after as usize, v));
            self.len += 1;
        } else {
            self.quick_insert(ni, off, after, v);
        }
    }

    /// Remove `count` elements starting at `start` (`quicklistDelRange`; no merging).
    pub fn remove_range(&mut self, start: usize, count: usize) {
        let count = count.min(self.len.saturating_sub(start));
        if count == 0 {
            return;
        }
        let (mut ni, mut off) = self.locate(start);
        self.len -= count;
        if let Nodes::Pack(_) = self.nodes {
            self.edit(0, |n| n.remove_range(off, count));
            return;
        }
        let mut left = count;
        while left > 0 {
            let n = self.node(ni);
            if off == 0 && left >= n.len() {
                left -= n.len();
                self.remove_node(ni);
            } else {
                let del = left.min(n.len() - off);
                self.edit(ni, |n| n.remove_range(off, del));
                left -= del;
                if self.node(ni).count == 0 {
                    self.remove_node(ni);
                } else {
                    ni += 1;
                }
            }
            off = 0;
        }
    }

    /// LREM: remove up to `limit` elements equal to `v` (0 = all), from the tail if `from_back`.
    pub fn remove_matching(&mut self, v: &[u8], limit: usize, from_back: bool) -> usize {
        let mut removed = 0;
        let n = self.nodes();
        for k in 0..n {
            let ni = if from_back { n - 1 - k } else { k };
            let left = if limit == 0 { 0 } else { limit - removed };
            removed += self.edit(ni, |node| node.remove_matching(v, left, from_back));
            if limit != 0 && removed >= limit {
                break;
            }
        }
        self.len -= removed;
        if let Nodes::Quick(q, _) = &mut self.nodes {
            let before = q.len();
            let freed: usize = q.iter().filter(|n| n.count == 0).map(|n| n.buf.capacity()).sum();
            q.retain(|n| n.count > 0);
            if q.len() != before {
                self.mem -= freed;
            }
        }
        removed
    }
}

/// `quicklistSetOptions`: the fill within its bits, the depth at most `COMPRESS_MAX`.
fn clamp_fill(fill: Fill) -> Fill {
    Fill { size: fill.size.clamp(-5, 32767), compress: fill.compress.min(65535), ..fill }
}

/// `MIN_COMPRESS_BYTES` and `MIN_COMPRESS_IMPROVE`: smaller nodes, and compression saving less,
/// stay uncompressed.
const MIN_COMPRESS_BYTES: usize = 48;
const MIN_COMPRESS_IMPROVE: usize = 8;

/// The bytes Redis keeps for a node: its listpack (or plain element), or `quicklistLZF` (its
/// size and the LZF data) when `compress` and LZF saves enough.
fn node_bytes(n: &Node, compress: bool) -> usize {
    if !compress || n.sz < MIN_COMPRESS_BYTES {
        return n.sz;
    }
    let mut raw = Vec::with_capacity(n.sz);
    if n.plain {
        raw.extend_from_slice(n.iter().next().unwrap_or_default());
    } else {
        let mut w = crate::rdb::LpWriter::new(&mut raw);
        for v in n.iter() {
            w.str(v);
        }
        w.finish();
    }
    let mut z = Vec::new();
    match crate::rdb::lzf_compress(&raw, n.sz, &mut z) {
        true if z.len() + MIN_COMPRESS_IMPROVE < n.sz => 8 + z.len(),
        _ => n.sz,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(l: &List) -> Vec<String> {
        l.iter().map(|v| String::from_utf8_lossy(v).into_owned()).collect()
    }

    /// Recompute what `mem` and each node's `sz` should be and compare.
    fn check(l: &List) {
        let mut mem = 0;
        let mut len = 0;
        for n in l.node_iter() {
            mem += n.buf.capacity();
            len += n.len();
            let sz: usize = if n.plain { n.iter().map(|v| v.len()).sum() } else { LP_EMPTY + n.iter().map(lp_entry_size).sum::<usize>() };
            assert_eq!(n.sz, sz, "node sz");
            assert_eq!(n.iter().count(), n.len());
            assert_eq!(n.iter().rev().count(), n.len());
            assert!(n.count > 0 || !l.is_quicklist(), "empty quicklist node");
        }
        if let Nodes::Quick(q, _) = &l.nodes {
            mem += q.capacity() * NODE_SIZE;
        }
        assert_eq!(l.mem, mem, "mem");
        assert_eq!(l.len, len, "len");
    }

    #[test]
    fn entry_sizes() {
        assert_eq!(lp_entry_size(b"0"), 2);
        assert_eq!(lp_entry_size(b"127"), 2);
        assert_eq!(lp_entry_size(b"128"), 3);
        assert_eq!(lp_entry_size(b"-4096"), 3);
        assert_eq!(lp_entry_size(b"32767"), 4);
        assert_eq!(lp_entry_size(b"8388607"), 5);
        assert_eq!(lp_entry_size(b"2147483647"), 6);
        assert_eq!(lp_entry_size(b"9223372036854775807"), 10);
        assert_eq!(lp_entry_size(b"01"), 4);
        assert_eq!(lp_entry_size(b"abc"), 5);
        assert_eq!(lp_entry_size(&[b'x'; 63]), 65);
        assert_eq!(lp_entry_size(&[b'x'; 64]), 67);
        assert_eq!(lp_entry_size(&[b'x'; 125]), 128);
        assert_eq!(lp_entry_size(&[b'x'; 126]), 130);
        assert_eq!(lp_entry_size(&[b'x'; 4096]), 4103);
    }

    #[test]
    fn node_both_ends() {
        let mut n = Node::packed();
        for i in 0..300 {
            let v = vec![b'a' + (i % 26) as u8; i];
            if i % 2 == 0 { n.push_back(&v) } else { n.push_front(&v) }
        }
        assert_eq!(n.len(), 300);
        let fwd: Vec<_> = n.iter().map(|v| v.len()).collect();
        let mut back: Vec<_> = n.iter().rev().map(|v| v.len()).collect();
        back.reverse();
        assert_eq!(fwd, back);
        assert_eq!(n.get(0).len(), 299);
        assert_eq!(n.get(299).len(), 298);
        assert_eq!(n.get(150).len(), fwd[150]);
        n.replace(150, b"zz");
        assert_eq!(n.get(150), b"zz");
        assert_eq!(n.pop_back().unwrap().len(), 298);
        assert_eq!(n.pop_front().unwrap().len(), 299);
        n.remove_range(10, 100);
        assert_eq!(n.len(), 198);
        let tail = n.split_off(50);
        assert_eq!((n.len(), tail.len()), (50, 148));
        n.append(tail);
        assert_eq!(n.len(), 198);
    }

    #[test]
    fn listpack_to_quicklist_and_back() {
        let mut l = List::default();
        // 60-byte strings take 62 bytes; the 133rd push would exceed 8192.
        let v = [b'v'; 60];
        for _ in 0..132 {
            l.grow(-2, 60, 1);
            l.push_back(&v);
        }
        assert_eq!(l.encoding(), "listpack");
        l.grow(-2, 60, 1);
        l.push_back(&v);
        assert_eq!(l.encoding(), "quicklist");
        assert_eq!(l.nodes(), 2);
        check(&l);
        while l.len() > 66 {
            l.pop_back();
            l.shrink(-2);
        }
        assert_eq!(l.encoding(), "quicklist");
        l.pop_back();
        l.shrink(-2);
        // One node of 65 * 62 + 7 = 4037 bytes ≤ 4096.
        assert_eq!(l.encoding(), "listpack");
        check(&l);
    }

    #[test]
    fn packed_threshold_makes_plain_nodes() {
        let fill = Fill { size: -2, packed_threshold: 4, compress: 0 };
        let mut l = List::default();
        for v in ["a", "b", "c"] {
            l.grow(fill, v.len(), 1);
            l.push_back(v.as_bytes());
        }
        // A listpack takes anything; a quicklist follows the threshold.
        assert_eq!(l.encoding(), "listpack");
        let mut q = List::new_quicklist(fill);
        for v in ["a", "bbbb", "c", "dd"] {
            q.grow(fill, v.len(), 1);
            q.push_back(v.as_bytes());
        }
        check(&q);
        assert_eq!(q.quicklist_info().unwrap().1.collect::<Vec<_>>(), [10, 4, 14]);
        // Lowered later, it applies to the next writes.
        q.grow(Fill { packed_threshold: 2, ..fill }, 2, 1);
        q.push_front(b"zz");
        check(&q);
        assert_eq!(q.nodes(), 4);
        assert_eq!(items(&q), ["zz", "a", "bbbb", "c", "dd"]);
    }

    #[test]
    fn count_fill() {
        let mut l = List::default();
        for i in 0..10 {
            l.grow(3, 1, 1);
            l.push_back(i.to_string().as_bytes());
        }
        assert_eq!(l.encoding(), "quicklist");
        assert_eq!(l.nodes(), 4);
        check(&l);
        assert_eq!(items(&l), ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]);
        l.insert(4, true, b"x");
        check(&l);
        assert_eq!(items(&l)[5], "x");
        l.remove_range(1, 7);
        check(&l);
        assert_eq!(items(&l), ["0", "7", "8", "9"]);
        assert_eq!(l.remove_matching(b"8", 0, false), 1);
        l.shrink(3);
        check(&l);
        assert_eq!(l.encoding(), "quicklist");
    }

    #[test]
    fn plain_nodes() {
        let mut l = List::default();
        let big = vec![b'b'; 10_000];
        l.grow(-2, 1, 1);
        l.push_back(b"a");
        l.grow(-2, big.len(), 1);
        l.push_back(&big);
        l.push_front(&big);
        assert_eq!(l.encoding(), "quicklist");
        assert_eq!(l.nodes(), 3);
        check(&l);
        assert!(l.set(1, &big));
        check(&l);
        assert!(l.set(1, b"small"));
        check(&l);
        assert_eq!(l.get(1), Some(&b"small"[..]));
        assert!(l.set(0, b"x"));
        check(&l);
        assert_eq!(l.len(), 3);
        l.pop_back();
        l.shrink(-2);
        check(&l);
        assert_eq!(items(&l), ["x", "small"]);
        assert_eq!(l.encoding(), "listpack");
    }

    #[test]
    fn insert_splits_and_merges() {
        let mut l = List::default();
        l.grow(4, 0, 9);
        for i in 0..9 {
            l.push_back(i.to_string().as_bytes());
        }
        // Nodes of four: [0..3] [4..7] [8]
        assert_eq!(l.nodes(), 3);
        l.insert(1, true, b"a");
        check(&l);
        assert_eq!(items(&l), ["0", "1", "a", "2", "3", "4", "5", "6", "7", "8"]);
        l.insert(0, false, b"b");
        check(&l);
        assert_eq!(items(&l)[0], "b");
        let mut want = items(&l);
        for k in 0..30 {
            let i = (k * 7) % l.len();
            l.insert(i, k % 2 == 0, b"n");
            want.insert(i + (k % 2 == 0) as usize, "n".into());
            check(&l);
            assert_eq!(items(&l), want);
        }
        assert!(l.iter().rev().map(|v| v.to_vec()).eq(want.iter().rev().map(|s| s.as_bytes().to_vec())));
        for (k, w) in want.iter().enumerate() {
            assert_eq!(l.get(k), Some(w.as_bytes()));
            assert_eq!(l.iter_from(k).next(), Some(w.as_bytes()));
        }
    }

    #[test]
    fn remove_matching_directions() {
        let mut l = List::default();
        l.grow(2, 0, 10);
        for v in ["a", "x", "b", "x", "c", "x", "d", "x"] {
            l.push_back(v.as_bytes());
        }
        assert_eq!(l.remove_matching(b"x", 2, true), 2);
        assert_eq!(items(&l), ["a", "x", "b", "x", "c", "d"]);
        check(&l);
        assert_eq!(l.remove_matching(b"x", 1, false), 1);
        assert_eq!(items(&l), ["a", "b", "x", "c", "d"]);
        assert_eq!(l.remove_matching(b"x", 0, false), 1);
        check(&l);
        assert_eq!(l.pop_n(false, 2), vec![b"d".to_vec(), b"c".to_vec()]);
        assert_eq!(l.pop_n(true, 5), vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(l.is_empty());
        check(&l);
    }

    #[test]
    fn from_items_picks_encoding() {
        let l = List::from_items(["a", "b", "c"], -2);
        assert_eq!(l.encoding(), "listpack");
        check(&l);
        let l = List::from_items((0..5000).map(|i| i.to_string()), -2);
        assert_eq!(l.encoding(), "quicklist");
        assert_eq!(l.len(), 5000);
        check(&l);
    }
}
