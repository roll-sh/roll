//! `PMap`: an ordered map from oids, shared structurally by a catalog's copies.
//!
//! Every DDL statement works on a copy of the catalog (and makes a few more: binding defaults
//! and checks over the statement's catalog, a frozen copy for the dependency walk), and its
//! commit compares the copy with the catalog it started from. With a plain `BTreeMap` behind
//! an `Arc`, the first change to a copy duplicated the whole map, and the comparison visited
//! every entry: O(objects) per statement. A `PMap` keeps its entries in chunks of
//! `CHUNK` consecutive oids, each behind an `Arc`, under an `Arc`'d index of the chunks: a
//! copy shares everything, a change copies the index (an `Arc` per chunk) and one chunk, and
//! two maps' differences are found by comparing chunk pointers first (`diff`), so a statement
//! pays for the objects it changed, not for the catalog's size.
//!
//! It behaves as a `BTreeMap<u32, V>` (iteration in key order, the same serde form: a map),
//! with the methods the catalog uses.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use bytes::Bytes;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

/// log2 of the oids per chunk. Oids of one database's objects are mostly consecutive (a
/// table takes a few: its row type, array type, constraints, index, sequence), so chunks are
/// well filled, and a chunk to copy stays small.
const SHIFT: u32 = 6;

/// log2 of the chunks per group. The chunks are indexed by groups of `1 << GROUP_SHIFT`
/// chunks, each behind an `Arc` too: a change copies the group index (one `Arc` per 4,096
/// oids), one group (at most 64 `Arc`s) and one chunk, and a diff compares groups by pointer
/// before chunks. (With one flat index, 10,000 tables' oids made a 1,100-entry index that
/// every change copied and dropped and every diff walked: most of an ALTER TABLE's own time.)
/// The levels are sorted vectors: small to copy and to keep (no tree nodes per entry), found
/// by binary search, and iterated by slice iterators (an iterator on every frame of deep
/// recursion that reads a catalog map stays small).
const GROUP_SHIFT: u32 = 6;

type Chunk<V> = Arc<ChunkData<V>>;
type Group<V> = Arc<GroupData<V>>;

/// Parses a stored entry (a record's bytes: `persist`'s JSON).
pub type Parse<V> = fn(&[u8]) -> crate::error::Result<V>;

/// Reads the records of key range [from, to) in key order: the database's store at its
/// latest commit (`persist::load`).
pub type Reader = Arc<dyn Fn(&[u8], &[u8]) -> crate::error::Result<Vec<(Vec<u8>, Bytes)>> + Send + Sync>;

/// Where a map read on demand (`PMap::stored`) finds its records: under `prefix` followed
/// by the entry's key (big-endian), up to `end`.
pub struct Source<V> {
    pub prefix: Vec<u8>,
    pub end: Vec<u8>,
    pub read: Reader,
    pub parse: Parse<V>,
}

/// A group of chunks: made, or a stored map's group not read yet, read from the store on
/// first use. A group is read before anything changes it (a change copies its chunks), so
/// what it reads later is what it had when the map was opened: a commit that writes a
/// group's records has read it first.
pub struct GroupData<V> {
    chunks: OnceLock<Vec<(u32, Chunk<V>)>>,
    src: Option<(Arc<Source<V>>, u32)>,
}

/// A chunk's entries: made, or records read with their group and parsed on first use.
pub struct ChunkData<V> {
    entries: OnceLock<Vec<(u32, V)>>,
    raw: Mutex<Vec<(u32, Bytes)>>,
    parse: Option<Parse<V>>,
}

fn unreadable(e: crate::error::Error) -> ! {
    panic!("corrupt or unreadable catalog records: {e}")
}

impl<V> ChunkData<V> {
    fn ready(entries: Vec<(u32, V)>) -> Self {
        ChunkData {
            entries: OnceLock::from(entries),
            raw: Mutex::new(Vec::new()),
            parse: None,
        }
    }

    fn entries(&self) -> &[(u32, V)] {
        self.entries.get_or_init(|| {
            let raw = std::mem::take(&mut *self.raw.lock());
            let parse = self.parse.expect("a chunk to parse has its parser");
            raw.iter()
                .map(|(k, b)| (*k, parse(b).unwrap_or_else(|e| unreadable(e))))
                .collect()
        })
    }

    /// Whether it has key `k`, without parsing it.
    fn has(&self, k: u32) -> bool {
        match self.entries.get() {
            Some(e) => find(e, k).is_ok(),
            None => {
                let raw = self.raw.lock();
                // (Parsed meanwhile: the raw records were taken.)
                if raw.is_empty() {
                    drop(raw);
                    return find(self.entries(), k).is_ok();
                }
                raw.binary_search_by_key(&k, |e| e.0).is_ok()
            }
        }
    }

    /// Its last key, without parsing it.
    fn last_key(&self) -> Option<u32> {
        match self.entries.get() {
            Some(e) => e.last().map(|e| e.0),
            None => {
                let last = self.raw.lock().last().map(|e| e.0);
                // (Taken by a parse under way: its entries.)
                last.or_else(|| self.entries().last().map(|e| e.0))
            }
        }
    }
}

impl<V> GroupData<V> {
    fn ready(chunks: Vec<(u32, Chunk<V>)>) -> Self {
        GroupData {
            chunks: OnceLock::from(chunks),
            src: None,
        }
    }

    fn chunks(&self) -> &[(u32, Chunk<V>)] {
        self.chunks.get_or_init(|| match &self.src {
            Some((src, g)) => read_group(src, *g),
            None => Vec::new(),
        })
    }
}

