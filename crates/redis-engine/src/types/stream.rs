//! The `stream` type.
//!
//! Redis stores a stream as a radix tree of listpacks: each tree key is a *master ID* and each
//! listpack holds up to `stream-node-max-entries` entries (or `stream-node-max-bytes` bytes),
//! delta-encoded against the master ID, with a *master entry* of field names that later entries
//! reuse when they have the same fields. Deleted entries stay in their node as tombstones until
//! the whole node goes.
//!
//! We keep the same logical structure — a `BTreeMap` from master ID to a compact [`Node`] —
//! because it is observable: node boundaries decide what approximate trimming (`MAXLEN ~`)
//! removes and `XINFO STREAM` reports `radix-tree-keys` / `radix-tree-nodes`. A node's byte size
//! is tracked exactly as the listpack Redis would build (`lp_bytes`), so nodes split where Redis'
//! do. Entries inside a node are a varint-encoded byte buffer (about 2–4 bytes of overhead per
//! entry plus field values; field names are stored once per node when entries share them).
//!
//! Consumer groups ([`Group`]) keep the PEL (pending entries list) keyed by ID, a per-consumer
//! index, and the time-ordered delivery list Redis uses for `XREADGROUP … CLAIM` (with the
//! "NACK zone" of `XNACK`ed entries at its head).

use super::rax;
use crate::client::jemalloc_size;
use crate::pack::Pack;
use crate::value::Str;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

// ---------------------------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------------------------

/// A stream entry ID: `<ms>-<seq>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId {
    pub ms: u64,
    pub seq: u64,
}

impl StreamId {
    pub const MIN: StreamId = StreamId { ms: 0, seq: 0 };
    pub const MAX: StreamId = StreamId { ms: u64::MAX, seq: u64::MAX };

    pub const fn new(ms: u64, seq: u64) -> Self {
        StreamId { ms, seq }
    }

    pub fn is_zero(&self) -> bool {
        self.ms == 0 && self.seq == 0
    }

    /// The successor, or `None` for the maximal ID (`streamIncrID` failing).
    pub fn next(self) -> Option<StreamId> {
        if self.seq == u64::MAX {
            if self.ms == u64::MAX { None } else { Some(StreamId::new(self.ms + 1, 0)) }
        } else {
            Some(StreamId::new(self.ms, self.seq + 1))
        }
    }

    /// The predecessor, or `None` for `0-0` (`streamDecrID` failing).
    pub fn prev(self) -> Option<StreamId> {
        if self.seq == 0 {
            if self.ms == 0 { None } else { Some(StreamId::new(self.ms - 1, u64::MAX)) }
        } else {
            Some(StreamId::new(self.ms, self.seq - 1))
        }
    }

    /// The 128-bit big-endian key Redis uses in its radix trees.
    pub fn to_be_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&self.ms.to_be_bytes());
        b[8..].copy_from_slice(&self.seq.to_be_bytes());
        b
    }

    pub fn from_be_bytes(b: [u8; 16]) -> Self {
        StreamId { ms: u64::from_be_bytes(b[..8].try_into().unwrap()), seq: u64::from_be_bytes(b[8..].try_into().unwrap()) }
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.ms, self.seq)
    }
}

// ---------------------------------------------------------------------------------------------
// Listpack size emulation (exact byte sizes of Redis' listpack entries)
// ---------------------------------------------------------------------------------------------

const LP_HDR_EOF: u64 = 7;
const LISTPACK_MAX: u64 = 1 << 30;

#[inline]
fn lp_backlen(l: u64) -> u64 {
    match l {
        0..=127 => 1,
        128..=16383 => 2,
        16384..=2097151 => 3,
        2097152..=268435455 => 4,
        _ => 5,
    }
}

#[inline]
fn lp_int(v: i64) -> u64 {
    let enc = match v {
        0..=127 => 1,
        -4096..=4095 => 2,
        -32768..=32767 => 3,
        -8388608..=8388607 => 4,
        -2147483648..=2147483647 => 5,
        _ => 9,
    };
    enc + 1
}

#[inline]
fn lp_str(s: &[u8]) -> u64 {
    if let Some(v) = crate::util::parse_i64(s) {
        return lp_int(v);
    }
    let n = s.len() as u64;
    let enc = if n < 64 {
        1 + n
    } else if n < 4096 {
        2 + n
    } else {
        5 + n
    };
    enc + lp_backlen(enc)
}

// ---------------------------------------------------------------------------------------------
// Varints for the node encoding
// ---------------------------------------------------------------------------------------------

#[inline]
fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

#[inline]
fn get_varint(buf: &[u8], pos: &mut usize) -> u64 {
    let mut n = 0u64;
    let mut shift = 0;
    loop {
        let b = buf[*pos];
        *pos += 1;
        n |= ((b & 0x7f) as u64) << shift;
        if b < 0x80 {
            return n;
        }
        shift += 7;
    }
}

#[inline]
fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

#[inline]
fn get_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let n = get_varint(buf, pos) as usize;
    let s = &buf[*pos..*pos + n];
    *pos += n;
    s
}

// ---------------------------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------------------------

const FLAG_DELETED: u8 = 1;
const FLAG_SAMEFIELDS: u8 = 2;

/// One radix tree node: the equivalent of a stream listpack.
///
/// `data` holds the entries back to back: `flags`, `ms - master.ms` (varint), `seq` (varint),
/// then either the values (same fields as the master entry) or `nfields` and field/value pairs,
/// each string as varint length + bytes.
#[derive(Clone, Debug)]
struct Node {
    fields: Pack,
    count: u32,
    deleted: u32,
    /// Emulated listpack bytes, excluding the header/EOF and the count/deleted integers.
    body: u64,
    /// ID of the last entry (live or tombstone).
    last: StreamId,
    data: Vec<u8>,
}

impl Node {
    fn lp_bytes(&self) -> u64 {
        LP_HDR_EOF + lp_int(self.count as i64) + lp_int(self.deleted as i64) + self.body
    }

    fn mem(&self) -> usize {
        NODE_OVERHEAD + self.data.capacity() + self.fields.mem()
    }

    /// Decode the entry at `pos`; returns it and the position after it.
    #[inline]
    fn decode<'a>(&'a self, master: StreamId, pos: usize) -> (EntryRef<'a>, usize) {
        let d = &self.data;
        let mut p = pos;
        let flags = d[p];
        p += 1;
        let ms = master.ms.wrapping_add(get_varint(d, &mut p));
        let seq = get_varint(d, &mut p);
        let same = flags & FLAG_SAMEFIELDS != 0;
        let nfields = if same { self.fields.len() } else { get_varint(d, &mut p) as usize };
        let start = p;
        let per = if same { 1 } else { 2 };
        for _ in 0..nfields * per {
            let n = get_varint(d, &mut p) as usize;
            p += n;
        }
        (
            EntryRef { id: StreamId::new(ms, seq), deleted: flags & FLAG_DELETED != 0, same, nfields, master: &self.fields, data: &d[start..p] },
            p,
        )
    }

    /// Positions of every entry (for reverse iteration).
    fn positions(&self, master: StreamId) -> Vec<usize> {
        let mut out = Vec::with_capacity((self.count + self.deleted) as usize);
        let mut p = 0;
        while p < self.data.len() {
            out.push(p);
            p = self.decode(master, p).1;
        }
        out
    }
}

const NODE_OVERHEAD: usize = std::mem::size_of::<Node>() + std::mem::size_of::<StreamId>() + 16;

/// A stream entry as stored (live or tombstone).
#[derive(Clone, Copy)]
pub struct EntryRef<'a> {
    pub id: StreamId,
    pub deleted: bool,
    same: bool,
    nfields: usize,
    master: &'a Pack,
    data: &'a [u8],
}

impl std::fmt::Debug for EntryRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry").field("id", &self.id).field("deleted", &self.deleted).finish()
    }
}

impl<'a> EntryRef<'a> {
    /// Number of field/value pairs.
    pub fn len(&self) -> usize {
        self.nfields
    }

    pub fn is_empty(&self) -> bool {
        self.nfields == 0
    }

    /// Stored with the master entry's field names (`STREAM_ITEM_FLAG_SAMEFIELDS`).
    pub fn same_fields(&self) -> bool {
        self.same
    }

    pub fn fields(&self) -> FieldIter<'a> {
        FieldIter { master: if self.same { Some(self.master.iter()) } else { None }, data: self.data, pos: 0, left: self.nfields }
    }
}

pub struct FieldIter<'a> {
    master: Option<crate::pack::PackIter<'a>>,
    data: &'a [u8],
    pos: usize,
    left: usize,
}

