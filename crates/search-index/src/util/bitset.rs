//! Doc id sets: a fixed-size bit set and a builder that picks sparse or dense storage.
//!
//! Multi-term filters, point ranges and cached filters materialize their matches per segment.
//! Few matches are kept as a sorted `Vec<u32>` (4 bytes per doc, cheap to build and iterate);
//! once more than `max_doc / 128` docs match, a bit set (`max_doc / 8` bytes) is smaller and
//! faster, so [`DocIdSetBuilder`] upgrades — Lucene's `DocIdSetBuilder` rule.

use search_codec::NO_MORE_DOCS;

/// A fixed-size bit set over doc ids `0..len`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixedBitSet {
    words: Vec<u64>,
    len: u32,
}

impl FixedBitSet {
    pub fn new(len: u32) -> FixedBitSet {
        FixedBitSet { words: vec![0; (len as usize).div_ceil(64)], len }
    }

    /// Every bit set.
    pub fn full(len: u32) -> FixedBitSet {
        let mut b = FixedBitSet { words: vec![u64::MAX; (len as usize).div_ceil(64)], len };
        b.clear_tail();
        b
    }

    pub fn from_words(words: Vec<u64>, len: u32) -> FixedBitSet {
        let mut words = words;
        words.resize((len as usize).div_ceil(64), 0);
        let mut b = FixedBitSet { words, len };
        b.clear_tail();
        b
    }

    fn clear_tail(&mut self) {
        let r = self.len % 64;
        if r != 0
            && let Some(w) = self.words.last_mut()
        {
            *w &= (1u64 << r) - 1;
        }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    #[inline]
    pub fn words_mut(&mut self) -> &mut [u64] {
        &mut self.words
    }

    #[inline]
    pub fn get(&self, i: u32) -> bool {
        let i = i as usize;
        self.words.get(i >> 6).is_some_and(|w| w >> (i & 63) & 1 != 0)
    }

    #[inline]
    pub fn set(&mut self, i: u32) {
        let i = i as usize;
        self.words[i >> 6] |= 1 << (i & 63);
    }

    /// Sets the bit, returning whether it was clear.
    #[inline]
    pub fn get_and_set(&mut self, i: u32) -> bool {
        let i = i as usize;
        let w = &mut self.words[i >> 6];
        let m = 1u64 << (i & 63);
        let was = *w & m != 0;
        *w |= m;
        !was
    }

    #[inline]
    pub fn clear(&mut self, i: u32) {
        let i = i as usize;
        self.words[i >> 6] &= !(1 << (i & 63));
    }

    /// Sets bits `start..end`.
    pub fn set_range(&mut self, start: u32, end: u32) {
        if start >= end {
            return;
        }
        let (s, e) = (start as usize, end as usize);
        let (sw, ew) = (s >> 6, (e - 1) >> 6);
        let smask = u64::MAX << (s & 63);
        let emask = u64::MAX >> (63 - ((e - 1) & 63));
        if sw == ew {
            self.words[sw] |= smask & emask;
            return;
        }
        self.words[sw] |= smask;
        for w in &mut self.words[sw + 1..ew] {
            *w = u64::MAX;
        }
        self.words[ew] |= emask;
    }

    /// Number of set bits.
    pub fn cardinality(&self) -> u32 {
        self.words.iter().map(|w| w.count_ones()).sum()
    }

    /// First set bit at or after `from`, or [`NO_MORE_DOCS`].
    #[inline]
    pub fn next_set_bit(&self, from: u32) -> u32 {
        if from >= self.len {
            return NO_MORE_DOCS;
        }
        let mut wi = (from >> 6) as usize;
        let mut w = self.words[wi] >> (from & 63);
        if w != 0 {
            return from + w.trailing_zeros();
        }
        loop {
            wi += 1;
            if wi >= self.words.len() {
                return NO_MORE_DOCS;
            }
            w = self.words[wi];
            if w != 0 {
                return (wi as u32) * 64 + w.trailing_zeros();
            }
        }
    }

    /// Last set bit at or before `from`, or `None`.
    pub fn prev_set_bit(&self, from: u32) -> Option<u32> {
        if self.len == 0 {
            return None;
        }
        let from = from.min(self.len - 1);
        let mut wi = (from >> 6) as isize;
        let sh = 63 - (from & 63);
        let mut w = (self.words[wi as usize] << sh) >> sh;
        loop {
            if w != 0 {
                return Some(wi as u32 * 64 + 63 - w.leading_zeros());
            }
            wi -= 1;
            if wi < 0 {
                return None;
            }
            w = self.words[wi as usize];
        }
    }

    pub fn or(&mut self, o: &FixedBitSet) {
        for (a, b) in self.words.iter_mut().zip(&o.words) {
            *a |= b;
        }
    }

    pub fn and(&mut self, o: &FixedBitSet) {
        for (i, a) in self.words.iter_mut().enumerate() {
            *a &= o.words.get(i).copied().unwrap_or(0);
        }
    }

    pub fn and_not(&mut self, o: &FixedBitSet) {
        for (a, b) in self.words.iter_mut().zip(&o.words) {
            *a &= !b;
        }
    }

    /// Iterates set bits in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let t = w.trailing_zeros();
                w &= w - 1;
                Some(wi as u32 * 64 + t)
            })
        })
    }

    /// Heap bytes held.
    pub fn ram_bytes(&self) -> usize {
        self.words.capacity() * 8 + std::mem::size_of::<Self>()
    }
}