/// Group `g`'s records, as chunks to parse.
fn read_group<V>(src: &Source<V>, g: u32) -> Vec<(u32, Chunk<V>)> {
    let lo = (g as u64) << (SHIFT + GROUP_SHIFT);
    let hi = lo + (1 << (SHIFT + GROUP_SHIFT));
    let key = |k: u64| [&src.prefix[..], &(k as u32).to_be_bytes()].concat();
    let to = if hi > u32::MAX as u64 { src.end.clone() } else { key(hi) };
    let records = (src.read)(&key(lo), &to).unwrap_or_else(|e| unreadable(e));
    chunks_of(src, records.into_iter().map(|(k, v)| (record_oid(src, &k), v)))
}

/// A stored record's key (its oid after the prefix).
fn record_oid<V>(src: &Source<V>, k: &[u8]) -> u32 {
    let Some(o) = k.get(src.prefix.len()..).and_then(|b| <[u8; 4]>::try_from(b).ok()) else {
        unreadable(crate::error::internal("bad record key"));
    };
    u32::from_be_bytes(o)
}

/// Records in key order, as chunks to parse.
fn chunks_of<V>(src: &Source<V>, records: impl Iterator<Item = (u32, Bytes)>) -> Vec<(u32, Chunk<V>)> {
    let mut out: Vec<(u32, Chunk<V>)> = Vec::new();
    let mut raw: Vec<(u32, Bytes)> = Vec::new();
    let flush = |raw: &mut Vec<(u32, Bytes)>, out: &mut Vec<(u32, Chunk<V>)>| {
        if let Some(c) = raw.first().map(|e| e.0 >> SHIFT) {
            let data = ChunkData {
                entries: OnceLock::new(),
                raw: Mutex::new(std::mem::take(raw)),
                parse: Some(src.parse),
            };
            out.push((c, Arc::new(data)));
        }
    };
    for (o, v) in records {
        if raw.first().is_some_and(|e| e.0 >> SHIFT != o >> SHIFT) {
            flush(&mut raw, &mut out);
        }
        raw.push((o, v));
    }
    flush(&mut raw, &mut out);
    out
}

/// `c`'s entries, to change: copied unless the chunk is this map's alone.
fn chunk_mut<V: Clone>(c: &mut Chunk<V>) -> &mut Vec<(u32, V)> {
    c.entries();
    if Arc::get_mut(c).is_none() {
        *c = Arc::new(ChunkData::ready(c.entries().to_vec()));
    }
    Arc::get_mut(c).and_then(|d| d.entries.get_mut()).expect("a chunk of its own")
}

/// `g`'s chunks, to change (read first if it is stored).
fn group_mut<V>(g: &mut Group<V>) -> &mut Vec<(u32, Chunk<V>)> {
    g.chunks();
    if Arc::get_mut(g).is_none() {
        *g = Arc::new(GroupData::ready(g.chunks().to_vec()));
    }
    Arc::get_mut(g).and_then(|d| d.chunks.get_mut()).expect("a group of its own")
}

fn group_of(k: u32) -> u32 {
    k >> (SHIFT + GROUP_SHIFT)
}

fn find<T>(v: &[(u32, T)], k: u32) -> std::result::Result<usize, usize> {
    v.binary_search_by_key(&k, |e| e.0)
}

pub struct PMap<V> {
    groups: Arc<Vec<(u32, Group<V>)>>,
    len: usize,
}

impl<V> Clone for PMap<V> {
    fn clone(&self) -> Self {
        PMap {
            groups: self.groups.clone(),
            len: self.len,
        }
    }
}

impl<V> Default for PMap<V> {
    fn default() -> Self {
        PMap {
            groups: Arc::new(Vec::new()),
            len: 0,
        }
    }
}