impl<'a> Iterator for FieldIter<'a> {
    type Item = (&'a [u8], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let f = match &mut self.master {
            Some(m) => m.next().unwrap_or(b""),
            None => get_bytes(self.data, &mut self.pos),
        };
        let v = get_bytes(self.data, &mut self.pos);
        Some((f, v))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left, Some(self.left))
    }
}

/// A node as seen by RDB serialization.
pub struct RawNode<'a> {
    pub master: StreamId,
    pub master_fields: Vec<&'a [u8]>,
    pub count: u32,
    pub deleted: u32,
    node: &'a Node,
}

impl<'a> RawNode<'a> {
    /// Every entry in the node, tombstones included, in order.
    pub fn entries(&self) -> impl Iterator<Item = EntryRef<'a>> + 'a {
        let node = self.node;
        let master = self.master;
        let mut p = 0;
        std::iter::from_fn(move || {
            if p >= node.data.len() {
                return None;
            }
            let (e, n) = node.decode(master, p);
            p = n;
            Some(e)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Trimming arguments
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimBy {
    MaxLen(u64),
    MinId(StreamId),
}

/// What to do with consumer group references when entries go (`KEEPREF | DELREF | ACKED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RefPolicy {
    #[default]
    KeepRef,
    DelRef,
    Acked,
}

#[derive(Clone, Copy, Debug)]
pub struct Trim {
    pub by: TrimBy,
    pub approx: bool,
    /// 0 = unlimited.
    pub limit: u64,
    pub policy: RefPolicy,
}

// ---------------------------------------------------------------------------------------------
// Entries: the stream log and its metadata
// ---------------------------------------------------------------------------------------------

/// The entries of a stream plus the ID bookkeeping (`last-generated-id`, `entries-added`, …).
#[derive(Clone, Debug, Default)]
pub struct Entries {
    nodes: BTreeMap<StreamId, Node>,
    rax: RaxShape,
    length: u64,
    last_id: StreamId,
    first_id: StreamId,
    max_deleted: StreamId,
    entries_added: u64,
    mem: usize,
}

impl Entries {
    pub fn len(&self) -> u64 {
        self.length
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn last_id(&self) -> StreamId {
        self.last_id
    }
    /// The recorded first entry ID (`recorded-first-entry-id`; 0-0 when empty).
    pub fn first_id(&self) -> StreamId {
        self.first_id
    }
    pub fn max_deleted_id(&self) -> StreamId {
        self.max_deleted
    }
    pub fn entries_added(&self) -> u64 {
        self.entries_added
    }
    pub fn set_last_id(&mut self, id: StreamId) {
        self.last_id = id;
    }
    pub fn set_first_id(&mut self, id: StreamId) {
        self.first_id = id;
    }
    pub fn set_max_deleted_id(&mut self, id: StreamId) {
        self.max_deleted = id;
    }
    pub fn set_entries_added(&mut self, n: u64) {
        self.entries_added = n;
    }

    /// `radix-tree-keys`.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// `radix-tree-nodes`: the node count of Redis' rax holding these master IDs (fixed 16-byte
    /// keys with inlined leaves): the root, every branching prefix, and every child of a
    /// branching prefix that is not a full key.
    pub fn rax_nodes(&self) -> usize {
        self.rax.live
    }

    pub fn mem(&self) -> usize {
        self.mem
    }

    /// The next auto-generated ID (`streamNextID`): `now` if it is past the last ID, otherwise
    /// the last ID's successor (`None` when the stream has exhausted the ID space).
    pub fn next_id(&self, now_ms: u64) -> Option<StreamId> {
        if now_ms > self.last_id.ms { Some(StreamId::new(now_ms, 0)) } else { self.last_id.next() }
    }

    /// Append an entry. The caller has checked `id > last_id`. Node limits are
    /// `stream-node-max-bytes` / `stream-node-max-entries`.
    pub fn append<F: AsRef<[u8]>>(&mut self, id: StreamId, fv: &[F], max_bytes: usize, max_entries: usize) {
        let nfields = fv.len() / 2;
        let totelelen: u64 = fv.iter().map(|x| x.as_ref().len() as u64).sum();
        let node_max = if max_bytes == 0 || max_bytes as u64 > LISTPACK_MAX { LISTPACK_MAX } else { max_bytes as u64 };
        let mut new_node = true;
        if let Some((_, tail)) = self.nodes.iter().next_back() {
            new_node = if tail.lp_bytes() + totelelen >= node_max {
                true
            } else {
                max_entries != 0 && (tail.count as u64 + tail.deleted as u64) >= max_entries as u64
            };
        }
        if new_node {
            let mut fields = Pack::new();
            let mut body = lp_int(nfields as i64) + lp_int(0);
            for i in 0..nfields {
                let f = fv[i * 2].as_ref();
                fields.push_back(f);
                body += lp_str(f);
            }
            fields.shrink_to_fit();
            let node = Node { fields, count: 0, deleted: 0, body, last: id, data: Vec::new() };
            self.mem += node.mem();
            if let Some((_, tail)) = self.nodes.iter_mut().next_back() {
                let before = tail.mem();
                tail.data.shrink_to_fit();
                self.mem = self.mem + tail.mem() - before;
            }
            self.nodes.insert(id, node);
            self.rax.insert(&id.to_be_bytes());
        }
        let (&master, node) = self.nodes.iter_mut().next_back().expect("tail node");
        let old_mem = node.mem();
        let same = nfields == node.fields.len() && node.fields.iter().zip(fv.iter().step_by(2)).all(|(a, b)| a == b.as_ref());
        let flags = if same { FLAG_SAMEFIELDS } else { 0 };
        // Listpack accounting (flags, ms-diff, seq-diff, [num-fields], [fields], values, lp-count).
        let mut lp = lp_int(flags as i64) + lp_int(id.ms.wrapping_sub(master.ms) as i64) + lp_int(id.seq.wrapping_sub(master.seq) as i64);
        let lp_count = if same { nfields as i64 + 3 } else { 2 * nfields as i64 + 4 };
        if !same {
            lp += lp_int(nfields as i64);
        }
        // Our own encoding.
        let d = &mut node.data;
        d.push(flags);
        put_varint(d, id.ms - master.ms);
        put_varint(d, id.seq);
        if !same {
            put_varint(d, nfields as u64);
        }
        for i in 0..nfields {
            let f = fv[i * 2].as_ref();
            let v = fv[i * 2 + 1].as_ref();
            if !same {
                put_bytes(d, f);
                lp += lp_str(f);
            }
            put_bytes(d, v);
            lp += lp_str(v);
        }
        lp += lp_int(lp_count);
        node.body += lp;
        node.count += 1;
        node.last = id;
        self.mem = self.mem + node.mem() - old_mem;
        self.length += 1;
        if self.entries_added < i64::MAX as u64 {
            self.entries_added += 1;
        }
        self.last_id = id;
        if self.length == 1 {
            self.first_id = id;
        }
    }

    /// Entries with `start <= id <= end`, tombstones skipped.
    pub fn range(&self, start: StreamId, end: StreamId) -> Range<'_> {
        Range::new(self, start, end, true)
    }

    /// Entries with `start <= id <= end` from the end, tombstones skipped.
    pub fn rev_range(&self, start: StreamId, end: StreamId) -> RevRange<'_> {
        RevRange::new(self, start, end, true)
    }

    /// Like [`Entries::range`] but including tombstones.
    pub fn range_raw(&self, start: StreamId, end: StreamId) -> Range<'_> {
        Range::new(self, start, end, false)
    }

    pub fn get(&self, id: StreamId) -> Option<EntryRef<'_>> {
        self.range(id, id).next()
    }

    /// Does a live entry with this ID exist (`streamEntryExists`)?
    pub fn exists(&self, id: StreamId) -> bool {
        self.get(id).is_some()
    }

    /// First live entry ID.
    pub fn first_valid_id(&self) -> Option<StreamId> {
        self.range(StreamId::MIN, StreamId::MAX).next().map(|e| e.id)
    }

    /// Last live entry ID (`streamLastValidID`).
    pub fn last_valid_id(&self) -> Option<StreamId> {
        self.rev_range(StreamId::MIN, StreamId::MAX).next().map(|e| e.id)
    }

    /// First entry ID including tombstones (`streamGetEdgeID(s,1,0)`), `MAX` when there are no
    /// nodes.
    pub fn first_raw_id(&self) -> StreamId {
        self.range_raw(StreamId::MIN, StreamId::MAX).next().map(|e| e.id).unwrap_or(StreamId::MAX)
    }

