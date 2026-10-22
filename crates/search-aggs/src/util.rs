//! Hashing and small helpers shared by aggregations.
//!
//! - [`murmur3_x86_32`]: Lucene's `StringHelper.murmurhash3_x86_32` (terms `include`
//!   partitions).
//! - [`murmur3_x64_128`]: Elasticsearch's `MurmurHash3.hash128` (cardinality hashes values with
//!   it, so HyperLogLog++ registers — and counts — match Elasticsearch's).
//! - [`mix64`]: hppc's `BitMixer.mix64` (numeric partitions, cardinality of numbers).
//! - [`OrdMap`]: `(owning ordinal, key) → bucket ordinal` assignment, Elasticsearch's
//!   `LongKeyedBucketOrds`.
//! - [`intro_sort`]: Lucene's `IntroSorter` (unstable: where equal elements end up is part of
//!   some outputs, e.g. `geo_line` points with equal sort values).

use hashbrown::HashTable;

use crate::error::{AggError, Result};

/// `StringHelper.murmurhash3_x86_32(bytes, seed)`.
pub fn murmur3_x86_32(data: &[u8], seed: u32) -> i32 {
    const C1: u32 = 0xcc9e2d51;
    const C2: u32 = 0x1b873593;
    let mut h1 = seed;
    let len = data.len();
    let rounded = len & !3;
    let mut i = 0;
    while i < rounded {
        let mut k1 = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(13);
        h1 = h1.wrapping_mul(5).wrapping_add(0xe6546b64);
        i += 4;
    }
    let mut k1: u32 = 0;
    let tail = len & 3;
    if tail == 3 {
        k1 = (data[rounded + 2] as u32) << 16;
    }
    if tail >= 2 {
        k1 |= (data[rounded + 1] as u32) << 8;
    }
    if tail >= 1 {
        k1 |= data[rounded] as u32;
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(15);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= len as u32;
    h1 ^= h1 >> 16;
    h1 = h1.wrapping_mul(0x85ebca6b);
    h1 ^= h1 >> 13;
    h1 = h1.wrapping_mul(0xc2b2ae35);
    h1 ^= h1 >> 16;
    h1 as i32
}

#[inline]
fn fmix64(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51afd7ed558ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ceb9fe1a85ec53);
    k ^= k >> 33;
    k
}

/// Elasticsearch's `MurmurHash3.hash128(bytes, offset, length, seed)`: `(h1, h2)`.
pub fn murmur3_x64_128(data: &[u8], seed: u64) -> (u64, u64) {
    const C1: u64 = 0x87c37b91114253d5;
    const C2: u64 = 0x4cf5ad432745937f;
    let mut h1 = seed;
    let mut h2 = seed;
    let len = data.len();
    let nblocks = len / 16;
    for i in 0..nblocks {
        let b = &data[i * 16..i * 16 + 16];
        let mut k1 = u64::from_le_bytes(b[0..8].try_into().expect("8"));
        let mut k2 = u64::from_le_bytes(b[8..16].try_into().expect("8"));
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
        h1 = h1.rotate_left(27);
        h1 = h1.wrapping_add(h2);
        h1 = h1.wrapping_mul(5).wrapping_add(0x52dce729);
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
        h2 = h2.rotate_left(31);
        h2 = h2.wrapping_add(h1);
        h2 = h2.wrapping_mul(5).wrapping_add(0x38495ab5);
    }
    let tail = &data[nblocks * 16..];
    let mut k1: u64 = 0;
    let mut k2: u64 = 0;
    let t = tail.len();
    if t >= 15 {
        k2 ^= (tail[14] as u64) << 48;
    }
    if t >= 14 {
        k2 ^= (tail[13] as u64) << 40;
    }
    if t >= 13 {
        k2 ^= (tail[12] as u64) << 32;
    }
    if t >= 12 {
        k2 ^= (tail[11] as u64) << 24;
    }
    if t >= 11 {
        k2 ^= (tail[10] as u64) << 16;
    }
    if t >= 10 {
        k2 ^= (tail[9] as u64) << 8;
    }
    if t >= 9 {
        k2 ^= tail[8] as u64;
        k2 = k2.wrapping_mul(C2);
        k2 = k2.rotate_left(33);
        k2 = k2.wrapping_mul(C1);
        h2 ^= k2;
    }
    if t >= 8 {
        k1 ^= (tail[7] as u64) << 56;
    }
    if t >= 7 {
        k1 ^= (tail[6] as u64) << 48;
    }
    if t >= 6 {
        k1 ^= (tail[5] as u64) << 40;
    }
    if t >= 5 {
        k1 ^= (tail[4] as u64) << 32;
    }
    if t >= 4 {
        k1 ^= (tail[3] as u64) << 24;
    }
    if t >= 3 {
        k1 ^= (tail[2] as u64) << 16;
    }
    if t >= 2 {
        k1 ^= (tail[1] as u64) << 8;
    }
    if t >= 1 {
        k1 ^= tail[0] as u64;
        k1 = k1.wrapping_mul(C1);
        k1 = k1.rotate_left(31);
        k1 = k1.wrapping_mul(C2);
        h1 ^= k1;
    }
    h1 ^= len as u64;
    h2 ^= len as u64;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    h1 = fmix64(h1);
    h2 = fmix64(h2);
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    (h1, h2)
}

