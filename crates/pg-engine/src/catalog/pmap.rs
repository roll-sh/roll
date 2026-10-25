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
use std::collections::BTreeMap;
use std::sync::Arc;

/// log2 of the oids per chunk. Oids of one database's objects are mostly consecutive (a
/// table takes a few: its row type, array type, constraints, index, sequence), so chunks are
/// well filled, and a chunk to copy stays small.
const SHIFT: u32 = 6;

type Chunk<V> = Arc<BTreeMap<u32, V>>;

pub struct PMap<V> {
    chunks: Arc<BTreeMap<u32, Chunk<V>>>,
    len: usize,
}

impl<V> Clone for PMap<V> {
    fn clone(&self) -> Self {
        PMap {
            chunks: self.chunks.clone(),
            len: self.len,
        }
    }
}

impl<V> Default for PMap<V> {
    fn default() -> Self {
        PMap {
            chunks: Arc::new(BTreeMap::new()),
            len: 0,
        }
    }
}

type ChunkIter<'a, V> = std::collections::btree_map::Iter<'a, u32, V>;

/// `PMap::iter`'s iterator (also `&PMap`'s `IntoIterator`).
pub type Iter<'a, V> = std::iter::FlatMap<
    std::collections::btree_map::Values<'a, u32, Chunk<V>>,
    ChunkIter<'a, V>,
    fn(&'a Chunk<V>) -> ChunkIter<'a, V>,
>;

fn chunk_iter<V>(c: &Chunk<V>) -> ChunkIter<'_, V> {
    c.iter()
}

impl<V> PMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, k: &u32) -> Option<&V> {
        self.chunks.get(&(k >> SHIFT))?.get(k)
    }

    pub fn contains_key(&self, k: &u32) -> bool {
        self.get(k).is_some()
    }

    pub fn iter(&self) -> Iter<'_, V> {
        self.chunks.values().flat_map(chunk_iter as fn(&Chunk<V>) -> ChunkIter<'_, V>)
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
        Arc::ptr_eq(&self.chunks, &other.chunks)
    }

    /// The entries in which `self` and `new` differ, in key order: `f(key, in self, in new)`
    /// for each key only one has, and for each both have where `same` says the values
    /// differ. Chunks the two share aren't visited.
    pub fn diff<'a>(
        &'a self,
        new: &'a Self,
        same: impl Fn(&V, &V) -> bool,
        mut f: impl FnMut(u32, Option<&'a V>, Option<&'a V>),
    ) {
        if self.ptr_eq(new) {
            return;
        }
        let mut a = self.chunks.iter().peekable();
        let mut b = new.chunks.iter().peekable();
        loop {
            match (a.peek(), b.peek()) {
                (None, None) => return,
                (Some((ka, ca)), Some((kb, cb))) if ka == kb => {
                    if !Arc::ptr_eq(ca, cb) {
                        diff_chunk(ca, cb, &same, &mut f);
                    }
                    a.next();
                    b.next();
                }
                (Some((ka, ca)), kb) if kb.is_none_or(|(kb, _)| ka < kb) => {
                    for (k, v) in ca.iter() {
                        f(*k, Some(v), None);
                    }
                    a.next();
                }
                (_, Some((_, cb))) => {
                    for (k, v) in cb.iter() {
                        f(*k, None, Some(v));
                    }
                    b.next();
                }
                (Some(_), None) => unreachable!(),
            }
        }
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

fn diff_chunk<'a, V>(
    a: &'a BTreeMap<u32, V>,
    b: &'a BTreeMap<u32, V>,
    same: &impl Fn(&V, &V) -> bool,
    f: &mut impl FnMut(u32, Option<&'a V>, Option<&'a V>),
) {
    let mut ia = a.iter().peekable();
    let mut ib = b.iter().peekable();
    loop {
        match (ia.peek(), ib.peek()) {
            (None, None) => return,
            (Some((ka, va)), Some((kb, vb))) if ka == kb => {
                if !same(va, vb) {
                    f(**ka, Some(va), Some(vb));
                }
                ia.next();
                ib.next();
            }
            (Some((ka, va)), kb) if kb.is_none_or(|(kb, _)| ka < kb) => {
                f(**ka, Some(va), None);
                ia.next();
            }
            (_, Some((kb, vb))) => {
                f(**kb, None, Some(vb));
                ib.next();
            }
            (Some(_), None) => unreachable!(),
        }
    }
}