    /// Recompute `first_id` after deletions (`streamGetEdgeID(s,1,1)`).
    pub fn refresh_first_id(&mut self) {
        self.first_id = if self.length == 0 { StreamId::MIN } else { self.first_valid_id().unwrap_or(StreamId::MAX) };
    }

    /// Delete one live entry (marking a tombstone, or dropping its node if it was the last live
    /// entry there). Does not touch `first_id` / `max_deleted` (callers do, like Redis).
    pub fn delete(&mut self, id: StreamId) -> bool {
        let Some((&master, node)) = self.nodes.range_mut(..=id).next_back() else { return false };
        let mut p = 0;
        let mut at = None;
        while p < node.data.len() {
            let (e, next) = node.decode(master, p);
            if e.id == id {
                if !e.deleted {
                    at = Some(p);
                }
                break;
            }
            if e.id > id {
                break;
            }
            p = next;
        }
        let Some(p) = at else { return false };
        if node.count == 1 {
            let m = node.mem();
            self.nodes.remove(&master);
            self.rax.remove(&master.to_be_bytes());
            self.mem -= m;
        } else {
            node.data[p] |= FLAG_DELETED;
            node.count -= 1;
            node.deleted += 1;
        }
        self.length -= 1;
        true
    }

    /// Note an explicitly deleted ID for `max-deleted-entry-id`.
    pub fn note_deleted(&mut self, id: StreamId) {
        if id > self.max_deleted {
            self.max_deleted = id;
        }
    }

    // ---- consumer group helpers that depend on the log ----

    /// `streamRangeHasTombstones`: could `[start, end]` contain a deleted entry?
    pub fn range_has_tombstones(&self, start: Option<StreamId>, end: Option<StreamId>) -> bool {
        if self.length == 0 || self.max_deleted.is_zero() {
            return false;
        }
        let s = start.unwrap_or(StreamId::MIN);
        let e = end.unwrap_or(StreamId::MAX);
        s <= self.max_deleted && self.max_deleted <= e
    }

    /// `streamEstimateDistanceFromFirstEverEntry`: the logical read counter of `id`, or -1 when
    /// it can't be known.
    pub fn estimate_distance(&self, id: StreamId) -> i64 {
        if self.entries_added == 0 {
            return 0;
        }
        if self.length == 0 && id <= self.last_id {
            return self.entries_added as i64;
        }
        if !id.is_zero() && id < self.max_deleted {
            return -1;
        }
        if id == self.last_id {
            return self.entries_added as i64;
        } else if id > self.last_id {
            return -1;
        }
        if self.max_deleted.is_zero() || self.max_deleted < self.first_id {
            if id < self.first_id {
                return (self.entries_added - self.length) as i64;
            } else if id == self.first_id {
                return (self.entries_added - self.length + 1) as i64;
            }
        }
        -1
    }

    /// A group's lag (`streamReplyWithCGLag`), `None` when unknown.
    pub fn lag(&self, g: &Group) -> Option<i64> {
        if self.entries_added == 0 || self.length == 0 {
            return Some(0);
        }
        if g.last_id < self.first_id && self.max_deleted < self.first_id {
            return Some(self.length as i64);
        }
        if g.entries_read != -1 && !self.range_has_tombstones(Some(g.last_id), None) {
            return Some(self.entries_added as i64 - g.entries_read);
        }
        let er = self.estimate_distance(g.last_id);
        if er != -1 { Some(self.entries_added as i64 - er) } else { None }
    }

    // ---- RDB support ----

    /// Every node with its raw entries (tombstones included), in ID order.
    pub fn raw_nodes(&self) -> impl Iterator<Item = RawNode<'_>> {
        self.nodes.iter().map(|(&master, node)| RawNode {
            master,
            master_fields: node.fields.iter().collect(),
            count: node.count,
            deleted: node.deleted,
            node,
        })
    }

    /// Rebuild a node (RDB load). Entries must be in increasing ID order and greater than any
    /// existing entry; `master_fields` are the node's master entry fields. Updates `length` but
    /// not the ID metadata (set it with the setters afterwards).
    pub fn load_node<'a, I, F>(&mut self, master: StreamId, master_fields: &[&[u8]], entries: I)
    where
        I: IntoIterator<Item = (StreamId, bool, F)>,
        F: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        let mut fields = Pack::new();
        let mut body = lp_int(master_fields.len() as i64) + lp_int(0);
        for f in master_fields {
            fields.push_back(f);
            body += lp_str(f);
        }
        let mut node = Node { fields, count: 0, deleted: 0, body, last: master, data: Vec::new() };
        for (id, deleted, fv) in entries {
            let fv: Vec<(&[u8], &[u8])> = fv.into_iter().collect();
            let same = fv.len() == node.fields.len() && node.fields.iter().zip(fv.iter()).all(|(a, (b, _))| a == *b);
            let mut flags = if same { FLAG_SAMEFIELDS } else { 0 };
            if deleted {
                flags |= FLAG_DELETED;
            }
            let n = fv.len();
            let mut lp = lp_int(flags as i64) + lp_int(id.ms.wrapping_sub(master.ms) as i64) + lp_int(id.seq.wrapping_sub(master.seq) as i64);
            let d = &mut node.data;
            d.push(flags);
            put_varint(d, id.ms.wrapping_sub(master.ms));
            put_varint(d, id.seq);
            if !same {
                put_varint(d, n as u64);
                lp += lp_int(n as i64);
            }
            for (f, v) in &fv {
                if !same {
                    put_bytes(d, f);
                    lp += lp_str(f);
                }
                put_bytes(d, v);
                lp += lp_str(v);
            }
            lp += lp_int(if same { n as i64 + 3 } else { 2 * n as i64 + 4 });
            node.body += lp;
            node.last = id;
            if deleted {
                node.deleted += 1;
            } else {
                node.count += 1;
            }
        }
        if node.count == 0 && node.deleted == 0 {
            return;
        }
        node.data.shrink_to_fit();
        self.length += node.count as u64;
        self.mem += node.mem();
        self.nodes.insert(master, node);
        self.rax.insert(&master.to_be_bytes());
    }
}

/// The shape of Redis' radix tree holding the node master IDs (fixed 16-byte keys with values
/// inlined into their leaf parents, as in 8.x). Only the structure is kept — enough to report
/// `radix-tree-nodes` exactly, which depends on the insert/delete history (deletions don't
/// always recompress). This mirrors `raxInsertAt` / `raxRemove` in `rax.c`.
#[derive(Clone, Debug)]
struct RaxShape {
    nodes: Vec<RaxNode>,
    free: Vec<u32>,
    head: u32,
    live: usize,
    /// The head's allocation after its last key was removed in place (`raxRemove` empties a
    /// leaf parent head without reallocating it); 0 while it is sized by its contents.
    head_alloc: usize,
}

#[derive(Clone, Debug, Default)]
struct RaxNode {
    compr: bool,
    /// Edge bytes (sorted) or the compressed string.
    data: Vec<u8>,
    /// Child nodes; empty for leaf parents (whose slots hold the values).
    kids: Vec<u32>,
}

const RAX_KEY_LEN: usize = 16;

impl Default for RaxShape {
    fn default() -> Self {
        RaxShape { nodes: vec![RaxNode::default()], free: Vec::new(), head: 0, live: 1, head_alloc: 0 }
    }
}

/// Where a walk stopped.
struct RaxStop {
    node: u32,
    consumed: usize,
    /// Split position in a compressed node, or the slot of a found key.
    pos: usize,
    found: bool,
    /// Parents walked through, with the child index taken.
    stack: Vec<(u32, usize)>,
}

