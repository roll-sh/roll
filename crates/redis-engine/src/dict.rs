//! A chained hash table with incremental rehashing, like Redis' `dict`.
//!
//! Why not `HashMap`: SCAN needs a cursor that survives resizes and guarantees every element
//! present for the whole iteration is returned. That relies on chaining (an element always lives
//! in bucket `hash & mask`) plus the reverse-binary cursor walk. Incremental rehashing also keeps
//! resizes of multi-million-key tables from stalling the shard.

use foldhash::fast::RandomState;
use std::hash::BuildHasher;

use crate::util::Rng;

struct Node<K, V> {
    key: K,
    val: V,
    next: Link<K, V>,
}

type Link<K, V> = Option<Box<Node<K, V>>>;

pub struct Dict<K, V> {
    t0: Vec<Link<K, V>>,
    /// Non-empty only while rehashing from `t0` into it.
    t1: Vec<Link<K, V>>,
    /// Next bucket of `t0` to move while rehashing.
    rehash_idx: usize,
    len: usize,
    hasher: RandomState,
    /// Grows and shrinks on its own (DEBUG DICT-RESIZING 0 turns this off for the keyspace).
    resize: bool,
}

const MIN_SIZE: usize = 4;
/// Empty buckets visited per rehash step before giving up (Redis uses 10x the step count).
const EMPTY_VISITS: usize = 10;

impl<K: AsRef<[u8]>, V> Default for Dict<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: AsRef<[u8]>, V> Dict<K, V> {
    pub fn new() -> Self {
        Self { t0: Vec::new(), t1: Vec::new(), rehash_idx: 0, len: 0, hasher: RandomState::default(), resize: true }
    }

    /// Allow or stop automatic expands and shrinks (`dictTypeResizeAllowed`). Explicit
    /// [`Dict::expand`] still applies.
    pub fn set_resize(&mut self, on: bool) {
        self.resize = on;
    }