type ChunkIter<'a, V> = std::iter::Map<std::slice::Iter<'a, (u32, V)>, fn(&'a (u32, V)) -> (&'a u32, &'a V)>;

type GroupIter<'a, V> = std::iter::FlatMap<
    std::slice::Iter<'a, (u32, Chunk<V>)>,
    ChunkIter<'a, V>,
    fn(&'a (u32, Chunk<V>)) -> ChunkIter<'a, V>,
>;

/// `PMap::iter`'s iterator (also `&PMap`'s `IntoIterator`).
pub type Iter<'a, V> = std::iter::FlatMap<
    std::slice::Iter<'a, (u32, Group<V>)>,
    GroupIter<'a, V>,
    fn(&'a (u32, Group<V>)) -> GroupIter<'a, V>,
>;

fn pair<V>(e: &(u32, V)) -> (&u32, &V) {
    (&e.0, &e.1)
}

fn chunk_iter<V>(c: &(u32, Chunk<V>)) -> ChunkIter<'_, V> {
    c.1.entries().iter().map(pair as fn(&(u32, V)) -> (&u32, &V))
}

fn group_iter<V>(g: &(u32, Group<V>)) -> GroupIter<'_, V> {
    g.1.chunks().iter().flat_map(chunk_iter as fn(&(u32, Chunk<V>)) -> ChunkIter<'_, V>)
}

/// The keys of sorted `a` and `b` in order, each with its values in both: `g(in a, in b)`.
fn merge_keys<'a, T>(a: &'a [(u32, T)], b: &'a [(u32, T)], mut g: impl FnMut(u32, Option<&'a T>, Option<&'a T>)) {
    let (mut i, mut j) = (0, 0);
    loop {
        match (a.get(i), b.get(j)) {
            (None, None) => return,
            (Some(x), Some(y)) if x.0 == y.0 => {
                g(x.0, Some(&x.1), Some(&y.1));
                i += 1;
                j += 1;
            }
            (Some(x), y) if y.is_none_or(|y| x.0 < y.0) => {
                g(x.0, Some(&x.1), None);
                i += 1;
            }
            (_, Some(y)) => {
                g(y.0, None, Some(&y.1));
                j += 1;
            }
            (Some(_), None) => unreachable!(),
        }
    }
}

impl<V> PMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// A stored map of `len` entries, the last under `last`, whose groups are read from
    /// `src` as they are first used: opening a catalog reads none of its records.
    pub fn stored(src: Source<V>, len: usize, last: u32) -> Self {
        if len == 0 {
            return Self::default();
        }
        let src = Arc::new(src);
        let groups = (0..=group_of(last))
            .map(|g| {
                let data = GroupData {
                    chunks: OnceLock::new(),
                    src: Some((src.clone(), g)),
                };
                (g, Arc::new(data))
            })
            .collect();
        PMap {
            groups: Arc::new(groups),
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, k: &u32) -> Option<&V> {
        let g = self.groups[find(&self.groups, group_of(*k)).ok()?].1.chunks();
        let c = g[find(g, k >> SHIFT).ok()?].1.entries();
        Some(&c[find(c, *k).ok()?].1)
    }

    /// The last key, without parsing any entry.
    pub fn last_key(&self) -> Option<u32> {
        (self.groups.iter().rev())
            .find_map(|g| g.1.chunks().iter().rev().find_map(|c| c.1.last_key()))
    }

    /// Every group read and parsed now, on up to 8 threads: a whole catalog's generation is
    /// about to visit every entry, and read one at a time a stored map's groups would be read
    /// and parsed serially (the open before format 4 parsed every record on 8 threads).
    pub fn read_all(&self)
    where
        V: Send + Sync,
    {
        self.read_unread();
        let todo: Vec<&GroupData<V>> = (self.groups.iter())
            .map(|g| &*g.1)
            .filter(|g| g.chunks().iter().any(|c| c.1.entries.get().is_none()))
            .collect();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8).min(todo.len());
        if threads < 2 {
            return;
        }
        std::thread::scope(|s| {
            for part in todo.chunks(todo.len().div_ceil(threads)) {
                s.spawn(move || {
                    for g in part {
                        for c in g.chunks() {
                            c.1.entries();
                        }
                    }
                });
            }
        });
    }

    pub fn contains_key(&self, k: &u32) -> bool {
        // (Without parsing the chunk: pgstat's first prune after an open asks it of every
        // relation with statistics, which parsed every table of a database at its first
        // transaction's end.)
        let Ok(g) = find(&self.groups, group_of(*k)) else {
            return false;
        };
        let g = self.groups[g].1.chunks();
        g.get(find(g, k >> SHIFT).ok().unwrap_or(usize::MAX)).is_some_and(|c| c.1.has(*k))
    }

    pub fn iter(&self) -> Iter<'_, V> {
        self.read_unread();
        self.groups.iter().flat_map(group_iter as fn(&(u32, Group<V>)) -> GroupIter<'_, V>)
    }

    /// The groups not read yet read in one scan of the map's records (a whole iteration
    /// reads them all; a scan per group set up a store iterator per 4,096 oids).
    fn read_unread(&self) {
        let unread: Vec<&GroupData<V>> = (self.groups.iter())
            .map(|g| &*g.1)
            .filter(|g| g.src.is_some() && g.chunks.get().is_none())
            .collect();
        let Some((src, _)) = unread.first().and_then(|g| g.src.as_ref()) else {
            return;
        };
        if unread.len() == 1 {
            unread[0].chunks();
            return;
        }
        let records = (src.read)(&src.prefix, &src.end).unwrap_or_else(|e| unreadable(e));
        let mut by_group: BTreeMap<u32, Vec<(u32, Bytes)>> = BTreeMap::new();
        for (k, v) in records {
            let o = record_oid(src, &k);
            by_group.entry(group_of(o)).or_default().push((o, v));
        }
        for g in &unread {
            if let Some((_, n)) = &g.src {
                let recs = by_group.remove(n).unwrap_or_default();
                let _ = g.chunks.set(chunks_of(src, recs.into_iter()));
            }
        }
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &u32> + Clone {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + Clone {
        self.iter().map(|(_, v)| v)
    }

    pub fn first_key_value(&self) -> Option<(&u32, &V)> {
        self.iter().next()
    }

    pub fn last_key_value(&self) -> Option<(&u32, &V)> {
        self.iter().next_back()
    }

    /// The same map (a copy nothing changed since): nothing in it differs.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.groups, &other.groups)
    }

    /// The entries in which `self` and `new` differ, in key order: `f(key, in self, in new)`
    /// for each key only one has, and for each both have where `same` says the values
    /// differ. Groups and chunks the two share aren't visited.
    pub fn diff<'a>(
        &'a self,
        new: &'a Self,
        same: impl Fn(&V, &V) -> bool,
        mut f: impl FnMut(u32, Option<&'a V>, Option<&'a V>),
    ) {
        if self.ptr_eq(new) {
            return;
        }
        merge_keys(&self.groups, &new.groups, |_, ga, gb| match (ga, gb) {
            (Some(ga), Some(gb)) if Arc::ptr_eq(ga, gb) => {}
            (Some(ga), Some(gb)) => merge_keys(ga.chunks(), gb.chunks(), |_, ca, cb| match (ca, cb) {
                (Some(ca), Some(cb)) if Arc::ptr_eq(ca, cb) => {}
                (ca, cb) => merge_keys(
                    ca.map_or(&[][..], |c| c.entries()),
                    cb.map_or(&[][..], |c| c.entries()),
                    |k, a, b| match (a, b) {
                        (Some(a), Some(b)) if same(a, b) => {}
                        (a, b) => f(k, a, b),
                    },
                ),
            }),
            (ga, gb) => {
                for (k, v) in ga.into_iter().flat_map(|g| g.chunks()).flat_map(|c| c.1.entries()).map(|e| (e.0, &e.1)) {
                    f(k, Some(v), None);
                }
                for (k, v) in gb.into_iter().flat_map(|g| g.chunks()).flat_map(|c| c.1.entries()).map(|e| (e.0, &e.1)) {
                    f(k, None, Some(v));
                }
            }
        });
    }
}