/// A materialized set of docs of one segment.
#[derive(Clone, Debug)]
pub enum DocIdSet {
    Empty,
    /// Every doc `0..max_doc`.
    All(u32),
    /// Sorted, unique docs.
    Sparse(Vec<u32>),
    Dense(FixedBitSet),
}

impl DocIdSet {
    /// Upper bound of the number of docs (exact for every variant).
    pub fn cost(&self) -> u64 {
        match self {
            DocIdSet::Empty => 0,
            DocIdSet::All(n) => *n as u64,
            DocIdSet::Sparse(v) => v.len() as u64,
            DocIdSet::Dense(b) => b.cardinality() as u64,
        }
    }

    pub fn contains(&self, doc: u32) -> bool {
        match self {
            DocIdSet::Empty => false,
            DocIdSet::All(n) => doc < *n,
            DocIdSet::Sparse(v) => v.binary_search(&doc).is_ok(),
            DocIdSet::Dense(b) => b.get(doc),
        }
    }

    /// An iterator positioned before the first doc.
    pub fn iter(&self) -> DocIdSetIter<'_> {
        DocIdSetIter { set: self, doc: u32::MAX, idx: 0 }
    }

    pub fn ram_bytes(&self) -> usize {
        match self {
            DocIdSet::Sparse(v) => v.capacity() * 4 + 32,
            DocIdSet::Dense(b) => b.ram_bytes(),
            _ => 32,
        }
    }

    /// Converts to a bit set of `max_doc` bits.
    pub fn to_bitset(&self, max_doc: u32) -> FixedBitSet {
        match self {
            DocIdSet::Empty => FixedBitSet::new(max_doc),
            DocIdSet::All(n) => FixedBitSet::full((*n).min(max_doc)),
            DocIdSet::Sparse(v) => {
                let mut b = FixedBitSet::new(max_doc);
                for &d in v {
                    if d < max_doc {
                        b.set(d);
                    }
                }
                b
            }
            DocIdSet::Dense(b) => b.clone(),
        }
    }
}

/// Iterates a [`DocIdSet`] with `next`/`advance` (Lucene `DocIdSetIterator` semantics: `doc()` is
/// `u32::MAX` before the first call and [`NO_MORE_DOCS`] at the end).
pub struct DocIdSetIter<'a> {
    set: &'a DocIdSet,
    doc: u32,
    idx: usize,
}

impl DocIdSetIter<'_> {
    #[inline]
    pub fn doc(&self) -> u32 {
        self.doc
    }

    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.advance(t)
    }

    pub fn advance(&mut self, target: u32) -> u32 {
        self.doc = match self.set {
            DocIdSet::Empty => NO_MORE_DOCS,
            DocIdSet::All(n) => {
                if target < *n {
                    target
                } else {
                    NO_MORE_DOCS
                }
            }
            DocIdSet::Sparse(v) => {
                let rest = &v[self.idx..];
                let k = if rest.len() > 8 && rest[7] < target {
                    rest.partition_point(|&d| d < target)
                } else {
                    rest.iter().position(|&d| d >= target).unwrap_or(rest.len())
                };
                self.idx += k;
                v.get(self.idx).copied().unwrap_or(NO_MORE_DOCS)
            }
            DocIdSet::Dense(b) => b.next_set_bit(target),
        };
        self.doc
    }
}