    pub fn with_capacity(n: usize) -> Self {
        let mut d = Self::new();
        if n > 0 {
            d.t0 = empty_table(n.next_power_of_two().max(MIN_SIZE));
        }
        d
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_rehashing(&self) -> bool {
        !self.t1.is_empty()
    }

    /// `dictGetStatsHt` for the main table and, while rehashing, the target table.
    pub fn stats(&self, full: bool) -> Vec<DictStats> {
        let target = chains(&self.t1).sum::<usize>();
        let mut out = vec![DictStats::new(0, self.t0.len(), self.len - target, full.then(|| chains(&self.t0)))];
        if self.is_rehashing() {
            out.push(DictStats::new(1, self.t1.len(), target, full.then(|| chains(&self.t1))));
        }
        out
    }

    /// `dictTryExpand`: make room for `n` elements now (no-op while rehashing, or if fewer than
    /// the elements already there).
    pub fn expand(&mut self, n: usize) {
        if self.is_rehashing() || self.len > n {
            return;
        }
        self.start_resize(n.next_power_of_two().max(MIN_SIZE));
    }

    /// Bytes used by the bucket arrays (nodes are accounted by the caller).
    pub fn table_bytes(&self) -> usize {
        (self.t0.capacity() + self.t1.capacity()) * std::mem::size_of::<Link<K, V>>()
    }

    /// Buckets of both tables (`dictBuckets`).
    pub fn buckets(&self) -> usize {
        self.t0.len() + self.t1.len()
    }

    /// Bytes per node beyond the key and value contents.
    pub const NODE_OVERHEAD: usize = std::mem::size_of::<Node<K, V>>() + 16;

    #[inline]
    fn hash(&self, key: &[u8]) -> u64 {
        self.hasher.hash_one(key)
    }

    pub fn get(&self, key: &[u8]) -> Option<&V> {
        self.get_key_value(key).map(|(_, v)| v)
    }

    pub fn get_key_value(&self, key: &[u8]) -> Option<(&K, &V)> {
        if self.len == 0 {
            return None;
        }
        let h = self.hash(key);
        for t in self.tables() {
            let mut cur = t[(h as usize) & (t.len() - 1)].as_deref();
            while let Some(n) = cur {
                if n.key.as_ref() == key {
                    return Some((&n.key, &n.val));
                }
                cur = n.next.as_deref();
            }
        }
        None
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.get_key_value(key).is_some()
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        if self.len == 0 {
            return None;
        }
        self.rehash_step();
        let h = self.hash(key);
        let (t0, t1) = (&mut self.t0, &mut self.t1);
        for t in [t0, t1] {
            if t.is_empty() {
                continue;
            }
            let mask = t.len() - 1;
            let mut cur = t[(h as usize) & mask].as_deref_mut();
            while let Some(n) = cur {
                if n.key.as_ref() == key {
                    return Some(&mut n.val);
                }
                cur = n.next.as_deref_mut();
            }
        }
        None
    }

    /// Insert or replace; returns the previous value.
    pub fn insert(&mut self, key: K, val: V) -> Option<V> {
        if let Some(v) = self.get_mut(key.as_ref()) {
            return Some(std::mem::replace(v, val));
        }
        self.insert_new(key, val);
        None
    }

    /// Insert a key known to be absent.
    pub fn insert_new(&mut self, key: K, val: V) -> &mut V {
        self.rehash_step();
        self.expand_if_needed();
        let h = self.hash(key.as_ref());
        let t = if self.is_rehashing() { &mut self.t1 } else { &mut self.t0 };
        let idx = (h as usize) & (t.len() - 1);
        let next = t[idx].take();
        t[idx] = Some(Box::new(Node { key, val, next }));
        self.len += 1;
        &mut t[idx].as_mut().unwrap().val
    }

    /// Get the value for `key`, inserting `make()` first when absent. Returns `(value, inserted)`.
    pub fn get_or_insert_with(
        &mut self,
        key: &[u8],
        make_key: impl FnOnce() -> K,
        make: impl FnOnce() -> V,
    ) -> (&mut V, bool) {
        // Two lookups keep the borrow checker happy; the first is a cheap probe.
        if self.contains_key(key) {
            return (self.get_mut(key).unwrap(), false);
        }
        (self.insert_new(make_key(), make()), true)
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<V> {
        self.remove_entry(key).map(|(_, v)| v)
    }

    pub fn remove_entry(&mut self, key: &[u8]) -> Option<(K, V)> {
        if self.len == 0 {
            return None;
        }
        self.rehash_step();
        let h = self.hash(key);
        let mut out = None;
        'tables: for t in [&mut self.t0, &mut self.t1] {
            if t.is_empty() {
                continue;
            }
            let mask = t.len() - 1;
            let mut link = &mut t[(h as usize) & mask];
            loop {
                match link {
                    None => break,
                    Some(n) if n.key.as_ref() == key => {
                        let mut node = link.take().unwrap();
                        *link = node.next.take();
                        out = Some((node.key, node.val));
                        break 'tables;
                    }
                    Some(n) => link = &mut n.next,
                }
            }
        }
        if out.is_some() {
            self.len -= 1;
            self.shrink_if_needed();
        }
        out
    }

    pub fn clear(&mut self) {
        // Drop chains iteratively so very long chains can't overflow the stack.
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut cur = b.take();
                while let Some(mut n) = cur {
                    cur = n.next.take();
                }
            }
        }
        self.t0 = Vec::new();
        self.t1 = Vec::new();
        self.rehash_idx = 0;
        self.len = 0;
    }

    fn tables(&self) -> impl Iterator<Item = &Vec<Link<K, V>>> {
        [&self.t0, &self.t1].into_iter().filter(|t| !t.is_empty())
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter { tables: [&self.t0, &self.t1], table: 0, bucket: 0, node: None, remaining: self.len }
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// Visit every value mutably.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(&K, &mut V)) {
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut cur = b.as_deref_mut();
                while let Some(n) = cur {
                    f(&n.key, &mut n.val);
                    cur = n.next.as_deref_mut();
                }
            }
        }
    }

    /// Remove every entry for which `keep` returns false.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let mut removed = 0;
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut cur = b.take();
                let mut kept: Link<K, V> = None;
                while let Some(mut n) = cur {
                    cur = n.next.take();
                    if keep(&n.key, &mut n.val) {
                        n.next = kept;
                        kept = Some(n);
                    } else {
                        removed += 1;
                    }
                }
                *b = kept;
            }
        }
        self.len -= removed;
        self.shrink_if_needed();
    }

    /// Move everything out.
    pub fn drain(&mut self) -> Vec<(K, V)> {
        let mut out = Vec::with_capacity(self.len);
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut cur = b.take();
                while let Some(mut n) = cur {
                    cur = n.next.take();
                    out.push((n.key, n.val));
                }
            }
        }
        self.clear();
        out
    }

    /// One SCAN step: calls `f` for every element of the bucket(s) at `cursor` and returns the
    /// next cursor (0 when the iteration is complete).
    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&K, &V)) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let mut v = cursor;
        let emit = |t: &Vec<Link<K, V>>, idx: u64, f: &mut dyn FnMut(&K, &V)| {
            let mut cur = t[idx as usize].as_deref();
            while let Some(n) = cur {
                f(&n.key, &n.val);
                cur = n.next.as_deref();
            }
        };
        if !self.is_rehashing() {
            let m0 = (self.t0.len() - 1) as u64;
            emit(&self.t0, v & m0, &mut f);
            v |= !m0;
            v = v.reverse_bits().wrapping_add(1).reverse_bits();
        } else {
            let (small, large) =
                if self.t0.len() <= self.t1.len() { (&self.t0, &self.t1) } else { (&self.t1, &self.t0) };
            let m0 = (small.len() - 1) as u64;
            let m1 = (large.len() - 1) as u64;
            emit(small, v & m0, &mut f);
            loop {
                emit(large, v & m1, &mut f);
                v |= !m1;
                v = v.reverse_bits().wrapping_add(1).reverse_bits();
                if v & (m0 ^ m1) == 0 {
                    break;
                }
            }
        }
        v
    }

    /// A random element (bucket-uniform, like `dictGetRandomKey`).
    pub fn random(&self, rng: &mut Rng) -> Option<(&K, &V)> {
        if self.len == 0 {
            return None;
        }
        let (n0, n1) = (self.t0.len(), self.t1.len());
        let head = loop {
            let i = if self.is_rehashing() {
                // Buckets of t0 below the rehash index are empty.
                self.rehash_idx + rng.below(n0 + n1 - self.rehash_idx)
            } else {
                rng.below(n0)
            };
            let b = if i < n0 { &self.t0[i] } else { &self.t1[i - n0] };
            if let Some(n) = b.as_deref() {
                break n;
            }
        };
        let mut len = 0;
        let mut cur = Some(head);
        while let Some(n) = cur {
            len += 1;
            cur = n.next.as_deref();
        }
        let mut pick = rng.below(len);
        let mut cur = head;
        while pick > 0 {
            cur = cur.next.as_deref().unwrap();
            pick -= 1;
        }
        Some((&cur.key, &cur.val))
    }

    /// Up to `count` elements from consecutive buckets starting at a random one
    /// (`dictGetSomeKeys`): cheap sampling for eviction and active expiry.
    pub fn sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(&K, &V)) {
        if self.len == 0 || count == 0 {
            return;
        }
        let count = count.min(self.len);
        let tables: Vec<&Vec<Link<K, V>>> = self.tables().collect();
        let maxsize = tables.iter().map(|t| t.len()).max().unwrap();
        let mut i = rng.below(maxsize);
        let mut got = 0;
        let mut empty = 0;
        let mut steps = count * 10;
        while got < count && steps > 0 {
            steps -= 1;
            for (ti, t) in tables.iter().enumerate() {
                // Skip already-rehashed buckets of t0.
                if ti == 0 && self.is_rehashing() && i < self.rehash_idx {
                    continue;
                }
                if i >= t.len() {
                    continue;
                }
                let mut cur = t[i].as_deref();
                if cur.is_none() {
                    empty += 1;
                    if empty >= 5 && empty > count {
                        i = rng.below(maxsize);
                        empty = 0;
                    }
                }
                while let Some(n) = cur {
                    f(&n.key, &n.val);
                    got += 1;
                    if got >= count {
                        return;
                    }
                    cur = n.next.as_deref();
                }
            }
            i = (i + 1) & (maxsize - 1);
        }
    }

    /// Move up to `n` buckets from the old table. Returns true while more rehashing is pending.
    pub fn rehash(&mut self, n: usize) -> bool {
        if !self.is_rehashing() {
            // Idle tables get resized from the cron, like Redis' tryResizeHashTables.
            self.shrink_if_needed();
            if !self.is_rehashing() {
                return false;
            }
        }
        let mut empty_visits = n * EMPTY_VISITS;
        let mut n = n;
        while n > 0 && self.rehash_idx < self.t0.len() {
            n -= 1;
            while self.t0[self.rehash_idx].is_none() {
                self.rehash_idx += 1;
                empty_visits -= 1;
                if self.rehash_idx >= self.t0.len() {
                    break;
                }
                if empty_visits == 0 {
                    return true;
                }
            }
            if self.rehash_idx >= self.t0.len() {
                break;
            }
            let mask = self.t1.len() - 1;
            let mut cur = self.t0[self.rehash_idx].take();
            while let Some(mut node) = cur {
                cur = node.next.take();
                let idx = (self.hasher.hash_one(node.key.as_ref()) as usize) & mask;
                node.next = self.t1[idx].take();
                self.t1[idx] = Some(node);
            }
            self.rehash_idx += 1;
        }
        if self.rehash_idx >= self.t0.len() {
            self.t0 = std::mem::take(&mut self.t1);
            self.rehash_idx = 0;
            // Elements removed during the rehash may call for a further shrink.
            self.shrink_if_needed();
            return self.is_rehashing();
        }
        true
    }

    #[inline]
    fn rehash_step(&mut self) {
        if self.is_rehashing() {
            self.rehash(1);
        }
    }

    fn start_resize(&mut self, size: usize) {
        if self.t0.is_empty() {
            self.t0 = empty_table(size);
            return;
        }
        if self.is_rehashing() || size == self.t0.len() {
            return;
        }
        self.t1 = empty_table(size);
        self.rehash_idx = 0;
    }

    fn expand_if_needed(&mut self) {
        if self.is_rehashing() {
            return;
        }
        if self.t0.is_empty() {
            self.t0 = empty_table(MIN_SIZE);
        } else if self.resize && self.len >= self.t0.len() {
            self.start_resize((self.len + 1).next_power_of_two());
        }
    }

    fn shrink_if_needed(&mut self) {
        if self.is_rehashing() || self.t0.len() <= MIN_SIZE || !self.resize {
            return;
        }
        if self.len == 0 {
            self.t0 = Vec::new();
            return;
        }
        // Shrink when less than 1/8 full (Redis 7.4+).
        if self.len * 8 <= self.t0.len() {
            self.start_resize(self.len.next_power_of_two().max(MIN_SIZE));
        }
    }
}