impl<T> PMap<Arc<T>> {
    /// The entries that differ between `self` and `new` (values compared by pointer: an
    /// object a copy didn't touch is the same `Arc`), in key order: (key, in self, in new).
    pub fn changes<'a>(&'a self, new: &'a Self) -> Vec<(u32, Option<&'a Arc<T>>, Option<&'a Arc<T>>)> {
        let mut out = Vec::new();
        self.diff(new, Arc::ptr_eq, |k, a, b| out.push((k, a, b)));
        out
    }
}

impl<V: Clone> PMap<V> {
    pub fn insert(&mut self, k: u32, v: V) -> Option<V> {
        let groups = Arc::make_mut(&mut self.groups);
        let gi = match find(groups, group_of(k)) {
            Ok(i) => i,
            Err(i) => {
                groups.insert(i, (group_of(k), Arc::new(GroupData::ready(Vec::new()))));
                i
            }
        };
        let group = group_mut(&mut groups[gi].1);
        let ci = match find(group, k >> SHIFT) {
            Ok(i) => i,
            Err(i) => {
                group.insert(i, (k >> SHIFT, Arc::new(ChunkData::ready(Vec::new()))));
                i
            }
        };
        let chunk = chunk_mut(&mut group[ci].1);
        match find(chunk, k) {
            Ok(i) => Some(std::mem::replace(&mut chunk[i].1, v)),
            Err(i) => {
                chunk.insert(i, (k, v));
                self.len += 1;
                None
            }
        }
    }

    pub fn remove(&mut self, k: &u32) -> Option<V> {
        if !self.contains_key(k) {
            return None;
        }
        let groups = Arc::make_mut(&mut self.groups);
        let gi = find(groups, group_of(*k)).ok()?;
        let group = group_mut(&mut groups[gi].1);
        let ci = find(group, k >> SHIFT).ok()?;
        let chunk = chunk_mut(&mut group[ci].1);
        let old = chunk.remove(find(chunk, *k).ok()?).1;
        if chunk.is_empty() {
            group.remove(ci);
        }
        if group.is_empty() {
            groups.remove(gi);
        }
        self.len -= 1;
        Some(old)
    }

    pub fn get_mut(&mut self, k: &u32) -> Option<&mut V> {
        if !self.contains_key(k) {
            return None;
        }
        let groups = Arc::make_mut(&mut self.groups);
        let gi = find(groups, group_of(*k)).ok()?;
        let group = group_mut(&mut groups[gi].1);
        let ci = find(group, k >> SHIFT).ok()?;
        let chunk = chunk_mut(&mut group[ci].1);
        let i = find(chunk, *k).ok()?;
        Some(&mut chunk[i].1)
    }

    /// Every value, for changing in place: every group and chunk is made the map's own.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        Arc::make_mut(&mut self.groups)
            .iter_mut()
            .flat_map(|g| group_mut(&mut g.1).iter_mut())
            .flat_map(|c| chunk_mut(&mut c.1).iter_mut().map(|e| &mut e.1))
    }

    /// Keep the entries `f` keeps; only chunks that lose one are copied.
    pub fn retain(&mut self, mut f: impl FnMut(&u32, &V) -> bool) {
        let gone: Vec<u32> = self.iter().filter(|(k, v)| !f(k, v)).map(|(k, _)| *k).collect();
        for k in gone {
            self.remove(&k);
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

impl<V> std::ops::Index<&u32> for PMap<V> {
    type Output = V;
    fn index(&self, k: &u32) -> &V {
        self.get(k).expect("no entry found for key")
    }
}

impl<'a, V> IntoIterator for &'a PMap<V> {
    type Item = (&'a u32, &'a V);
    type IntoIter = Iter<'a, V>;
    fn into_iter(self) -> Iter<'a, V> {
        self.iter()
    }
}

impl<V: Clone> FromIterator<(u32, V)> for PMap<V> {
    fn from_iter<I: IntoIterator<Item = (u32, V)>>(it: I) -> Self {
        let mut m = PMap::new();
        m.extend(it);
        m
    }
}

impl<V: Clone> Extend<(u32, V)> for PMap<V> {
    fn extend<I: IntoIterator<Item = (u32, V)>>(&mut self, it: I) {
        for (k, v) in it {
            self.insert(k, v);
        }
    }
}

impl<V: Clone> From<BTreeMap<u32, V>> for PMap<V> {
    fn from(m: BTreeMap<u32, V>) -> Self {
        let len = m.len();
        let mut groups: Vec<(u32, Group<V>)> = Vec::new();
        let mut group: Vec<(u32, Chunk<V>)> = Vec::new();
        let mut chunk: Vec<(u32, V)> = Vec::new();
        let (mut g, mut c) = (u32::MAX, u32::MAX);
        for (k, v) in m {
            if k >> SHIFT != c && !chunk.is_empty() {
                group.push((c, Arc::new(ChunkData::ready(std::mem::take(&mut chunk)))));
            }
            if group_of(k) != g && !group.is_empty() {
                groups.push((g, Arc::new(GroupData::ready(std::mem::take(&mut group)))));
            }
            (g, c) = (group_of(k), k >> SHIFT);
            chunk.push((k, v));
        }
        if !chunk.is_empty() {
            group.push((c, Arc::new(ChunkData::ready(chunk))));
        }
        if !group.is_empty() {
            groups.push((g, Arc::new(GroupData::ready(group))));
        }
        PMap {
            groups: Arc::new(groups),
            len,
        }
    }
}

impl<V: PartialEq> PartialEq for PMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr_eq(other) || (self.len == other.len && self.iter().eq(other.iter()))
    }
}

impl<V: std::fmt::Debug> std::fmt::Debug for PMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<V: Serialize> Serialize for PMap<V> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_map(self.iter())
    }
}

impl<'de, V: Deserialize<'de> + Clone> Deserialize<'de> for PMap<V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        BTreeMap::<u32, V>::deserialize(d).map(PMap::from)
    }
}