impl<V: Clone> PMap<V> {
    pub fn insert(&mut self, k: u32, v: V) -> Option<V> {
        let chunk = Arc::make_mut(&mut self.chunks).entry(k >> SHIFT).or_default();
        let old = Arc::make_mut(chunk).insert(k, v);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn remove(&mut self, k: &u32) -> Option<V> {
        let c = k >> SHIFT;
        if !self.chunks.get(&c).is_some_and(|ch| ch.contains_key(k)) {
            return None;
        }
        let chunks = Arc::make_mut(&mut self.chunks);
        let chunk = chunks.get_mut(&c)?;
        let old = Arc::make_mut(chunk).remove(k);
        if chunk.is_empty() {
            chunks.remove(&c);
        }
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn get_mut(&mut self, k: &u32) -> Option<&mut V> {
        let c = k >> SHIFT;
        if !self.chunks.get(&c).is_some_and(|ch| ch.contains_key(k)) {
            return None;
        }
        let chunk = Arc::make_mut(&mut self.chunks).get_mut(&c)?;
        Arc::make_mut(chunk).get_mut(k)
    }

    /// Every value, for changing in place: every chunk is made the map's own.
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        Arc::make_mut(&mut self.chunks)
            .values_mut()
            .flat_map(|c| Arc::make_mut(c).values_mut())
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
        let mut chunks: BTreeMap<u32, BTreeMap<u32, V>> = BTreeMap::new();
        let len = m.len();
        for (k, v) in m {
            chunks.entry(k >> SHIFT).or_default().insert(k, v);
        }
        PMap {
            chunks: Arc::new(chunks.into_iter().map(|(c, m)| (c, Arc::new(m))).collect()),
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

    pub fn insert(&mut self, k: u32, v: V) -> Option<V> {
        // (The old entry's contributions go first: the new one may share keys with them.)
        if let Some(o) = self.map.get(&k) {
            let o = o.clone();
            self.ix.remove(k, &o);
        }
        self.ix.add(k, &v);
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

/// A multiset of oids (a count per member), as a persistent map.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct Counts(PMap<u32>);

impl Counts {
    pub fn add(&mut self, k: u32) {
        let n = self.0.get(&k).copied().unwrap_or(0);
        self.0.insert(k, n + 1);
    }

    pub fn remove(&mut self, k: u32) {
        match self.0.get(&k).copied() {
            Some(n) if n > 1 => {
                self.0.insert(k, n - 1);
            }
            Some(_) => {
                self.0.remove(&k);
            }
            None => debug_assert!(false, "{k} isn't counted"),
        }
    }

    pub fn contains(&self, k: u32) -> bool {
        self.0.contains_key(&k)
    }

    /// The members, in order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.0.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// An unordered map shared structurally by a catalog's copies, as `PMap` (the name index:
/// a change copies one bucket of `BUCKETS`, not every name of the catalog).
pub struct PHash<K, V> {
    buckets: Arc<Vec<Arc<crate::exec::fasthash::HashMap<K, V>>>>,
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

impl<K: std::hash::Hash + Eq + Clone, V: Clone> PHash<K, V> {
    fn bucket(k: &K) -> usize {
        use std::hash::BuildHasher;
        static STATE: std::sync::LazyLock<crate::exec::fasthash::FastState> =
            std::sync::LazyLock::new(Default::default);
        (STATE.hash_one(k) >> 32) as usize % BUCKETS
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, k: &K) -> Option<&V> {
        self.buckets.get(Self::bucket(k))?.get(k)
    }

    pub fn contains_key(&self, k: &K) -> bool {
        self.get(k).is_some()
    }

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let b = Self::bucket(&k);
        let buckets = Arc::make_mut(&mut self.buckets);
        if buckets.is_empty() {
            buckets.resize_with(BUCKETS, Default::default);
        }
        let old = Arc::make_mut(&mut buckets[b]).insert(k, v);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        let b = Self::bucket(k);
        if !self.buckets.get(b).is_some_and(|m| m.contains_key(k)) {
            return None;
        }
        let old = Arc::make_mut(&mut Arc::make_mut(&mut self.buckets)[b]).remove(k);
        if old.is_some() {
            self.len -= 1;
        }
        old
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.buckets.iter().flat_map(|b| b.iter())
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for PHash<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.buckets.iter().flat_map(|b| b.iter())).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