/// Chain-length histogram slots (`DICT_STATS_VECTLEN`).
const STATS_VECTLEN: usize = 50;

/// One hash table's statistics (`dictStats`), as DEBUG HTSTATS prints them.
#[derive(Clone, Debug)]
pub struct DictStats {
    pub htidx: u8,
    pub size: usize,
    pub used: usize,
    pub buckets: usize,
    pub max_chain: usize,
    pub total_chain: usize,
    pub chains: [usize; STATS_VECTLEN],
}

impl DictStats {
    /// From each bucket's chain length (only with `full`).
    pub fn new(htidx: u8, size: usize, used: usize, chains: Option<impl Iterator<Item = usize>>) -> Self {
        let mut st = DictStats { htidx, size, used, buckets: 0, max_chain: 0, total_chain: 0, chains: [0; STATS_VECTLEN] };
        for n in chains.into_iter().flatten() {
            st.chains[n.min(STATS_VECTLEN - 1)] += 1;
            if n > 0 {
                st.buckets += 1;
                st.max_chain = st.max_chain.max(n);
                st.total_chain += n;
            }
        }
        st
    }

    /// `dictCombineStats` (a kvstore's dicts summed).
    pub fn combine(&mut self, o: &DictStats) {
        self.buckets += o.buckets;
        self.max_chain = self.max_chain.max(o.max_chain);
        self.total_chain += o.total_chain;
        self.size += o.size;
        self.used += o.used;
        for (a, b) in self.chains.iter_mut().zip(o.chains) {
            *a += b;
        }
    }