/// Secondary indexes over an `IndexedMap`'s entries, kept up to date as entries come and go
/// (and shared by the map's copies as the map is: persistent maps of their own).
pub trait MapIndex<V>: Clone + Default {
    fn add(&mut self, k: u32, v: &V);
    fn remove(&mut self, k: u32, v: &V);
    /// `old` under `k` replaced by `new`: as `remove` then `add` (an index may skip what
    /// both have alike).
    fn replace(&mut self, k: u32, old: &V, new: &V) {
        self.remove(k, old);
        self.add(k, new);
    }
}

/// A `PMap` with secondary indexes (`MapIndex`): what DDL statements and catalog lookups ask
/// of every object (which tables a key's triggers touch, which constraint names a schema
/// has, ...) is answered from the indexes, which a change updates for the entries it
/// touches, instead of by a scan of the catalog. Reads go through `Deref` to the map.
pub struct IndexedMap<V, I> {
    map: PMap<V>,
    ix: I,
}

impl<V, I: Clone> Clone for IndexedMap<V, I> {
    fn clone(&self) -> Self {
        IndexedMap {
            map: self.map.clone(),
            ix: self.ix.clone(),
        }
    }
}

impl<V, I: Default> Default for IndexedMap<V, I> {
    fn default() -> Self {
        IndexedMap {
            map: PMap::default(),
            ix: I::default(),
        }
    }
}

impl<V, I> std::ops::Deref for IndexedMap<V, I> {
    type Target = PMap<V>;
    fn deref(&self) -> &PMap<V> {
        &self.map
    }
}

impl<V: Clone, I: MapIndex<V>> IndexedMap<V, I> {
    pub fn ix(&self) -> &I {
        &self.ix
    }

    /// The map and its indexes, as stored (`persist`: both are read as they were written,
    /// neither made from the other).
    pub fn parts_mut(&mut self) -> (&mut PMap<V>, &mut I) {
        (&mut self.map, &mut self.ix)
    }

    pub fn insert(&mut self, k: u32, v: V) -> Option<V> {
        // (The old entry's contributions go first: the new one may share keys with them.)
        match self.map.get(&k) {
            Some(o) => {
                let o = o.clone();
                self.ix.replace(k, &o, &v);
            }
            None => self.ix.add(k, &v),
        }
        self.map.insert(k, v)
    }

    pub fn remove(&mut self, k: &u32) -> Option<V> {
        let old = self.map.remove(k);
        if let Some(o) = &old {
            self.ix.remove(*k, o);
        }
        old
    }

    pub fn retain(&mut self, mut f: impl FnMut(&u32, &V) -> bool) {
        let gone: Vec<u32> = self.map.iter().filter(|(k, v)| !f(k, v)).map(|(k, _)| *k).collect();
        for k in gone {
            self.remove(&k);
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every value, for changing in place (the indexes follow).
    pub fn update_all(&mut self, mut f: impl FnMut(&mut V)) {
        let keys: Vec<u32> = self.map.keys().copied().collect();
        for k in keys {
            let mut v = self.map[&k].clone();
            f(&mut v);
            self.insert(k, v);
        }
    }
}

impl<'a, V, I> IntoIterator for &'a IndexedMap<V, I> {
    type Item = (&'a u32, &'a V);
    type IntoIter = Iter<'a, V>;
    fn into_iter(self) -> Iter<'a, V> {
        self.map.iter()
    }
}

impl<V: Clone, I: MapIndex<V>> FromIterator<(u32, V)> for IndexedMap<V, I> {
    fn from_iter<T: IntoIterator<Item = (u32, V)>>(it: T) -> Self {
        let mut m = IndexedMap::default();
        for (k, v) in it {
            m.insert(k, v);
        }
        m
    }
}

impl<V: PartialEq, I> PartialEq for IndexedMap<V, I> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl<V: std::fmt::Debug, I> std::fmt::Debug for IndexedMap<V, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.map.fmt(f)
    }
}

impl<V: Serialize, I> Serialize for IndexedMap<V, I> {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.map.serialize(s)
    }
}

impl<'de, V: Deserialize<'de> + Clone, I: MapIndex<V>> Deserialize<'de> for IndexedMap<V, I> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let map = PMap::<V>::deserialize(d)?;
        let mut ix = I::default();
        for (k, v) in map.iter() {
            ix.add(*k, v);
        }
        Ok(IndexedMap { map, ix })
    }
}

/// A multiset of oids (a count per member). Almost every one the indexes keep has one member
/// (the table a constraint, a default or a row type belongs to): that one is kept inline, and
/// a persistent map only from two members on (a `PMap` of one member took six allocations,
/// about a kilobyte of every table's index entries).
#[derive(Clone, Default, Debug, PartialEq)]
pub enum Counts {
    #[default]
    Empty,
    /// The member and its count.
    One(u32, u32),
    /// Two members or more.
    Many(PMap<u32>),
}

impl Counts {
    pub fn add(&mut self, k: u32) {
        match self {
            Counts::Empty => *self = Counts::One(k, 1),
            Counts::One(o, n) if *o == k => *n += 1,
            Counts::One(o, n) => {
                let mut m = PMap::default();
                m.insert(*o, *n);
                m.insert(k, 1);
                *self = Counts::Many(m);
            }
            Counts::Many(m) => {
                let n = m.get(&k).copied().unwrap_or(0);
                m.insert(k, n + 1);
            }
        }
    }

    pub fn remove(&mut self, k: u32) {
        match self {
            Counts::One(o, n) if *o == k && *n > 1 => *n -= 1,
            Counts::One(o, _) if *o == k => *self = Counts::Empty,
            Counts::Many(m) if m.contains_key(&k) => {
                match m.get(&k).copied() {
                    Some(n) if n > 1 => {
                        m.insert(k, n - 1);
                    }
                    _ => {
                        m.remove(&k);
                    }
                }
                if m.len() == 1 {
                    let (o, n) = m.iter().next().map(|(o, n)| (*o, *n)).unwrap();
                    *self = Counts::One(o, n);
                }
            }
            _ => debug_assert!(false, "{k} isn't counted"),
        }
    }