/// hppc `BitMixer.mix64`.
#[inline]
pub fn mix64(z: u64) -> u64 {
    let z = (z ^ (z >> 32)).wrapping_mul(0x4cd6944c5cc20b6d);
    let z = (z ^ (z >> 29)).wrapping_mul(0xfc12c5b19d3259e9);
    z ^ (z >> 32)
}

/// `Math.floorMod(x, m)` for ints.
#[inline]
pub fn floor_mod(x: i64, m: i64) -> i64 {
    x.rem_euclid(m)
}

#[inline]
fn hash_pair(owning: u32, key: u64) -> u64 {
    mix64(key ^ ((owning as u64) << 32 | owning as u64).rotate_left(17))
}

/// Assigns dense bucket ordinals to `(owning ordinal, key)` pairs. The table holds the whole
/// entry (key, owning, ordinal), so a lookup touches one cache line; `keys` maps ordinals back.
#[derive(Default)]
pub struct OrdMap {
    table: HashTable<(u64, u32, u32)>,
    keys: Vec<(u32, u64)>,
}

impl OrdMap {
    pub fn new() -> OrdMap {
        OrdMap::default()
    }

    /// The ordinal of `(owning, key)`, adding it when new; `(ord, is_new)`.
    #[inline]
    pub fn add(&mut self, owning: u32, key: u64) -> (u32, bool) {
        let h = hash_pair(owning, key);
        let o = self.keys.len() as u32;
        match self.table.entry(h, |e| e.0 == key && e.1 == owning, |e| hash_pair(e.1, e.0)) {
            hashbrown::hash_table::Entry::Occupied(e) => (e.get().2, false),
            hashbrown::hash_table::Entry::Vacant(v) => {
                v.insert((key, owning, o));
                self.keys.push((owning, key));
                (o, true)
            }
        }
    }

    #[inline]
    pub fn find(&self, owning: u32, key: u64) -> Option<u32> {
        let h = hash_pair(owning, key);
        self.table.find(h, |e| e.0 == key && e.1 == owning).map(|e| e.2)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// `(owning, key)` of an ordinal.
    #[inline]
    pub fn key(&self, ord: u32) -> (u32, u64) {
        self.keys[ord as usize]
    }

    pub fn keys(&self) -> &[(u32, u64)] {
        &self.keys
    }

    pub fn ram_bytes(&self) -> usize {
        self.keys.capacity() * 16 + self.table.capacity() * 17
    }
}

/// Assigns dense bucket ordinals to `(owning ordinal, byte string)` pairs (map execution).
#[derive(Default)]
pub struct BytesOrdMap {
    table: HashTable<u32>,
    owning: Vec<u32>,
    offsets: Vec<u32>,
    data: Vec<u8>,
}

fn hash_bytes(owning: u32, b: &[u8]) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = foldhash::fast::FixedState::with_seed(0x5eed).build_hasher();
    h.write_u32(owning);
    h.write(b);
    h.finish()
}