    /// `dictGetStatsMsg`.
    pub fn msg(&self, full: bool) -> String {
        use std::fmt::Write;
        let name = if self.htidx == 0 { "main hash table" } else { "rehashing target" };
        if self.used == 0 {
            return format!("Hash table {} stats ({name}):\nNo stats available for empty dictionaries\n", self.htidx);
        }
        let mut s = format!("Hash table {} stats ({name}):\n table size: {}\n number of elements: {}\n", self.htidx, self.size, self.used);
        if full {
            // C divides in float and prints the result as a double.
            let ratio = |a: usize, b: usize| (a as f32 / b as f32) as f64;
            let _ = write!(
                s,
                " different slots: {}\n max chain length: {}\n avg chain length (counted): {:.2}\n avg chain length (computed): {:.2}\n Chain length distribution:\n",
                self.buckets,
                self.max_chain,
                ratio(self.total_chain, self.buckets),
                ratio(self.used, self.buckets)
            );
            for (i, &n) in self.chains[..STATS_VECTLEN - 1].iter().enumerate() {
                if n != 0 {
                    let _ = writeln!(s, "   {i}: {n} ({:.2}%)", ((n as f32 / self.size as f32) * 100.0) as f64);
                }
            }
        }
        s
    }
}

/// Each bucket's chain length.
fn chains<K, V>(t: &[Link<K, V>]) -> impl Iterator<Item = usize> + '_ {
    t.iter().map(|b| {
        let (mut n, mut cur) = (0, b.as_deref());
        while let Some(node) = cur {
            n += 1;
            cur = node.next.as_deref();
        }
        n
    })
}