impl RaxShape {
    fn alloc(&mut self, n: RaxNode) -> u32 {
        self.live += 1;
        match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = n;
                i
            }
            None => {
                self.nodes.push(n);
                (self.nodes.len() - 1) as u32
            }
        }
    }

    fn release(&mut self, i: u32) {
        self.live -= 1;
        self.nodes[i as usize] = RaxNode::default();
        self.free.push(i);
    }

    /// What Redis allocates for the tree's nodes.
    fn alloc_size(&self) -> usize {
        let mut total = 0;
        let mut stack = vec![self.head];
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i as usize];
            total += match self.head_alloc {
                a if a != 0 && i == self.head => a,
                _ => rax::node_alloc(n.data.len(), n.compr, false),
            };
            stack.extend_from_slice(&n.kids);
        }
        total
    }

    fn step(&self, i: u32) -> usize {
        let n = &self.nodes[i as usize];
        if n.compr { n.data.len() } else { 1 }
    }

    /// `raxLowWalk`.
    fn walk(&self, key: &[u8; RAX_KEY_LEN]) -> RaxStop {
        let mut h = self.head;
        let mut i = 0;
        let mut j = 0;
        let mut stack = Vec::new();
        while !self.nodes[h as usize].data.is_empty() && i < RAX_KEY_LEN {
            let n = &self.nodes[h as usize];
            if n.compr {
                j = 0;
                while j < n.data.len() && i < RAX_KEY_LEN && n.data[j] == key[i] {
                    j += 1;
                    i += 1;
                }
                if j != n.data.len() {
                    break;
                }
            } else {
                match n.data.iter().position(|&c| c == key[i]) {
                    Some(p) => j = p,
                    None => {
                        j = n.data.len();
                        break;
                    }
                }
                i += 1;
            }
            let idx = if n.compr { 0 } else { j };
            if i == RAX_KEY_LEN {
                return RaxStop { node: h, consumed: i, pos: idx, found: true, stack };
            }
            stack.push((h, idx));
            h = n.kids[idx];
            j = 0;
        }
        RaxStop { node: h, consumed: i, pos: j, found: false, stack }
    }

    fn insert(&mut self, key: &[u8; RAX_KEY_LEN]) {
        let stop = self.walk(key);
        if stop.found {
            return;
        }
        // Inserting into the emptied head compresses it, reallocating it to size.
        self.head_alloc = 0;
        let (mut h, mut i, j) = (stop.node, stop.consumed, stop.pos);
        if self.nodes[h as usize].compr {
            // Split the compressed node at `j` (ALGO 1).
            let old = std::mem::take(&mut self.nodes[h as usize]);
            let next = old.kids.first().copied();
            let postfix_len = old.data.len() - j - 1;
            let postfix = if postfix_len > 0 {
                let n = RaxNode { compr: postfix_len > 1, data: old.data[j + 1..].to_vec(), kids: next.into_iter().collect() };
                Some(self.alloc(n))
            } else {
                next
            };
            let split = RaxNode { compr: false, data: vec![old.data[j]], kids: postfix.into_iter().collect() };
            if j == 0 {
                self.nodes[h as usize] = split;
            } else {
                let sp = self.alloc(split);
                self.nodes[h as usize] = RaxNode { compr: j > 1, data: old.data[..j].to_vec(), kids: vec![sp] };
                h = sp;
            }
        }
        loop {
            let n = &mut self.nodes[h as usize];
            if n.data.is_empty() && RAX_KEY_LEN - i > 1 {
                n.compr = true;
                n.data = key[i..].to_vec();
                n.kids.clear();
                return;
            }
            let pos = n.data.iter().position(|&c| c > key[i]).unwrap_or(n.data.len());
            if i + 1 == RAX_KEY_LEN {
                n.data.insert(pos, key[i]);
                return;
            }
            let child = self.alloc(RaxNode::default());
            let n = &mut self.nodes[h as usize];
            n.data.insert(pos, key[i]);
            n.kids.insert(pos, child);
            h = child;
            i += 1;
        }
    }

    fn remove(&mut self, key: &[u8; RAX_KEY_LEN]) {
        let RaxStop { node: h, pos: slot, found, mut stack, .. } = self.walk(key);
        if !found {
            return;
        }
        {
            let n = &mut self.nodes[h as usize];
            if !n.compr && n.data.len() > 1 {
                n.data.remove(slot);
                return;
            }
            if h == self.head {
                self.head_alloc = rax::node_alloc(n.data.len(), n.compr, false);
            }
            *n = RaxNode::default();
        }
        let mut cur = h;
        let mut child = None;
        while cur != self.head {
            child = Some(cur);
            self.release(cur);
            cur = stack.pop().expect("non-head node has a parent").0;
            let n = &self.nodes[cur as usize];
            if !n.compr && n.data.len() != 1 {
                break;
            }
        }
        let Some(child) = child else { return };
        {
            let n = &mut self.nodes[cur as usize];
            if n.compr {
                *n = RaxNode::default();
            } else {
                let p = n.kids.iter().position(|&k| k == child).expect("child of parent");
                n.kids.remove(p);
                n.data.remove(p);
            }
            if n.data.len() != 1 {
                return;
            }
        }
        // Recompression: climb to the topmost single-child ancestor, then merge the chain below
        // it, stopping before a multi-child node or a leaf parent.
        let mut start = cur;
        let parent = loop {
            match stack.pop() {
                None => break None,
                Some((p, _)) => {
                    let n = &self.nodes[p as usize];
                    if !n.compr && n.data.len() != 1 {
                        break Some(p);
                    }
                    start = p;
                }
            }
        };
        let mut depth: usize = stack.iter().map(|&(n, _)| self.step(n)).sum();
        if let Some(p) = parent {
            depth += self.step(p);
        }
        let mut chain = vec![start];
        let mut h = start;
        while !self.nodes[h as usize].data.is_empty() {
            depth += self.step(h);
            h = self.nodes[h as usize].kids[0];
            let n = &self.nodes[h as usize];
            if (!n.compr && n.data.len() != 1) || depth + self.step(h) == RAX_KEY_LEN {
                break;
            }
            chain.push(h);
        }
        if chain.len() > 1 {
            let mut data = Vec::new();
            for &c in &chain {
                data.extend_from_slice(&self.nodes[c as usize].data);
            }
            for &c in &chain[1..] {
                self.release(c);
            }
            self.nodes[start as usize] = RaxNode { compr: true, data, kids: vec![h] };
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Iterators
// ---------------------------------------------------------------------------------------------

pub struct Range<'a> {
    nodes: std::collections::btree_map::Range<'a, StreamId, Node>,
    cur: Option<(StreamId, &'a Node, usize)>,
    start: StreamId,
    end: StreamId,
    skip_tombstones: bool,
    done: bool,
}

impl<'a> Range<'a> {
    fn new(e: &'a Entries, start: StreamId, end: StreamId, skip_tombstones: bool) -> Self {
        let from = if start.is_zero() { None } else { e.nodes.range(..=start).next_back().map(|(k, _)| *k) };
        let nodes = match from {
            Some(k) => e.nodes.range(k..),
            None => e.nodes.range(..),
        };
        Range { nodes, cur: None, start, end, skip_tombstones, done: start > end }
    }
}

impl<'a> Iterator for Range<'a> {
    type Item = EntryRef<'a>;
    fn next(&mut self) -> Option<EntryRef<'a>> {
        if self.done {
            return None;
        }
        loop {
            if self.cur.is_none() {
                let (k, n) = self.nodes.next()?;
                self.cur = Some((*k, n, 0));
            }
            let (master, node, pos) = self.cur.unwrap();
            if pos >= node.data.len() {
                self.cur = None;
                continue;
            }
            let (e, next) = node.decode(master, pos);
            self.cur = Some((master, node, next));
            if e.id >= self.start && !(self.skip_tombstones && e.deleted) {
                if e.id > self.end {
                    self.done = true;
                    return None;
                }
                return Some(e);
            }
        }
    }
}

pub struct RevRange<'a> {
    nodes: std::iter::Rev<std::collections::btree_map::Range<'a, StreamId, Node>>,
    cur: Option<(StreamId, &'a Node, Vec<usize>)>,
    start: StreamId,
    end: StreamId,
    skip_tombstones: bool,
    done: bool,
}

impl<'a> RevRange<'a> {
    fn new(e: &'a Entries, start: StreamId, end: StreamId, skip_tombstones: bool) -> Self {
        let from = if end.is_zero() { None } else { e.nodes.range(..=end).next_back().map(|(k, _)| *k) };
        let nodes = match from {
            Some(k) => e.nodes.range(..=k).rev(),
            None => e.nodes.range(..).rev(),
        };
        RevRange { nodes, cur: None, start, end, skip_tombstones, done: start > end }
    }
}

impl<'a> Iterator for RevRange<'a> {
    type Item = EntryRef<'a>;
    fn next(&mut self) -> Option<EntryRef<'a>> {
        if self.done {
            return None;
        }
        loop {
            if self.cur.is_none() {
                let (k, n) = self.nodes.next()?;
                self.cur = Some((*k, n, n.positions(*k)));
            }
            let (master, node, positions) = self.cur.as_mut().unwrap();
            let Some(pos) = positions.pop() else {
                self.cur = None;
                continue;
            };
            let (e, _) = node.decode(*master, pos);
            if e.id <= self.end && !(self.skip_tombstones && e.deleted) {
                if e.id < self.start {
                    self.done = true;
                    return None;
                }
                return Some(e);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Consumer groups
// ---------------------------------------------------------------------------------------------

/// Position in the time-ordered PEL list: NACK zone first (FIFO), then by delivery time with
/// insertion order breaking ties.
type OrderKey = (bool, i64, u64);

/// A pending entry (`streamNACK`).
#[derive(Clone, Debug)]
pub struct Nack {
    pub delivery_time: i64,
    pub delivery_count: u64,
    /// `None` for entries released with `XNACK` or created by `XCLAIM … FORCE` before assignment.
    pub consumer: Option<Str>,
    key: OrderKey,
}

#[derive(Clone, Debug)]
pub struct Consumer {
    pub seen_time: i64,
    /// -1 until the consumer successfully read or claimed something.
    pub active_time: i64,
    pel: BTreeSet<StreamId>,
    /// Has had pending entries (its emptied PEL tree keeps a node's allocation).
    had_pending: bool,
}

impl Consumer {
    pub fn pending(&self) -> usize {
        self.pel.len()
    }
    pub fn pel(&self) -> impl DoubleEndedIterator<Item = StreamId> + '_ {
        self.pel.iter().copied()
    }
    pub fn pel_from(&self, start: StreamId) -> impl Iterator<Item = StreamId> + '_ {
        self.pel.range(start..).copied()
    }
}

const NACK_OVERHEAD: usize = std::mem::size_of::<Nack>() + 2 * std::mem::size_of::<StreamId>() + 64;
const CONSUMER_OVERHEAD: usize = std::mem::size_of::<Consumer>() + 48;
const GROUP_OVERHEAD: usize = std::mem::size_of::<Group>() + 48;

#[derive(Clone, Debug)]
pub struct Group {
    pub last_id: StreamId,
    /// -1 when invalid (`SCG_INVALID_ENTRIES_READ`).
    pub entries_read: i64,
    pel: BTreeMap<StreamId, Nack>,
    order: BTreeMap<OrderKey, StreamId>,
    seq: u64,
    nacked: usize,
    consumers: BTreeMap<Str, Consumer>,
    mem: usize,
    /// Has had pending entries (Redis then keeps the stream's `cgroups_ref` tree).
    nacked_ever: bool,
}

impl Group {
    pub fn new(last_id: StreamId, entries_read: i64) -> Self {
        Group {
            last_id,
            entries_read,
            pel: BTreeMap::new(),
            order: BTreeMap::new(),
            seq: 0,
            nacked: 0,
            consumers: BTreeMap::new(),
            mem: GROUP_OVERHEAD,
            nacked_ever: false,
        }
    }

    pub fn mem(&self) -> usize {
        self.mem
    }

    pub fn pel_len(&self) -> usize {
        self.pel.len()
    }

    pub fn pel(&self) -> impl DoubleEndedIterator<Item = (StreamId, &Nack)> {
        self.pel.iter().map(|(k, v)| (*k, v))
    }

    pub fn pel_range(&self, start: StreamId, end: StreamId) -> impl Iterator<Item = (StreamId, &Nack)> {
        let it = if start <= end { Some(self.pel.range(start..=end)) } else { None };
        it.into_iter().flatten().map(|(k, v)| (*k, v))
    }

    /// First pending ID `>= from`.
    pub fn pel_next(&self, from: StreamId) -> Option<StreamId> {
        self.pel.range(from..).next().map(|(k, _)| *k)
    }

    pub fn nack(&self, id: StreamId) -> Option<&Nack> {
        self.pel.get(&id)
    }

    /// Pending entries in delivery order (NACK zone first, then oldest delivery first).
    pub fn by_time(&self) -> impl Iterator<Item = StreamId> + '_ {
        self.order.values().copied()
    }

    /// The `n`-th entry in delivery order after `key` — used to walk the list while editing it.
    fn order_after(&self, key: Option<OrderKey>) -> Option<(OrderKey, StreamId)> {
        match key {
            None => self.order.iter().next().map(|(k, v)| (*k, *v)),
            Some(k) => self.order.range((std::ops::Bound::Excluded(k), std::ops::Bound::Unbounded)).next().map(|(k, v)| (*k, *v)),
        }
    }

    /// The time-order position of a pending entry (for walking with [`Group::by_time_after`]).
    pub fn order_key(&self, id: StreamId) -> Option<(bool, i64, u64)> {
        self.pel.get(&id).map(|n| n.key)
    }

    /// The pending entry following position `key` in delivery order (`None` = from the head).
    pub fn by_time_after(&self, key: Option<(bool, i64, u64)>) -> Option<((bool, i64, u64), StreamId)> {
        self.order_after(key)
    }

    /// The last position in delivery order (the list tail).
    pub fn order_tail(&self) -> Option<(bool, i64, u64)> {
        self.order.keys().next_back().copied()
    }

    /// Entries in the NACK zone (`nacked-count`).
    pub fn nacked_count(&self) -> usize {
        self.nacked
    }

    pub fn consumers(&self) -> impl Iterator<Item = (&[u8], &Consumer)> {
        self.consumers.iter().map(|(k, v)| (k.as_slice(), v))
    }

    pub fn consumer_count(&self) -> usize {
        self.consumers.len()
    }

    pub fn consumer(&self, name: &[u8]) -> Option<&Consumer> {
        self.consumers.get(name)
    }

    pub fn consumer_mut(&mut self, name: &[u8]) -> Option<&mut Consumer> {
        self.consumers.get_mut(name)
    }

    /// Create a consumer (`streamCreateConsumer`); false if it exists.
    pub fn create_consumer(&mut self, name: &[u8], now: i64) -> bool {
        if self.consumers.contains_key(name) {
            return false;
        }
        self.consumers.insert(Str::from_slice(name), Consumer { seen_time: now, active_time: -1, pel: BTreeSet::new(), had_pending: false });
        self.mem += CONSUMER_OVERHEAD + name.len();
        true
    }

    /// Delete a consumer and its pending entries; returns how many it had.
    pub fn delete_consumer(&mut self, name: &[u8]) -> Option<usize> {
        let c = self.consumers.remove(name)?;
        self.mem -= CONSUMER_OVERHEAD + name.len();
        for id in &c.pel {
            if let Some(n) = self.pel.remove(id) {
                self.unlink(&n);
                self.mem -= NACK_OVERHEAD;
            }
        }
        Some(c.pel.len())
    }

    fn unlink(&mut self, n: &Nack) {
        self.order.remove(&n.key);
        if !n.key.0 {
            self.nacked -= 1;
        }
    }

    fn next_key(&mut self, nacked: bool, time: i64) -> OrderKey {
        self.seq += 1;
        if nacked { (false, 0, self.seq) } else { (true, time, self.seq) }
    }

    /// Add a new pending entry at the tail of the delivery list (delivery count 1).
    pub fn add_nack(&mut self, id: StreamId, consumer: Option<&[u8]>, time: i64) {
        self.nacked_ever = true;
        let key = self.next_key(false, time);
        self.order.insert(key, id);
        if let Some(c) = consumer
            && let Some(cons) = self.consumers.get_mut(c)
        {
            cons.pel.insert(id);
            cons.had_pending = true;
        }
        if let Some(old) = self.pel.insert(id, Nack { delivery_time: time, delivery_count: 1, consumer: consumer.map(Str::from_slice), key }) {
            self.order.remove(&old.key);
            if !old.key.0 {
                self.nacked -= 1;
            }
        } else {
            self.mem += NACK_OVERHEAD;
        }
    }

    /// Add a pending entry straight into the NACK zone (`XNACK … FORCE`, RDB load).
    pub fn add_nacked(&mut self, id: StreamId, delivery_count: u64) {
        self.nacked_ever = true;
        let key = self.next_key(true, 0);
        self.order.insert(key, id);
        self.nacked += 1;
        if self.pel.insert(id, Nack { delivery_time: 0, delivery_count, consumer: None, key }).is_none() {
            self.mem += NACK_OVERHEAD;
        }
    }

    /// Insert a pending entry with explicit state, sorted by delivery time (RDB load).
    pub fn load_nack(&mut self, id: StreamId, consumer: Option<&[u8]>, delivery_time: i64, delivery_count: u64) {
        self.add_nack(id, consumer, delivery_time);
        if let Some(n) = self.pel.get_mut(&id) {
            n.delivery_count = delivery_count;
        }
    }

    /// Remove a pending entry (XACK and friends).
    pub fn remove_nack(&mut self, id: StreamId) -> Option<Nack> {
        let n = self.pel.remove(&id)?;
        self.unlink(&n);
        if let Some(c) = &n.consumer
            && let Some(cons) = self.consumers.get_mut(c.as_slice())
        {
            cons.pel.remove(&id);
        }
        self.mem -= NACK_OVERHEAD;
        Some(n)
    }

    /// Give a pending entry to `consumer` (which must exist).
    pub fn assign(&mut self, id: StreamId, consumer: &[u8]) {
        let Some(n) = self.pel.get_mut(&id) else { return };
        if n.consumer.as_deref() == Some(consumer) {
            return;
        }
        if let Some(old) = n.consumer.take()
            && let Some(cons) = self.consumers.get_mut(old.as_slice())
        {
            cons.pel.remove(&id);
        }
        n.consumer = Some(Str::from_slice(consumer));
        if let Some(cons) = self.consumers.get_mut(consumer) {
            cons.pel.insert(id);
            cons.had_pending = true;
        }
    }

    /// Detach a pending entry from its consumer and move it to the end of the NACK zone
    /// (`XNACK`): delivery time becomes 0.
    pub fn release(&mut self, id: StreamId) {
        let Some(n) = self.pel.get(&id) else { return };
        let old_key = n.key;
        let consumer = n.consumer.clone();
        if let Some(c) = consumer
            && let Some(cons) = self.consumers.get_mut(c.as_slice())
        {
            cons.pel.remove(&id);
        }
        self.order.remove(&old_key);
        if !old_key.0 {
            self.nacked -= 1;
        }
        let key = self.next_key(true, 0);
        self.order.insert(key, id);
        self.nacked += 1;
        let n = self.pel.get_mut(&id).unwrap();
        n.consumer = None;
        n.delivery_time = 0;
        n.key = key;
    }

    /// Set the delivery time and reposition in the delivery list (`pelListUpdate`).
    pub fn touch_nack(&mut self, id: StreamId, time: i64) {
        let Some(n) = self.pel.get(&id) else { return };
        let old = n.key;
        self.order.remove(&old);
        if !old.0 {
            self.nacked -= 1;
        }
        let key = self.next_key(false, time);
        self.order.insert(key, id);
        let n = self.pel.get_mut(&id).unwrap();
        n.delivery_time = time;
        n.key = key;
    }

    pub fn set_delivery_count(&mut self, id: StreamId, count: u64) {
        if let Some(n) = self.pel.get_mut(&id) {
            n.delivery_count = count;
        }
    }

    /// Increment the delivery count, saturating at `i64::MAX` like Redis.
    pub fn bump_delivery_count(&mut self, id: StreamId) {
        if let Some(n) = self.pel.get_mut(&id)
            && n.delivery_count != i64::MAX as u64
        {
            n.delivery_count += 1;
        }
    }
}

/// The consumer groups of a stream, by name.
#[derive(Clone, Debug, Default)]
pub struct Groups {
    map: BTreeMap<Str, Group>,
    mem: usize,
    /// Redis made the groups tree (`s->cgroups`, kept once made).
    made: bool,
    /// A removed group had pending entries (`s->cgroups_ref` is kept once made).
    refs_made: bool,
}

/// Mutable access to one group; keeps the groups' memory total in sync on drop.
pub struct GroupMut<'a> {
    group: &'a mut Group,
    total: &'a mut usize,
    old: usize,
}