impl BytesOrdMap {
    pub fn new() -> BytesOrdMap {
        BytesOrdMap { table: HashTable::new(), owning: Vec::new(), offsets: vec![0], data: Vec::new() }
    }

    #[inline]
    pub fn get_bytes(&self, ord: u32) -> &[u8] {
        &self.data[self.offsets[ord as usize] as usize..self.offsets[ord as usize + 1] as usize]
    }

    pub fn owning(&self, ord: u32) -> u32 {
        self.owning[ord as usize]
    }

    #[inline]
    pub fn add(&mut self, owning: u32, key: &[u8]) -> (u32, bool) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        let h = hash_bytes(owning, key);
        let found = {
            let (ow, offs, data) = (&self.owning, &self.offsets, &self.data);
            self.table
                .find(h, |&o| ow[o as usize] == owning && &data[offs[o as usize] as usize..offs[o as usize + 1] as usize] == key)
                .copied()
        };
        if let Some(o) = found {
            return (o, false);
        }
        let o = self.owning.len() as u32;
        self.owning.push(owning);
        self.data.extend_from_slice(key);
        self.offsets.push(self.data.len() as u32);
        let (ow, offs, data) = (&self.owning, &self.offsets, &self.data);
        self.table.insert_unique(h, o, |&o| hash_bytes(ow[o as usize], &data[offs[o as usize] as usize..offs[o as usize + 1] as usize]));
        (o, true)
    }

    pub fn find(&self, owning: u32, key: &[u8]) -> Option<u32> {
        if self.owning.is_empty() {
            return None;
        }
        let h = hash_bytes(owning, key);
        self.table.find(h, |&o| self.owning[o as usize] == owning && self.get_bytes(o) == key).copied()
    }

    pub fn len(&self) -> usize {
        self.owning.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owning.is_empty()
    }

    pub fn ram_bytes(&self) -> usize {
        self.owning.capacity() * 4 + self.offsets.capacity() * 4 + self.data.capacity() + self.table.capacity() * 5
    }
}