fn empty_table<K, V>(n: usize) -> Vec<Link<K, V>> {
    let mut v = Vec::with_capacity(n);
    v.resize_with(n, || None);
    v
}

impl<K, V> Drop for Dict<K, V> {
    fn drop(&mut self) {
        for t in [&mut self.t0, &mut self.t1] {
            for b in t.iter_mut() {
                let mut cur = b.take();
                while let Some(mut n) = cur {
                    cur = n.next.take();
                }
            }
        }
    }
}

impl<K: AsRef<[u8]> + Clone, V: Clone> Clone for Dict<K, V> {
    fn clone(&self) -> Self {
        let mut d = Dict::with_capacity(self.len);
        for (k, v) in self.iter() {
            d.insert_new(k.clone(), v.clone());
        }
        d
    }
}

impl<K: AsRef<[u8]> + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for Dict<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

pub struct Iter<'a, K, V> {
    tables: [&'a Vec<Link<K, V>>; 2],
    table: usize,
    bucket: usize,
    node: Option<&'a Node<K, V>>,
    remaining: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(n) = self.node {
                self.node = n.next.as_deref();
                self.remaining -= 1;
                return Some((&n.key, &n.val));
            }
            if self.table >= 2 {
                return None;
            }
            let t = self.tables[self.table];
            if self.bucket >= t.len() {
                self.table += 1;
                self.bucket = 0;
                continue;
            }
            self.node = t[self.bucket].as_deref();
            self.bucket += 1;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn stats_and_expand() {
        let mut d: Dict<Vec<u8>, ()> = Dict::new();
        d.expand(100);
        assert_eq!(d.t0.len(), 128);
        d.insert(b"a".to_vec(), ());
        let st = d.stats(true);
        assert_eq!((st.len(), st[0].size, st[0].used, st[0].buckets, st[0].max_chain), (1, 128, 1, 1, 1));
        assert_eq!(
            st[0].msg(true),
            "Hash table 0 stats (main hash table):\n table size: 128\n number of elements: 1\n different slots: 1\n max chain length: 1\n \
             avg chain length (counted): 1.00\n avg chain length (computed): 1.00\n Chain length distribution:\n   0: 127 (99.22%)\n   1: 1 (0.78%)\n"
        );
        assert_eq!(st[0].msg(false), "Hash table 0 stats (main hash table):\n table size: 128\n number of elements: 1\n");
        // Smaller than the elements there, or the same size: nothing to do.
        d.expand(0);
        d.expand(65);
        assert!(!d.is_rehashing());
        d.expand(1000);
        assert!(d.is_rehashing());
        let st = d.stats(false);
        assert_eq!((st[1].htidx, st[1].size, st[0].used + st[1].used), (1, 1024, 1));
        let empty: Dict<Vec<u8>, ()> = Dict::new();
        assert_eq!(empty.stats(false)[0].msg(false), "Hash table 0 stats (main hash table):\nNo stats available for empty dictionaries\n");
    }

    fn key(i: usize) -> Vec<u8> {
        format!("key:{i}").into_bytes()
    }

    #[test]
    fn insert_get_remove_through_resizes() {
        let mut d: Dict<Vec<u8>, usize> = Dict::new();
        for i in 0..10_000 {
            assert!(d.insert(key(i), i).is_none());
        }
        assert_eq!(d.len(), 10_000);
        for i in 0..10_000 {
            assert_eq!(d.get(&key(i)), Some(&i));
        }
        assert_eq!(d.insert(key(5), 55), Some(5));
        for i in 0..9_990 {
            assert_eq!(d.remove(&key(i)), Some(if i == 5 { 55 } else { i }));
        }
        assert_eq!(d.len(), 10);
        while d.rehash(100) {}
        assert!(d.t0.len() <= 16, "shrunk to {}", d.t0.len());
        assert_eq!(d.iter().count(), 10);
    }

    #[test]
    fn scan_returns_everything_despite_resizes() {
        let mut d: Dict<Vec<u8>, ()> = Dict::new();
        for i in 0..1000 {
            d.insert(key(i), ());
        }
        let mut seen = HashSet::new();
        let mut cursor = 0;
        let mut round = 0;
        loop {
            cursor = d.scan(cursor, |k, _| {
                seen.insert(k.clone());
            });
            // Grow and shrink the table mid-scan with elements that aren't guaranteed.
            round += 1;
            if round == 3 {
                for i in 1000..5000 {
                    d.insert(key(i), ());
                }
            }
            if round == 50 {
                for i in 1000..5000 {
                    d.remove(&key(i));
                }
            }
            if cursor == 0 {
                break;
            }
        }
        for i in 0..1000 {
            assert!(seen.contains(&key(i)), "missing {i}");
        }
    }

    #[test]
    fn random_and_sample() {
        let mut d: Dict<Vec<u8>, usize> = Dict::new();
        let mut rng = Rng::seeded(7);
        assert!(d.random(&mut rng).is_none());
        for i in 0..100 {
            d.insert(key(i), i);
        }
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            seen.insert(*d.random(&mut rng).unwrap().1);
        }
        assert!(seen.len() > 90);
        let mut n = 0;
        d.sample(&mut rng, 5, |_, _| n += 1);
        assert_eq!(n, 5);
    }

    #[test]
    fn retain_and_drain() {
        let mut d: Dict<Vec<u8>, usize> = Dict::new();
        for i in 0..100 {
            d.insert(key(i), i);
        }
        d.retain(|_, v| *v % 2 == 0);
        assert_eq!(d.len(), 50);
        assert!(d.get(&key(3)).is_none());
        let all = d.drain();
        assert_eq!(all.len(), 50);
        assert!(d.is_empty());
    }
}