    pub fn contains(&self, k: u32) -> bool {
        match self {
            Counts::Empty => false,
            Counts::One(o, _) => *o == k,
            Counts::Many(m) => m.contains_key(&k),
        }
    }

    /// The members, in order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        let (one, many) = match self {
            Counts::Empty => (None, None),
            Counts::One(o, _) => (Some(*o), None),
            Counts::Many(m) => (None, Some(m.keys().copied())),
        };
        one.into_iter().chain(many.into_iter().flatten())
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Counts::Empty)
    }

    /// Each member with its count, in order.
    pub fn counts(&self) -> Vec<(u32, u32)> {
        match self {
            Counts::Empty => Vec::new(),
            Counts::One(o, n) => vec![(*o, *n)],
            Counts::Many(m) => m.iter().map(|(k, n)| (*k, *n)).collect(),
        }
    }
}

/// (Stored as a list of [member, count].)
impl Serialize for Counts {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.counts().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Counts {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = Vec::<(u32, u32)>::deserialize(d)?;
        Ok(match v[..] {
            [] => Counts::Empty,
            [(o, n)] => Counts::One(o, n),
            _ => Counts::Many(v.into_iter().collect()),
        })
    }
}

/// A `PHash` key: its bucket is a stable hash of its bytes (`encode`), which a stored map's
/// records are keyed by (`PHash::stored`), so a bucket is read alone.
pub trait Key: std::hash::Hash + Eq + Clone {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(b: &[u8]) -> Option<Self>;
    /// FNV-1a of the key's bytes.
    fn bucket(&self) -> usize {
        let mut b = Vec::new();
        self.encode(&mut b);
        fnv(&b)
    }
}

fn fnv(b: &[u8]) -> usize {
    fnv_on(0x811c_9dc5, b) as usize % BUCKETS
}

fn fnv_on(mut h: u32, b: &[u8]) -> u32 {
    for x in b {
        h = (h ^ *x as u32).wrapping_mul(0x0100_0193);
    }
    h
}

impl Key for u32 {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_be_bytes());
    }
    fn decode(b: &[u8]) -> Option<Self> {
        Some(u32::from_be_bytes(b.try_into().ok()?))
    }
}

impl Key for String {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.as_bytes());
    }
    fn decode(b: &[u8]) -> Option<Self> {
        String::from_utf8(b.to_vec()).ok()
    }
    fn bucket(&self) -> usize {
        fnv(self.as_bytes())
    }
}

impl Key for (u32, String) {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_be_bytes());
        out.extend_from_slice(self.1.as_bytes());
    }
    fn decode(b: &[u8]) -> Option<Self> {
        Some((u32::decode(b.get(..4)?)?, String::decode(&b[4..])?))
    }
    fn bucket(&self) -> usize {
        fnv_on(fnv_on(0x811c_9dc5, &self.0.to_be_bytes()), self.1.as_bytes()) as usize % BUCKETS
    }
}

type HashMap<K, V> = crate::exec::fasthash::HashMap<K, V>;

/// Where a `PHash` read on demand finds its records: under `prefix`, the bucket's number and
/// the key's bytes; `parse` makes an entry of the bytes after the bucket and the value.
pub struct HSource<K, V> {
    pub prefix: Vec<u8>,
    pub end: Vec<u8>,
    pub read: Reader,
    pub parse: fn(&[u8], &[u8]) -> crate::error::Result<(K, V)>,
}

/// A bucket: made, or a stored map's bucket read on first use (as `GroupData`).
struct BucketData<K, V> {
    map: OnceLock<HashMap<K, V>>,
    src: Option<(Arc<HSource<K, V>>, u8)>,
}

impl<K: Key, V> BucketData<K, V> {
    fn ready(map: HashMap<K, V>) -> Self {
        BucketData {
            map: OnceLock::from(map),
            src: None,
        }
    }

    fn map(&self) -> &HashMap<K, V> {
        self.map.get_or_init(|| {
            let mut m = HashMap::default();
            let Some((src, b)) = &self.src else {
                return m;
            };
            let from = [&src.prefix[..], &[*b]].concat();
            let to = if *b == u8::MAX { src.end.clone() } else { [&src.prefix[..], &[*b + 1]].concat() };
            for (k, v) in (src.read)(&from, &to).unwrap_or_else(|e| unreadable(e)) {
                let (k, v) = (src.parse)(&k[from.len()..], &v).unwrap_or_else(|e| unreadable(e));
                m.insert(k, v);
            }
            m
        })
    }
}

/// An unordered map shared structurally by a catalog's copies, as `PMap` (the name index:
/// a change copies one bucket of `BUCKETS`, not every name of the catalog).
pub struct PHash<K, V> {
    buckets: Arc<Vec<Arc<BucketData<K, V>>>>,
    len: usize,
}

const BUCKETS: usize = 256;

impl<K, V> Clone for PHash<K, V> {
    fn clone(&self) -> Self {
        PHash {
            buckets: self.buckets.clone(),
            len: self.len,
        }
    }
}

impl<K, V> Default for PHash<K, V> {
    fn default() -> Self {
        PHash {
            buckets: Arc::new(Vec::new()),
            len: 0,
        }
    }
}

impl<K: Key, V: Clone> PHash<K, V> {
    /// A stored map of `len` entries whose buckets are read from `src` as they are first
    /// used.
    pub fn stored(src: HSource<K, V>, len: usize) -> Self {
        if len == 0 {
            return Self::default();
        }
        let src = Arc::new(src);
        let buckets = (0..BUCKETS)
            .map(|b| {
                let data = BucketData {
                    map: OnceLock::new(),
                    src: Some((src.clone(), b as u8)),
                };
                Arc::new(data)
            })
            .collect();
        PHash {
            buckets: Arc::new(buckets),
            len,
        }
    }