/// Groups the items of key-sorted lists by key the way Elasticsearch's reduces merge shard
/// buckets: a Lucene `PriorityQueue` of list cursors ordered by `less`, popping the top item and
/// re-sifting (`updateTop`) or removing (`pop`) its cursor. Items with equal keys come out in the
/// heap's order, which is not always list order, and that order is what sub-aggregations are
/// reduced in (floating-point sums, tie-breaks). Empty lists are skipped, as Elasticsearch does.
pub fn lucene_merge_groups<B>(lists: Vec<Vec<B>>, less: impl Fn(&B, &B) -> bool) -> Vec<Vec<B>> {
    let mut cursors: Vec<std::iter::Peekable<std::vec::IntoIter<B>>> =
        lists.into_iter().filter(|l| !l.is_empty()).map(|l| l.into_iter().peekable()).collect();
    // 1-based heap of cursor indices.
    let mut heap: Vec<usize> = vec![usize::MAX];
    let lt = |cursors: &mut Vec<std::iter::Peekable<std::vec::IntoIter<B>>>, a: usize, b: usize| -> bool {
        let (x, y) = if a < b {
            let (l, r) = cursors.split_at_mut(b);
            (l[a].peek(), r[0].peek())
        } else if a > b {
            let (l, r) = cursors.split_at_mut(a);
            (r[0].peek(), l[b].peek())
        } else {
            return false;
        };
        match (x, y) {
            (Some(x), Some(y)) => less(x, y),
            _ => false,
        }
    };
    let up_heap = |heap: &mut Vec<usize>, cursors: &mut Vec<_>, orig: usize| {
        let mut i = orig;
        let node = heap[i];
        let mut j = i >> 1;
        while j > 0 && lt(cursors, node, heap[j]) {
            heap[i] = heap[j];
            i = j;
            j >>= 1;
        }
        heap[i] = node;
    };
    let down_heap = |heap: &mut Vec<usize>, cursors: &mut Vec<_>, start: usize| {
        let size = heap.len() - 1;
        let mut i = start;
        let node = heap[i];
        let mut j = i << 1;
        let mut k = j + 1;
        if k <= size && lt(cursors, heap[k], heap[j]) {
            j = k;
        }
        while j <= size && lt(cursors, heap[j], node) {
            heap[i] = heap[j];
            i = j;
            j = i << 1;
            k = j + 1;
            if k <= size && lt(cursors, heap[k], heap[j]) {
                j = k;
            }
        }
        heap[i] = node;
    };
    for c in 0..cursors.len() {
        heap.push(c);
        let n = heap.len() - 1;
        up_heap(&mut heap, &mut cursors, n);
    }
    let mut groups: Vec<Vec<B>> = Vec::new();
    let mut group: Vec<B> = Vec::new();
    while heap.len() > 1 {
        let top = heap[1];
        let item = cursors[top].next().expect("non-empty cursor");
        if let Some(last) = group.last()
            && (less(last, &item) || less(&item, last))
        {
            groups.push(std::mem::take(&mut group));
        }
        group.push(item);
        if cursors[top].peek().is_some() {
            down_heap(&mut heap, &mut cursors, 1);
        } else {
            // pop(): the last element moves to the top and sifts down.
            let last = heap.pop().expect("heap");
            if heap.len() > 1 {
                heap[1] = last;
                down_heap(&mut heap, &mut cursors, 1);
            }
        }
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

/// A model of `java.util.HashMap`'s bucket layout (no values): enough to know the iteration
/// order Elasticsearch's responses inherit from it. Keys are given by id and spread hash
/// (`h ^ (h >>> 16)`); bins of eight or more entries in tables of 64+ slots (treeified in Java)
/// keep insertion order here.
pub struct JavaHashMapModel {
    bins: Vec<Vec<usize>>,
    hashes: hashbrown::HashMap<usize, u32>,
    threshold: usize,
    size: usize,
}

impl JavaHashMapModel {
    /// `new HashMap<>()` (`None`) or `new HashMap<>(initial)`.
    pub fn new(initial: Option<usize>) -> JavaHashMapModel {
        let threshold = match initial {
            None => 0,
            Some(n) => n.max(1).next_power_of_two(),
        };
        JavaHashMapModel { bins: Vec::new(), hashes: hashbrown::HashMap::new(), threshold, size: 0 }
    }

    /// `HashMap.hash(key)` of a key whose `hashCode()` is `h`.
    pub fn spread(h: i32) -> u32 {
        let h = h as u32;
        h ^ (h >> 16)
    }

    fn resize(&mut self) {
        let old = self.bins.len();
        let (cap, thr) = if old > 0 {
            let cap = old * 2;
            (cap, if old >= 16 { self.threshold * 2 } else { (cap as f32 * 0.75) as usize })
        } else if self.threshold > 0 {
            let cap = self.threshold;
            (cap, (cap as f32 * 0.75) as usize)
        } else {
            (16, 12)
        };
        let mut next = vec![Vec::new(); cap];
        for bin in self.bins.drain(..) {
            for id in bin {
                next[(self.hashes[&id] as usize) & (cap - 1)].push(id);
            }
        }
        self.bins = next;
        self.threshold = thr;
    }

    fn slot(&self, hash: u32) -> usize {
        (hash as usize) & (self.bins.len() - 1)
    }

    /// `put` of a new key (`putVal`: appended to its bin, resize after the insert).
    pub fn put_new(&mut self, id: usize, hash: u32) {
        if self.bins.is_empty() {
            self.resize();
        }
        self.hashes.insert(id, hash);
        let slot = self.slot(hash);
        let existing = self.bins[slot].len();
        self.bins[slot].push(id);
        if existing >= 8 && self.bins.len() < 64 {
            self.resize();
        }
        self.size += 1;
        if self.size > self.threshold {
            self.resize();
        }
    }

    /// `computeIfAbsent` of a new key (resize check first, linked at the head of its bin).
    pub fn compute_if_absent_new(&mut self, id: usize, hash: u32) {
        if self.bins.is_empty() || self.size > self.threshold {
            self.resize();
        }
        self.hashes.insert(id, hash);
        let slot = self.slot(hash);
        let existing = self.bins[slot].len();
        self.bins[slot].insert(0, id);
        self.size += 1;
        if existing >= 7 && self.bins.len() < 64 {
            self.resize();
        }
    }

    /// `computeIfAbsent` of a key already present (only the resize check).
    pub fn compute_if_absent_existing(&mut self) {
        if self.bins.is_empty() || self.size > self.threshold {
            self.resize();
        }
    }

    /// Ids in iteration order.
    pub fn order(&self) -> Vec<usize> {
        self.bins.iter().flatten().copied().collect()
    }
}

/// `String.hashCode()` (UTF-16 code units).
pub fn java_string_hash_code(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, u| h.wrapping_mul(31).wrapping_add(u as i32))
}

/// The iteration order of Elasticsearch's `ObjectObjectPagedHashMap` (open addressing, linear
/// probing, load factor 0.6, keys placed by their raw `hashCode()`) built with
/// `initial_capacity` and filled by `put` with distinct keys whose hash codes are `hashes`, in
/// that order. Returns indices into `hashes`.
pub fn paged_hash_map_order(hashes: &[i32], initial_capacity: usize) -> Vec<usize> {
    const LOAD: f32 = 0.6;
    let mut buckets: i64 = 1 + (initial_capacity as f32 / LOAD) as i64;
    buckets = if buckets - 1 <= 0 { 1 } else { ((1i64 << (63 - (buckets - 1).leading_zeros())) << 1).max(1) };
    let mut max_size = (buckets as f32 * LOAD) as i64;
    let mut table: Vec<Option<usize>> = vec![None; buckets as usize];
    let mut mask = buckets - 1;
    let mut size: i64 = 0;
    let place = |table: &mut Vec<Option<usize>>, mask: i64, entry: usize| {
        let mut idx = (hashes[entry] as i64) & mask;
        loop {
            if table[idx as usize].is_none() {
                table[idx as usize] = Some(entry);
                return;
            }
            idx = (idx + 1) & mask;
        }
    };
    for e in 0..hashes.len() {
        if size >= max_size {
            // grow(): double, then re-add the old slots in order and the run that follows them.
            let old = table.len();
            let new = old * 2;
            table.resize(new, None);
            mask = new as i64 - 1;
            for i in 0..old {
                if let Some(x) = table[i].take() {
                    place(&mut table, mask, x);
                }
            }
            for i in old..new {
                match table[i].take() {
                    Some(x) => place(&mut table, mask, x),
                    None => break,
                }
            }
            max_size = (new as f32 * LOAD) as i64;
        }
        place(&mut table, mask, e);
        size += 1;
    }
    table.into_iter().flatten().collect()
}

/// Lucene's `PriorityQueue` / Elasticsearch's `ObjectArrayPriorityQueue` with a caller-given
/// `less_than`: same heap moves, so items that compare equal come out in the same order.
pub struct LucenePq<T> {
    heap: Vec<Option<T>>,
    size: usize,
    max_size: usize,
}

impl<T> LucenePq<T> {
    pub fn new(max_size: usize) -> LucenePq<T> {
        let mut heap = Vec::with_capacity(max_size.max(1) + 1);
        heap.push(None);
        LucenePq { heap, size: 0, max_size }
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    fn get(&self, i: usize) -> &T {
        self.heap[i].as_ref().expect("heap slot")
    }

    fn up_heap(&mut self, orig: usize, less: &impl Fn(&T, &T) -> bool) {
        let mut i = orig;
        let node = self.heap[i].take();
        let mut j = i >> 1;
        while j > 0 && less(node.as_ref().expect("node"), self.get(j)) {
            self.heap[i] = self.heap[j].take();
            i = j;
            j >>= 1;
        }
        self.heap[i] = node;
    }

    fn down_heap(&mut self, start: usize, less: &impl Fn(&T, &T) -> bool) {
        let mut i = start;
        let node = self.heap[i].take();
        let mut j = i << 1;
        let mut k = j + 1;
        if k <= self.size && less(self.get(k), self.get(j)) {
            j = k;
        }
        while j <= self.size && less(self.get(j), node.as_ref().expect("node")) {
            self.heap[i] = self.heap[j].take();
            i = j;
            j = i << 1;
            k = j + 1;
            if k <= self.size && less(self.get(k), self.get(j)) {
                j = k;
            }
        }
        self.heap[i] = node;
    }

    pub fn add(&mut self, x: T, less: &impl Fn(&T, &T) -> bool) {
        self.size += 1;
        if self.heap.len() <= self.size {
            self.heap.push(Some(x));
        } else {
            self.heap[self.size] = Some(x);
        }
        let n = self.size;
        self.up_heap(n, less);
    }

    /// `insertWithOverflow`: the element that didn't fit (or was pushed out), if any.
    pub fn insert_with_overflow(&mut self, x: T, less: &impl Fn(&T, &T) -> bool) -> Option<T> {
        if self.size < self.max_size {
            self.add(x, less);
            None
        } else if self.size > 0 && less(self.get(1), &x) {
            let old = self.heap[1].replace(x);
            self.down_heap(1, less);
            old
        } else {
            Some(x)
        }
    }

    pub fn pop(&mut self, less: &impl Fn(&T, &T) -> bool) -> Option<T> {
        if self.size == 0 {
            return None;
        }
        let result = self.heap[1].take();
        self.heap[1] = self.heap[self.size].take();
        self.size -= 1;
        if self.size > 0 {
            self.down_heap(1, less);
        }
        result
    }

    pub fn top(&self) -> Option<&T> {
        if self.size == 0 { None } else { self.heap[1].as_ref() }
    }

    pub fn top_mut(&mut self) -> Option<&mut T> {
        if self.size == 0 { None } else { self.heap[1].as_mut() }
    }

    /// `updateTop()`: re-sifts the top after it changed.
    pub fn update_top(&mut self, less: &impl Fn(&T, &T) -> bool) {
        if self.size > 0 {
            self.down_heap(1, less);
        }
    }

    /// Pops everything: the least first.
    pub fn drain_ascending(mut self, less: &impl Fn(&T, &T) -> bool) -> Vec<T> {
        let mut out = Vec::with_capacity(self.size);
        while let Some(x) = self.pop(less) {
            out.push(x);
        }
        out
    }
}

/// The length of an Elasticsearch `BigArrays` array of 8-byte elements (`LongArray`,
/// `DoubleArray`) as it grows (`grow` → `overSize`; arrays of 1024+ elements are paged, as with
/// the request recycler), and the exception reading past it throws — Elasticsearch has paths that
/// read such arrays out of bounds, and its responses then are those errors.
#[derive(Clone, Copy, Debug)]
pub struct BigArraySize {
    pub len: u64,
    paged: bool,
}

impl BigArraySize {
    const PAGE: u64 = 2048;

    /// `newXArray(1)`.
    pub fn new() -> BigArraySize {
        BigArraySize { len: 1, paged: false }
    }

    /// `BigArrays.grow(values, min)`.
    pub fn grow(&mut self, min: u64) {
        if min <= self.len {
            return;
        }
        let new = if min < Self::PAGE {
            // `ArrayUtil.oversize(min, 8)`, capped at a page.
            (min + (min >> 3).max(3)).min(Self::PAGE)
        } else {
            min.div_ceil(Self::PAGE) * Self::PAGE
        };
        // `resize`: paged arrays stay paged; others are reallocated, paged from half a page
        // (the recycler's threshold).
        self.paged = self.paged || new > Self::PAGE || new >= Self::PAGE / 2;
        self.len = new;
    }

    /// `values.get(index)`: the exception Java throws reading past the array, if any.
    pub fn check(&self, index: u64) -> Result<()> {
        let oob = |i: u64, l: u64| AggError::new(500, "array_index_out_of_bounds_exception", format!("Index {i} out of bounds for length {l}"));
        if self.paged {
            let pages = self.len.div_ceil(Self::PAGE);
            let page = index >> 11;
            if page >= pages {
                return Err(oob(page, pages));
            }
        } else if index >= self.len {
            // A `VarHandle` view over `byte[len * 8]`.
            return Err(oob(index * 8, self.len * 8 - 7));
        }
        Ok(())
    }
}


impl Default for BigArraySize {
    fn default() -> Self {
        BigArraySize::new()
    }
}

/// What [`intro_sort`] sorts (Lucene's `IntroSorter` callbacks).
pub trait IntroSortable {
    fn swap(&mut self, i: usize, j: usize);
    fn set_pivot(&mut self, i: usize);
    /// The pivot compared with element `j` (negative: pivot first).
    fn compare_pivot(&self, j: usize) -> std::cmp::Ordering;
    /// `compare(i, j)`: `setPivot(i)` then `comparePivot(j)` (it moves the pivot, like Lucene).
    fn compare(&mut self, i: usize, j: usize) -> std::cmp::Ordering {
        self.set_pivot(i);
        self.compare_pivot(j)
    }
}

/// Lucene's `IntroSorter.sort(from, to)`: quicksort with median pivots and Bentley-McIlroy
/// three-way partitioning, insertion sort below 17 elements, heapsort past the depth limit —
/// step for step, so ties land where Lucene puts them.
pub fn intro_sort<S: IntroSortable + ?Sized>(s: &mut S, from: usize, to: usize) {
    if to <= from {
        return;
    }
    // `2 * MathUtil.log(to - from, 2)`.
    let mut n = to - from;
    let mut log = 0i32;
    while n >= 2 {
        n /= 2;
        log += 1;
    }
    intro_sort_rec(s, from, to, 2 * log);
}

fn intro_sort_rec<S: IntroSortable + ?Sized>(s: &mut S, mut from: usize, mut to: usize, mut max_depth: i32) {
    use std::cmp::Ordering::{Equal, Greater, Less};
    const INSERTION_SORT_THRESHOLD: usize = 16;
    const SINGLE_MEDIAN_THRESHOLD: usize = 40;
    loop {
        let size = to - from;
        if size <= INSERTION_SORT_THRESHOLD {
            break;
        }
        max_depth -= 1;
        if max_depth < 0 {
            heap_sort(s, from, to);
            return;
        }
        let last = to - 1;
        let mid = (from + last) >> 1;
        let pivot = if size <= SINGLE_MEDIAN_THRESHOLD {
            let range = size >> 2;
            median(s, mid - range, mid, mid + range)
        } else {
            let range = size >> 3;
            let double_range = range << 1;
            let first = median(s, from, from + range, from + double_range);
            let middle = median(s, mid - range, mid, mid + range);
            let end = median(s, last - double_range, last - range, last);
            median(s, first, middle, end)
        };
        s.set_pivot(pivot);
        s.swap(from, pivot);
        let mut i = from;
        let mut j = to;
        let mut p = from + 1;
        let mut q = last;
        loop {
            let mut left;
            loop {
                i += 1;
                left = s.compare_pivot(i);
                if left != Greater {
                    break;
                }
            }
            let mut right;
            loop {
                j -= 1;
                right = s.compare_pivot(j);
                if right != Less {
                    break;
                }
            }
            if i >= j {
                if i == j && right == Equal {
                    s.swap(i, p);
                }
                break;
            }
            s.swap(i, j);
            if right == Equal {
                s.swap(i, p);
                p += 1;
            }
            if left == Equal {
                s.swap(j, q);
                q -= 1;
            }
        }
        i = j + 1;
        let mut k = from;
        while k < p {
            s.swap(k, j);
            k += 1;
            j = j.wrapping_sub(1);
        }
        let mut k = last;
        while k > q {
            s.swap(k, i);
            k -= 1;
            i += 1;
        }
        // `j` may have stepped below `from` (Java's int goes to `from - 1`).
        let j1 = j.wrapping_add(1);
        if (j1 as isize - 1 - from as isize) < (last as isize - i as isize) {
            intro_sort_rec(s, from, j1, max_depth);
            from = i;
        } else {
            intro_sort_rec(s, i, to, max_depth);
            to = j1;
        }
    }
    // `Sorter.insertionSort`.
    let mut i = from + 1;
    while i < to {
        let mut current = i;
        i += 1;
        loop {
            let previous = current - 1;
            if s.compare(previous, current) != Greater {
                break;
            }
            s.swap(previous, current);
            if previous == from {
                break;
            }
            current = previous;
        }
    }
}

fn median<S: IntroSortable + ?Sized>(s: &mut S, i: usize, j: usize, k: usize) -> usize {
    use std::cmp::Ordering::{Greater, Less};
    if s.compare(i, j) == Less {
        if s.compare(j, k) != Greater {
            return j;
        }
        return if s.compare(i, k) == Less { k } else { i };
    }
    if s.compare(j, k) != Less {
        return j;
    }
    if s.compare(i, k) == Less { i } else { k }
}

/// `Sorter.heapSort`.
fn heap_sort<S: IntroSortable + ?Sized>(s: &mut S, from: usize, to: usize) {
    if to - from <= 1 {
        return;
    }
    // `heapify`: from `heapParent(from, to - 1)` down to `from`.
    let mut i = ((to - 1 - from - 1) >> 1) + from;
    loop {
        sift_down(s, i, from, to);
        if i == from {
            break;
        }
        i -= 1;
    }
    let mut end = to - 1;
    while end > from {
        s.swap(from, end);
        sift_down(s, from, from, end);
        end -= 1;
    }
}

fn sift_down<S: IntroSortable + ?Sized>(s: &mut S, mut i: usize, from: usize, to: usize) {
    use std::cmp::Ordering::Less;
    loop {
        let left = ((i - from) << 1) + 1 + from;
        if left >= to {
            break;
        }
        let right = left + 1;
        if s.compare(i, left) == Less {
            if right < to && s.compare(left, right) == Less {
                s.swap(i, right);
                i = right;
            } else {
                s.swap(i, left);
                i = left;
            }
        } else if right < to && s.compare(i, right) == Less {
            s.swap(i, right);
            i = right;
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn paged_hash_map_order_is_by_slot() {
        // Capacity 2: 4 buckets (max 2 entries); the third put grows to 8 and re-adds.
        let order = super::paged_hash_map_order(&[5, 1, 4, 3], 2);
        assert_eq!(order, vec![1, 3, 2, 0]);
        assert_eq!(super::java_string_hash_code("k01"), 104364);
    }

    #[test]
    fn lucene_merge_matches_priority_queue_order() {
        // Three shards with the same keys: the first group pops in list order, later groups in
        // the order the heap leaves them (computed by hand from Lucene's PriorityQueue).
        let lists = vec![vec![(1, 'a'), (2, 'a'), (3, 'a')], vec![(1, 'b'), (2, 'b'), (3, 'b')], vec![(1, 'c'), (2, 'c'), (3, 'c')]];
        let groups = super::lucene_merge_groups(lists, |x: &(i32, char), y: &(i32, char)| x.0 < y.0);
        let order: Vec<String> = groups.iter().map(|g| g.iter().map(|x| x.1).collect()).collect();
        assert_eq!(order[0], "abc");
        assert_eq!(groups.len(), 3);
    }

    use super::*;

    #[test]
    fn murmur_known_values() {
        // Lucene StringHelper.murmurhash3_x86_32 on "hello" with seed 0 (same as the reference
        // MurmurHash3_x86_32).
        assert_eq!(murmur3_x86_32(b"hello", 0) as u32, 0x248bfa47);
        assert_eq!(murmur3_x86_32(b"", 0), 0);
        // Reference MurmurHash3_x64_128("hello", 0).
        let (h1, h2) = murmur3_x64_128(b"hello", 0);
        assert_eq!(h1, 0xcbd8a7b341bd9b02);
        assert_eq!(h2, 0x5b1e906a48ae1d19);
    }

    #[test]
    fn ord_maps() {
        let mut m = OrdMap::new();
        assert_eq!(m.add(0, 5), (0, true));
        assert_eq!(m.add(1, 5), (1, true));
        assert_eq!(m.add(0, 5), (0, false));
        assert_eq!(m.find(1, 5), Some(1));
        assert_eq!(m.find(2, 5), None);
        let mut b = BytesOrdMap::new();
        assert_eq!(b.add(0, b"x"), (0, true));
        assert_eq!(b.add(0, b"y"), (1, true));
        assert_eq!(b.add(0, b"x"), (0, false));
        assert_eq!(b.get_bytes(1), b"y");
        for i in 0..1000u32 {
            b.add(i % 7, format!("k{i}").as_bytes());
        }
        assert_eq!(b.find(3, b"k10"), Some(b.find(3, b"k10").unwrap()));
        assert_eq!(b.get_bytes(b.find(3, b"k10").unwrap()), b"k10");
    }
}