impl std::ops::Deref for GroupMut<'_> {
    type Target = Group;
    fn deref(&self) -> &Group {
        self.group
    }
}

impl std::ops::DerefMut for GroupMut<'_> {
    fn deref_mut(&mut self) -> &mut Group {
        self.group
    }
}

impl Drop for GroupMut<'_> {
    fn drop(&mut self) {
        *self.total = (*self.total + self.group.mem).saturating_sub(self.old);
    }
}

impl Groups {
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn mem(&self) -> usize {
        self.mem
    }
    pub fn get(&self, name: &[u8]) -> Option<&Group> {
        self.map.get(name)
    }
    pub fn get_mut(&mut self, name: &[u8]) -> Option<GroupMut<'_>> {
        let group = self.map.get_mut(name)?;
        let old = group.mem;
        Some(GroupMut { group, total: &mut self.mem, old })
    }
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Group)> {
        self.map.iter().map(|(k, v)| (k.as_slice(), v))
    }
    /// Create a group; false if the name is taken.
    pub fn create(&mut self, name: &[u8], last_id: StreamId, entries_read: i64) -> bool {
        if self.map.contains_key(name) {
            return false;
        }
        let g = Group::new(last_id, entries_read);
        self.made = true;
        self.mem += g.mem + name.len();
        self.map.insert(Str::from_slice(name), g);
        true
    }
    /// Insert a fully built group (RDB load), replacing any with the same name.
    pub fn insert(&mut self, name: &[u8], group: Group) {
        self.made = true;
        self.mem += group.mem + name.len();
        if let Some(old) = self.map.insert(Str::from_slice(name), group) {
            self.mem -= old.mem + name.len();
        }
    }
    pub fn remove(&mut self, name: &[u8]) -> Option<Group> {
        let g = self.map.remove(name)?;
        self.mem -= g.mem + name.len();
        self.refs_made |= g.nacked_ever;
        Some(g)
    }

    /// What Redis allocates for the groups: the groups tree, each group with its PEL and
    /// consumers trees, pending entries and consumers (with their names and PELs), and the
    /// `cgroups_ref` tree indexing every pending ID. PEL and name trees are sized in the shape
    /// insertions give them.
    fn alloc_size(&self) -> usize {
        // `sizeof(streamCG)`, `streamNACK` and `streamConsumer`.
        const GROUP: usize = 64;
        const NACK: usize = 64;
        const CONSUMER: usize = 32;
        // A PEL tree; one that lost its last entry keeps the head that held it (`raxRemove`
        // empties a leaf parent head in place).
        fn ids<'a>(ids: impl Iterator<Item = &'a StreamId>, used: bool) -> usize {
            let keys: Vec<[u8; 16]> = ids.map(|id| id.to_be_bytes()).collect();
            if keys.is_empty() && used {
                return rax::RAX + rax::node_alloc(16, true, false);
            }
            let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
            rax::alloc(&refs, Some(16))
        }
        if !self.made {
            return 0;
        }
        let names: Vec<&[u8]> = self.map.keys().map(|k| k.as_slice()).collect();
        let mut total = rax::alloc(&names, None);
        let mut pending = BTreeSet::new();
        let mut refs = self.refs_made;
        for g in self.map.values() {
            let consumers: Vec<&[u8]> = g.consumers.keys().map(|k| k.as_slice()).collect();
            total += GROUP + ids(g.pel.keys(), g.nacked_ever) + rax::alloc(&consumers, None) + g.pel.len() * NACK;
            for (name, c) in &g.consumers {
                total += CONSUMER + crate::object::sds_alloc(name.len()) + ids(c.pel.iter(), c.had_pending);
            }
            pending.extend(g.pel.keys().copied());
            refs |= g.nacked_ever;
        }
        if refs {
            total += ids(pending.iter(), true);
        }
        total
    }

    /// The smallest `last_id` across groups (`min_cgroup_last_id`).
    pub fn min_last_id(&self) -> Option<StreamId> {
        self.map.values().map(|g| g.last_id).min()
    }

    /// `streamEntryIsReferenced`: not yet delivered to some group, or pending in one.
    pub fn is_referenced(&self, id: StreamId) -> bool {
        let Some(min) = self.min_last_id() else { return false };
        if min < id {
            return true;
        }
        self.map.values().any(|g| g.pel.contains_key(&id))
    }

    /// `streamCleanupEntryCGroupRefs`: drop `id` from every group's PEL.
    pub fn cleanup_refs(&mut self, id: StreamId) {
        let mut total = self.mem;
        for g in self.map.values_mut() {
            let old = g.mem;
            g.remove_nack(id);
            total = total + g.mem - old;
        }
        self.mem = total;
    }
}