    /// Bucket `b` of `buckets`, to change (read first if it is stored).
    fn bucket_mut(buckets: &mut [Arc<BucketData<K, V>>], b: usize) -> &mut HashMap<K, V> {
        let d = &mut buckets[b];
        d.map();
        if Arc::get_mut(d).is_none() {
            *d = Arc::new(BucketData::ready(d.map().clone()));
        }
        Arc::get_mut(d).and_then(|d| d.map.get_mut()).expect("a bucket of its own")
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.buckets.get(k.bucket())?.map().get(k)
    }

    /// The value under `k`, to change in place (its bucket made this map's own).
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let b = k.bucket();
        if !self.buckets.get(b).is_some_and(|m| m.map().contains_key(k)) {
            return None;
        }
        Self::bucket_mut(Arc::make_mut(&mut self.buckets).as_mut_slice(), b).get_mut(k)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let b = k.bucket();
        let buckets = Arc::make_mut(&mut self.buckets);
        if buckets.is_empty() {
            buckets.resize_with(BUCKETS, || Arc::new(BucketData::ready(HashMap::default())));
        }
        let old = Self::bucket_mut(buckets, b).insert(k, v);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let b = k.bucket();
        if !self.buckets.get(b).is_some_and(|m| m.map().contains_key(k)) {
            return None;
        }
        let old = Self::bucket_mut(Arc::make_mut(&mut self.buckets).as_mut_slice(), b).remove(k);
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every bucket not read yet read now, in one scan of the map's records (as
    /// `PMap::read_all`).
    pub fn read_all(&self) {
        let todo: Vec<&BucketData<K, V>> = self.buckets.iter().map(|b| &**b).filter(|b| b.map.get().is_none()).collect();
        if let Some((src, _)) = todo.iter().find_map(|b| b.src.as_ref()) {
            let records = (src.read)(&src.prefix, &src.end).unwrap_or_else(|e| unreadable(e));
            let mut by_bucket: Vec<HashMap<K, V>> = (0..BUCKETS).map(|_| HashMap::default()).collect();
            for (k, v) in records {
                let b = *k.get(src.prefix.len()).unwrap_or_else(|| unreadable(crate::error::internal("bad index key")));
                let (k, v) = (src.parse)(&k[src.prefix.len() + 1..], &v).unwrap_or_else(|e| unreadable(e));
                by_bucket[b as usize].insert(k, v);
            }
            for b in &todo {
                if let Some((_, n)) = &b.src {
                    let _ = b.map.set(std::mem::take(&mut by_bucket[*n as usize]));
                }
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.read_all();
        self.buckets.iter().flat_map(|b| b.map().iter())
    }

    /// The entries in which `self` and `new` differ: `f(key, in self, in new)`. Buckets the
    /// two share aren't visited.
    pub fn diff<'a>(&'a self, new: &'a Self, same: impl Fn(&V, &V) -> bool, mut f: impl FnMut(&'a K, Option<&'a V>, Option<&'a V>)) {
        if Arc::ptr_eq(&self.buckets, &new.buckets) {
            return;
        }
        for b in 0..BUCKETS {
            let (x, y) = (self.buckets.get(b), new.buckets.get(b));
            if let (Some(x), Some(y)) = (x, y)
                && Arc::ptr_eq(x, y)
            {
                continue;
            }
            let (x, y) = (x.map(|x| x.map()), y.map(|y| y.map()));
            for (k, a) in x.into_iter().flatten() {
                match y.and_then(|y| y.get(k)) {
                    Some(b) if same(a, b) => {}
                    b => f(k, Some(a), b),
                }
            }
            for (k, b) in y.into_iter().flatten() {
                if !x.is_some_and(|x| x.contains_key(k)) {
                    f(k, None, Some(b));
                }
            }
        }
    }
}