/// Collects docs (in any order, with repeats) and builds the cheaper [`DocIdSet`].
pub struct DocIdSetBuilder {
    max_doc: u32,
    sparse: Vec<u32>,
    dense: Option<FixedBitSet>,
    threshold: usize,
    /// Whether `sparse` is still sorted and unique (point leaves often arrive in order).
    ordered: bool,
}

impl DocIdSetBuilder {
    pub fn new(max_doc: u32) -> DocIdSetBuilder {
        DocIdSetBuilder {
            max_doc,
            sparse: Vec::new(),
            dense: None,
            threshold: (max_doc as usize >> 7).max(64),
            ordered: true,
        }
    }

    /// Hint that about `n` more docs follow (upgrades early when they cannot fit sparse).
    pub fn grow(&mut self, n: usize) {
        if self.dense.is_none() && self.sparse.len() + n > self.threshold {
            self.upgrade();
        } else if self.dense.is_none() {
            self.sparse.reserve(n);
        }
    }

    fn upgrade(&mut self) {
        let mut b = FixedBitSet::new(self.max_doc);
        for &d in &self.sparse {
            b.set(d);
        }
        self.sparse = Vec::new();
        self.dense = Some(b);
    }

    #[inline]
    pub fn add(&mut self, doc: u32) {
        if let Some(b) = &mut self.dense {
            b.set(doc);
            return;
        }
        if let Some(&last) = self.sparse.last()
            && last >= doc
        {
            if last == doc {
                return;
            }
            self.ordered = false;
        }
        self.sparse.push(doc);
        if self.sparse.len() > self.threshold {
            self.upgrade();
        }
    }

    /// Adds docs `start..end`.
    pub fn add_range(&mut self, start: u32, end: u32) {
        if end <= start {
            return;
        }
        if self.dense.is_none() && self.sparse.len() + (end - start) as usize > self.threshold {
            self.upgrade();
        }
        match &mut self.dense {
            Some(b) => b.set_range(start, end),
            None => {
                for d in start..end {
                    self.add(d);
                }
            }
        }
    }

    /// Mutable bit set (upgrading first), for bulk producers such as `PostingsCursor::into_bitset`.
    pub fn bitset(&mut self) -> &mut FixedBitSet {
        if self.dense.is_none() {
            self.upgrade();
        }
        self.dense.as_mut().unwrap()
    }

    pub fn build(mut self) -> DocIdSet {
        if let Some(b) = self.dense {
            if b.words().iter().all(|&w| w == 0) {
                return DocIdSet::Empty;
            }
            return DocIdSet::Dense(b);
        }
        if self.sparse.is_empty() {
            return DocIdSet::Empty;
        }
        if !self.ordered {
            self.sparse.sort_unstable();
            self.sparse.dedup();
        }
        DocIdSet::Sparse(self.sparse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_ops() {
        let mut b = FixedBitSet::new(200);
        b.set(3);
        b.set(64);
        b.set(199);
        b.set_range(100, 131);
        assert_eq!(b.cardinality(), 3 + 31);
        assert_eq!(b.next_set_bit(0), 3);
        assert_eq!(b.next_set_bit(4), 64);
        assert_eq!(b.next_set_bit(65), 100);
        assert_eq!(b.next_set_bit(131), 199);
        assert_eq!(b.next_set_bit(200), NO_MORE_DOCS);
        assert_eq!(b.prev_set_bit(99), Some(64));
        assert_eq!(b.prev_set_bit(2), None);
        assert_eq!(b.prev_set_bit(1000), Some(199));
        let all: Vec<u32> = b.iter().collect();
        assert_eq!(all.len(), 34);
        let f = FixedBitSet::full(70);
        assert_eq!(f.cardinality(), 70);
    }

    #[test]
    fn builder_upgrades_and_iterates() {
        for n in [0u32, 1, 10, 1000, 100_000] {
            let mut bld = DocIdSetBuilder::new(n.max(1) * 3);
            let mut expect = Vec::new();
            for i in (0..n).rev() {
                bld.add(i * 3);
                expect.push(i * 3);
            }
            expect.sort();
            let s = bld.build();
            let mut it = s.iter();
            let mut got = Vec::new();
            loop {
                let d = it.next();
                if d == NO_MORE_DOCS {
                    break;
                }
                got.push(d);
            }
            assert_eq!(got, expect);
            assert_eq!(s.cost(), n as u64);
            if n > 2 {
                let mut it = s.iter();
                assert_eq!(it.advance(4), 6);
            }
        }
    }
}