// ---------------------------------------------------------------------------------------------
// IDMP (idempotent producers)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Producer {
    map: HashMap<Box<[u8]>, StreamId>,
    order: VecDeque<(Box<[u8]>, StreamId)>,
}

impl Producer {
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn get(&self, iid: &[u8]) -> Option<StreamId> {
        self.map.get(iid).copied()
    }
    /// Tracked IIDs, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8], StreamId)> {
        self.order.iter().map(|(k, v)| (&k[..], *v))
    }
}

/// Idempotency tracking: producer ID -> recent idempotent IDs and the entries they created.
#[derive(Clone, Debug, Default)]
pub struct Idmp {
    producers: BTreeMap<Str, Producer>,
    mem: usize,
    /// Redis made the producers tree (it stays once made).
    made: bool,
}

const IID_OVERHEAD: usize = 64;

impl Idmp {
    pub fn is_empty(&self) -> bool {
        self.producers.is_empty()
    }

    /// What Redis allocates for IDMP: the producers tree, each producer and its IIDs.
    fn alloc_size(&self) -> usize {
        if !self.made {
            return 0;
        }
        // `sizeof(idmpProducer)` and `idmpEntry` before the IID.
        const PRODUCER: usize = 24;
        const ENTRY: usize = 32;
        let pids: Vec<&[u8]> = self.producers.keys().map(|k| k.as_slice()).collect();
        rax::alloc(&pids, None)
            + self.producers.values().map(|p| PRODUCER + p.order.iter().map(|(iid, _)| jemalloc_size(ENTRY + iid.len())).sum::<usize>()).sum::<usize>()
    }