impl<K: Key + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for PHash<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.buckets.iter().flat_map(|b| b.map().iter())).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory store of records, as `persist` writes them: (reader, records read).
    fn store_of(records: BTreeMap<Vec<u8>, Bytes>) -> (Reader, Arc<std::sync::atomic::AtomicUsize>) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n = reads.clone();
        let read: Reader = Arc::new(move |from: &[u8], to: &[u8]| {
            let out: Vec<(Vec<u8>, Bytes)> =
                records.range(from.to_vec()..to.to_vec()).map(|(k, v)| (k.clone(), v.clone())).collect();
            n.fetch_add(out.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(out)
        });
        (read, reads)
    }

    fn parse_u64(b: &[u8]) -> crate::error::Result<u64> {
        Ok(std::str::from_utf8(b).unwrap().parse().unwrap())
    }

    /// A stored map reads only the groups it is asked for, behaves as the map it was stored
    /// from through edits, and its diff against a copy lists what changed.
    #[test]
    fn stored_maps_read_groups_as_used() {
        let model: BTreeMap<u32, u64> = (0..20_000u32).filter(|k| k % 3 != 1).map(|k| (k * 7, k as u64)).collect();
        let prefix = vec![9u8, 2];
        let records = (model.iter())
            .map(|(k, v)| ([&prefix[..], &k.to_be_bytes()].concat(), Bytes::from(v.to_string())))
            .collect();
        let (read, reads) = store_of(records);
        let src = Source { prefix: prefix.clone(), end: vec![9, 3], read, parse: parse_u64 };
        let last = *model.keys().last().unwrap();
        let m = PMap::stored(src, model.len(), last);
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(m.get(&(500 * 7)), model.get(&(500 * 7)));
        let one = reads.load(std::sync::atomic::Ordering::Relaxed);
        assert!(one > 0 && one < model.len() / 10, "{one} records read for one key");
        assert_eq!(m.last_key(), Some(last));
        let mut m2 = m.clone();
        let mut model2 = model.clone();
        for k in [3u32, 7 * 9, 70_000, 139_993, 5] {
            assert_eq!(m2.insert(k, 1), model2.insert(k, 1));
        }
        assert_eq!(m2.remove(&(7 * 12)), model2.remove(&(7 * 12)));
        assert!(m2.iter().eq(model2.iter()));
        assert!(m.iter().eq(model.iter()));
        assert_eq!(m2.len(), model2.len());
        let mut got = Vec::new();
        m.diff(&m2, |a, b| a == b, |k, a, b| got.push((k, a.copied(), b.copied())));
        let want: Vec<_> = (model.keys().chain(model2.keys()).copied().collect::<std::collections::BTreeSet<_>>())
            .into_iter()
            .map(|k| (k, model.get(&k).copied(), model2.get(&k).copied()))
            .filter(|(_, a, b)| a != b)
            .collect();
        assert_eq!(got, want);
    }

    /// A stored `PHash` reads a bucket at a time and behaves as the map it was stored from.
    #[test]
    fn stored_phash_reads_buckets_as_used() {
        let model: std::collections::HashMap<(u32, String), u64> =
            (0..5000u32).map(|i| ((i % 7, format!("n{i}")), i as u64)).collect();
        let prefix = vec![9u8, 7];
        let records = (model.iter())
            .map(|(k, v)| {
                let mut key = prefix.clone();
                key.push(k.bucket() as u8);
                k.encode(&mut key);
                (key, Bytes::from(v.to_string()))
            })
            .collect();
        let (read, reads) = store_of(records);
        fn parse(k: &[u8], v: &[u8]) -> crate::error::Result<((u32, String), u64)> {
            Ok((<(u32, String)>::decode(k).unwrap(), parse_u64(v)?))
        }
        let src = HSource { prefix, end: vec![9, 8], read, parse };
        let mut m = PHash::stored(src, model.len());
        assert_eq!(m.get(&(3, "n10".into())), model.get(&(3, "n10".into())));
        assert!(reads.load(std::sync::atomic::Ordering::Relaxed) < model.len() / 50);
        let copy = m.clone();
        m.insert((1, "new".into()), 1);
        m.remove(&(3, "n10".into()));
        assert_eq!(m.len(), model.len());
        let mut got = Vec::new();
        copy.diff(&m, |a, b| a == b, |k, a, b| got.push((k.clone(), a.copied(), b.copied())));
        got.sort();
        assert_eq!(got, vec![((1, "new".into()), None, Some(1)), ((3, "n10".into()), Some(10), None)]);
        for (k, v) in &model {
            assert_eq!(copy.get(k), Some(v));
        }
        assert_eq!(copy.iter().count(), model.len());
    }

    #[test]
    fn phash_behaves_as_a_hashmap() {
        let mut m: PHash<(u32, String), u32> = PHash::default();
        let mut model = std::collections::HashMap::new();
        for i in 0..3000u32 {
            let k = (i % 7, format!("n{}", i % 1000));
            let copy = m.clone();
            let copy_len = copy.len();
            if i % 3 == 0 {
                assert_eq!(m.remove(&k), model.remove(&k));
            } else {
                assert_eq!(m.insert(k.clone(), i), model.insert(k, i));
            }
            assert_eq!(copy.len(), copy_len);
            assert_eq!(m.len(), model.len());
        }
        for (k, v) in &model {
            assert_eq!(m.get(k), Some(v));
        }
        assert_eq!(m.iter().count(), model.len());
    }

    /// Random edits against a `BTreeMap` model: contents, order, length, the serde form and
    /// `diff` against an earlier copy.
    #[test]
    fn behaves_as_a_btreemap() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut m: PMap<u64> = PMap::new();
        let mut model: BTreeMap<u32, u64> = BTreeMap::new();
        for round in 0..200 {
            let before = m.clone();
            let before_model = model.clone();
            for _ in 0..rnd(40) {
                let k = (rnd(3000) + if rnd(4) == 0 { 1 << 20 } else { 0 }) as u32;
                match rnd(5) {
                    0..=2 => {
                        let v = rnd(1000);
                        assert_eq!(m.insert(k, v), model.insert(k, v));
                    }
                    3 => assert_eq!(m.remove(&k), model.remove(&k)),
                    _ => {
                        if let Some(v) = m.get_mut(&k) {
                            *v += 1;
                        }
                        if let Some(v) = model.get_mut(&k) {
                            *v += 1;
                        }
                    }
                }
            }
            if round % 50 == 7 {
                m.retain(|k, _| k % 3 != 0);
                model.retain(|k, _| k % 3 != 0);
            }
            assert_eq!(m.len(), model.len());
            assert!(m.iter().eq(model.iter()));
            assert!(m.iter().rev().eq(model.iter().rev()));
            assert_eq!(m.first_key_value(), model.first_key_value());
            assert_eq!(m.last_key_value(), model.last_key_value());
            for k in 0..3100u32 {
                assert_eq!(m.get(&k), model.get(&k));
            }
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                serde_json::to_string(&model).unwrap()
            );
            let back: PMap<u64> = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
            assert_eq!(back, m);
            // The earlier copy is unchanged, and `diff` lists exactly what changed.
            assert!(before.iter().eq(before_model.iter()));
            let mut got = Vec::new();
            before.diff(&m, |a, b| a == b, |k, a, b| got.push((k, a.copied(), b.copied())));
            let keys: std::collections::BTreeSet<u32> =
                before_model.keys().chain(model.keys()).copied().collect();
            let want: Vec<(u32, Option<u64>, Option<u64>)> = keys
                .into_iter()
                .map(|k| (k, before_model.get(&k).copied(), model.get(&k).copied()))
                .filter(|(_, a, b)| a != b)
                .collect();
            assert_eq!(got, want);
        }
    }
}