    pub fn producers(&self) -> impl Iterator<Item = (&[u8], &Producer)> {
        self.producers.iter().map(|(k, v)| (k.as_slice(), v))
    }
    pub fn lookup(&self, pid: &[u8], iid: &[u8]) -> Option<StreamId> {
        self.producers.get(pid).and_then(|p| p.get(iid))
    }
    /// Record `iid -> id` for producer `pid`, evicting the oldest beyond `max`.
    pub fn insert(&mut self, pid: &[u8], iid: &[u8], id: StreamId, max: u64) {
        self.made = true;
        if !self.producers.contains_key(pid) {
            self.producers.insert(Str::from_slice(pid), Producer::default());
            self.mem += 96 + pid.len();
        }
        let p = self.producers.get_mut(pid).unwrap();
        let k: Box<[u8]> = iid.into();
        p.map.insert(k.clone(), id);
        p.order.push_back((k, id));
        self.mem += 2 * (IID_OVERHEAD + iid.len());
        if p.map.len() as u64 > max
            && let Some((old, _)) = p.order.pop_front()
        {
            p.map.remove(&old);
            self.mem -= 2 * (IID_OVERHEAD + old.len());
        }
    }
    /// Make sure a producer exists (Redis creates it before the duplicate check).
    pub fn ensure_producer(&mut self, pid: &[u8]) {
        self.made = true;
        if !self.producers.contains_key(pid) {
            self.producers.insert(Str::from_slice(pid), Producer::default());
            self.mem += 96 + pid.len();
        }
    }
    /// Drop expired IIDs from the head of each producer's list, and empty producers
    /// (`handleExpiredIdmpEntries`). `cutoff_ms`: entries with `id.ms <= cutoff_ms` expire.
    pub fn expire(&mut self, cutoff_ms: u64) -> bool {
        let mut changed = false;
        let mut freed = 0;
        self.producers.retain(|pid, p| {
            while let Some((_, id)) = p.order.front() {
                if id.ms > cutoff_ms {
                    break;
                }
                let (k, _) = p.order.pop_front().unwrap();
                p.map.remove(&k);
                freed += 2 * (IID_OVERHEAD + k.len());
                changed = true;
            }
            if p.order.is_empty() {
                freed += 96 + pid.len();
                changed = true;
                false
            } else {
                true
            }
        });
        self.mem -= freed;
        changed
    }
    /// What `expire(cutoff_ms)` would leave: (producers, IIDs).
    pub fn live_counts(&self, cutoff_ms: u64) -> (usize, usize) {
        let mut pids = 0;
        let mut iids = 0;
        for p in self.producers.values() {
            let gone = p.order.iter().take_while(|(_, id)| id.ms <= cutoff_ms).count();
            let left = p.map.len() - gone.min(p.map.len());
            if left > 0 {
                pids += 1;
                iids += left;
            }
        }
        (pids, iids)
    }
    pub fn clear(&mut self) {
        self.producers.clear();
        self.mem = 0;
    }
    pub fn mem(&self) -> usize {
        self.mem
    }
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Stream {
    pub entries: Entries,
    pub groups: Groups,
    pub idmp: Idmp,
    /// Per-stream IDMP settings (`XCFGSET`), seeded from `stream-idmp-*` on creation.
    pub idmp_duration: u64,
    pub idmp_maxsize: u64,
    pub iids_added: u64,
    pub iids_duplicates: u64,
}

impl Default for Stream {
    fn default() -> Self {
        Stream {
            entries: Entries::default(),
            groups: Groups::default(),
            idmp: Idmp::default(),
            idmp_duration: 100,
            idmp_maxsize: 100,
            iids_added: 0,
            iids_duplicates: 0,
        }
    }
}

impl Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.len() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        self.entries.mem() + self.groups.mem() + self.idmp.mem()
    }

    /// `s->alloc_size`: the stream, its tree of nodes and their listpacks, consumer groups and
    /// IDMP producers.
    pub fn alloc_size(&self) -> usize {
        // `sizeof(stream)`.
        const STREAM: usize = 160;
        let e = &self.entries;
        STREAM + rax::RAX + e.rax.alloc_size() + e.nodes.values().map(|n| n.lp_bytes() as usize).sum::<usize>() + self.groups.alloc_size() + self.idmp.alloc_size()
    }

    pub fn encoding(&self) -> &'static str {
        "stream"
    }

    /// Borrow the log immutably and the groups mutably at once.
    pub fn split_mut(&mut self) -> (&mut Entries, &mut Groups) {
        (&mut self.entries, &mut self.groups)
    }

    /// Trim (`streamTrim`), returning how many entries were removed. With `KeepRef` and
    /// `approx`, only whole nodes go.
    pub fn trim(&mut self, t: &Trim) -> u64 {
        let ent = &mut self.entries;
        let groups = &mut self.groups;
        let mut deleted: u64 = 0;
        let mut cursor: Option<StreamId> = None;
        loop {
            let next = match cursor {
                None => ent.nodes.keys().next().copied(),
                Some(k) => ent.nodes.range((std::ops::Bound::Excluded(k), std::ops::Bound::Unbounded)).next().map(|(k, _)| *k),
            };
            let Some(master) = next else { break };
            cursor = Some(master);
            if let TrimBy::MaxLen(m) = t.by
                && ent.length <= m
            {
                break;
            }
            let node = &ent.nodes[&master];
            let entries = node.count as u64;
            if t.limit != 0 && deleted + entries > t.limit {
                break;
            }
            let eligible = match t.by {
                TrimBy::MaxLen(m) => ent.length - entries >= m,
                TrimBy::MinId(id) => node.last < id,
            };
            if eligible && t.policy == RefPolicy::KeepRef {
                let n = ent.nodes.remove(&master).unwrap();
                ent.rax.remove(&master.to_be_bytes());
                ent.mem -= n.mem();
                ent.length -= entries;
                deleted += entries;
                continue;
            }
            if t.approx && t.policy == RefPolicy::KeepRef {
                break;
            }
            // Trim entries inside the node.
            let mut marks: Vec<usize> = Vec::new();
            {
                let node = &ent.nodes[&master];
                let mut p = 0;
                let mut length = ent.length;
                while p < node.data.len() {
                    let (e, next) = node.decode(master, p);
                    let stop = match t.by {
                        TrimBy::MaxLen(m) => length <= m,
                        TrimBy::MinId(id) => e.id >= id,
                    };
                    if stop {
                        break;
                    }
                    if !e.deleted {
                        let can = match t.policy {
                            RefPolicy::Acked => !groups.is_referenced(e.id),
                            RefPolicy::DelRef => {
                                groups.cleanup_refs(e.id);
                                true
                            }
                            RefPolicy::KeepRef => true,
                        };
                        if can {
                            marks.push(p);
                            length -= 1;
                        }
                    }
                    p = next;
                }
            }
            let dfl = marks.len() as u64;
            ent.length -= dfl;
            deleted += dfl;
            if eligible && dfl == entries {
                let n = ent.nodes.remove(&master).unwrap();
                ent.rax.remove(&master.to_be_bytes());
                ent.mem -= n.mem();
                continue;
            }
            let node = ent.nodes.get_mut(&master).unwrap();
            for p in marks {
                node.data[p] |= FLAG_DELETED;
            }
            node.count -= dfl as u32;
            node.deleted += dfl as u32;
            if eligible {
                continue;
            }
            break;
        }
        if ent.length == 0 {
            ent.first_id = StreamId::MIN;
        } else if deleted > 0 {
            ent.first_id = ent.first_valid_id().unwrap_or(StreamId::MAX);
        }
        deleted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(ms: u64, seq: u64) -> StreamId {
        StreamId::new(ms, seq)
    }

    fn add(s: &mut Stream, i: StreamId, fv: &[&str]) {
        s.entries.append(i, fv, 4096, 100);
    }

    #[test]
    fn append_and_range() {
        let mut s = Stream::new();
        for i in 1..=250 {
            add(&mut s, id(i, 0), &["f", "v"]);
        }
        assert_eq!(s.len(), 250);
        assert_eq!(s.entries.node_count(), 3);
        assert_eq!(s.entries.rax_nodes(), 5);
        let all: Vec<_> = s.entries.range(StreamId::MIN, StreamId::MAX).map(|e| e.id.ms).collect();
        assert_eq!(all, (1..=250).collect::<Vec<_>>());
        let rev: Vec<_> = s.entries.rev_range(id(10, 0), id(20, 0)).map(|e| e.id.ms).collect();
        assert_eq!(rev, (10..=20).rev().collect::<Vec<_>>());
        let e = s.entries.get(id(7, 0)).unwrap();
        assert_eq!(e.fields().collect::<Vec<_>>(), vec![(&b"f"[..], &b"v"[..])]);
    }

    #[test]
    fn mixed_fields() {
        let mut s = Stream::new();
        add(&mut s, id(1, 0), &["a", "1", "b", "2"]);
        add(&mut s, id(1, 1), &["a", "x"]);
        add(&mut s, id(1, 2), &["a", "3", "b", "4"]);
        let v: Vec<Vec<(Vec<u8>, Vec<u8>)>> =
            s.entries.range(StreamId::MIN, StreamId::MAX).map(|e| e.fields().map(|(f, v)| (f.to_vec(), v.to_vec())).collect()).collect();
        assert_eq!(v[1], vec![(b"a".to_vec(), b"x".to_vec())]);
        assert_eq!(v[2], vec![(b"a".to_vec(), b"3".to_vec()), (b"b".to_vec(), b"4".to_vec())]);
    }

    #[test]
    fn delete_and_tombstones() {
        let mut s = Stream::new();
        for i in 1..=5 {
            add(&mut s, id(i, 0), &["f", "v"]);
        }
        assert!(s.entries.delete(id(3, 0)));
        assert!(!s.entries.delete(id(3, 0)));
        assert_eq!(s.len(), 4);
        assert!(!s.entries.exists(id(3, 0)));
        assert_eq!(s.entries.range_raw(StreamId::MIN, StreamId::MAX).count(), 5);
        for i in [1, 2, 4, 5] {
            assert!(s.entries.delete(id(i, 0)));
        }
        assert_eq!(s.entries.node_count(), 0);
        assert_eq!(s.entries.rax_nodes(), 1);
    }

    #[test]
    fn node_split_by_bytes() {
        let mut s = Stream::new();
        let big = "x".repeat(1000);
        for i in 1..=10 {
            s.entries.append(id(i, 0), &["f", big.as_str()], 4096, 100);
        }
        // 7 + count + deleted + master(3 + 2 + 2) and ~1006 bytes per entry: 4 per node.
        assert_eq!(s.entries.node_count(), 3);
    }

    #[test]
    fn rax_count_shapes() {
        let key = |ms: u64| StreamId::new(ms, 0).to_be_bytes();
        let mut r = RaxShape::default();
        assert_eq!(r.live, 1);
        r.insert(&key(1));
        assert_eq!(r.live, 1);
        r.insert(&key(11));
        assert_eq!(r.live, 4);
        r.insert(&key(21));
        assert_eq!(r.live, 5);
        // Deleting doesn't fold the leaf parent back into the compressed prefix.
        r.remove(&key(1));
        assert_eq!(r.live, 4);
        r.remove(&key(11));
        assert_eq!(r.live, 2);
        r.remove(&key(21));
        assert_eq!(r.live, 1);
        r.insert(&key(5));
        assert_eq!(r.live, 1);
        // Keys differing in the last byte share one leaf parent.
        let mut r = RaxShape::default();
        r.insert(&StreamId::new(1, 1).to_be_bytes());
        r.insert(&StreamId::new(1, 2).to_be_bytes());
        assert_eq!(r.live, 2);
        r.remove(&StreamId::new(1, 1).to_be_bytes());
        assert_eq!(r.live, 2);
    }

    /// Randomized model check of ranges, deletes and exact trimming against a BTreeMap.
    #[test]
    fn model_check() {
        let mut rng = crate::util::Rng::seeded(42);
        for round in 0..30 {
            let mut s = Stream::new();
            let mut model: BTreeMap<StreamId, String> = BTreeMap::new();
            let mut ms = 1u64;
            let max_entries = 1 + rng.below(20);
            for step in 0..600 {
                match rng.below(10) {
                    0..=5 => {
                        ms += rng.below(3) as u64;
                        let i = s.entries.next_id(ms).unwrap();
                        let v = format!("v{round}-{step}");
                        let fv: Vec<&str> = if rng.below(2) == 0 { vec!["f", &v] } else { vec!["g", &v, "h", "1"] };
                        s.entries.append(i, &fv, 4096, max_entries);
                        model.insert(i, v);
                    }
                    6 => {
                        if let Some(k) = model.keys().nth(rng.below(model.len().max(1))).copied() {
                            assert!(s.entries.delete(k));
                            model.remove(&k);
                        }
                    }
                    7 => {
                        let keep = rng.below(model.len() + 2) as u64;
                        let n = s.trim(&Trim { by: TrimBy::MaxLen(keep), approx: false, limit: 0, policy: RefPolicy::KeepRef });
                        let mut removed = 0;
                        while model.len() as u64 > keep {
                            let k = *model.keys().next().unwrap();
                            model.remove(&k);
                            removed += 1;
                        }
                        assert_eq!(n, removed);
                    }
                    8 => {
                        let min = StreamId::new(rng.below(ms as usize + 1) as u64, 0);
                        let n = s.trim(&Trim { by: TrimBy::MinId(min), approx: false, limit: 0, policy: RefPolicy::KeepRef });
                        let before = model.len();
                        model.retain(|k, _| *k >= min);
                        assert_eq!(n as usize, before - model.len());
                    }
                    _ => {
                        let a = StreamId::new(rng.below(ms as usize + 1) as u64, rng.below(3) as u64);
                        let b = StreamId::new(rng.below(ms as usize + 1) as u64, rng.below(3) as u64);
                        let got: Vec<StreamId> = s.entries.range(a, b).map(|e| e.id).collect();
                        let want: Vec<StreamId> = if a <= b { model.range(a..=b).map(|(k, _)| *k).collect() } else { vec![] };
                        assert_eq!(got, want);
                        let got: Vec<StreamId> = s.entries.rev_range(a, b).map(|e| e.id).collect();
                        let want: Vec<StreamId> = want.into_iter().rev().collect();
                        assert_eq!(got, want);
                    }
                }
                assert_eq!(s.len(), model.len());
                for (k, v) in model.iter().take(3) {
                    let e = s.entries.get(*k).unwrap();
                    assert_eq!(e.fields().next().unwrap().1, v.as_bytes());
                }
            }
        }
    }

    #[test]
    fn approx_trim_whole_nodes() {
        let mut s = Stream::new();
        for i in 1..=300 {
            add(&mut s, id(i, 0), &["f", "v"]);
        }
        let n = s.trim(&Trim { by: TrimBy::MaxLen(150), approx: true, limit: 10000, policy: RefPolicy::KeepRef });
        assert_eq!(n, 100);
        assert_eq!(s.len(), 200);
        assert_eq!(s.entries.first_id(), id(101, 0));
    }

    #[test]
    fn pel_order() {
        let mut g = Group::new(StreamId::MIN, 0);
        g.create_consumer(b"c", 0);
        g.add_nack(id(1, 0), Some(b"c"), 10);
        g.add_nack(id(2, 0), Some(b"c"), 5);
        g.add_nack(id(3, 0), Some(b"c"), 10);
        assert_eq!(g.by_time().collect::<Vec<_>>(), vec![id(2, 0), id(1, 0), id(3, 0)]);
        g.release(id(3, 0));
        g.release(id(1, 0));
        assert_eq!(g.by_time().collect::<Vec<_>>(), vec![id(3, 0), id(1, 0), id(2, 0)]);
        assert_eq!(g.nacked_count(), 2);
        assert_eq!(g.consumer(b"c").unwrap().pending(), 1);
        g.touch_nack(id(3, 0), 20);
        assert_eq!(g.nacked_count(), 1);
        assert_eq!(g.by_time().collect::<Vec<_>>(), vec![id(1, 0), id(2, 0), id(3, 0)]);
        assert_eq!(g.delete_consumer(b"c"), Some(1));
        assert_eq!(g.pel_len(), 2);
    }
}
