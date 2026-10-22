//! Term → id hashing for the indexing buffer (Lucene's `BytesRefHash`).
//!
//! Terms are appended once to a byte arena and identified by a dense `u32` id in insertion
//! order; the table stores only ids (4 bytes per slot) and a side array keeps each id's hash so
//! growing never re-hashes term bytes. Lookups compare the stored hash first, so a probe touches
//! term bytes only for a likely match.

use std::hash::BuildHasher;

use hashbrown::HashTable;

/// Fixed-seed hasher: buffers are private to a shard, so no seed randomness is needed and a
/// fixed seed keeps flushes deterministic.
#[inline]
pub fn hash_bytes(b: &[u8]) -> u64 {
    foldhash::fast::FixedState::with_seed(0x9e37_79b9_7f4a_7c15).hash_one(b)
}

/// Unique byte strings with dense ids.
#[derive(Default)]
pub struct TermHash {
    table: HashTable<u32>,
    bytes: Vec<u8>,
    /// `starts[id]..starts[id + 1]` are the bytes of `id`.
    starts: Vec<u32>,
    hashes: Vec<u64>,
}

impl TermHash {
    pub fn new() -> TermHash {
        TermHash { table: HashTable::new(), bytes: Vec::new(), starts: vec![0], hashes: Vec::new() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    #[inline]
    pub fn get(&self, id: u32) -> &[u8] {
        let i = id as usize;
        &self.bytes[self.starts[i] as usize..self.starts[i + 1] as usize]
    }

    /// The id of `term`, adding it if new: `(id, is_new)`.
    #[inline]
    pub fn add(&mut self, term: &[u8]) -> (u32, bool) {
        let h = hash_bytes(term);
        let (bytes, starts, hashes) = (&self.bytes, &self.starts, &self.hashes);
        let eq = |&id: &u32| {
            let i = id as usize;
            hashes[i] == h && &bytes[starts[i] as usize..starts[i + 1] as usize] == term
        };
        if let Some(&id) = self.table.find(h, eq) {
            return (id, false);
        }
        let id = self.hashes.len() as u32;
        self.bytes.extend_from_slice(term);
        self.starts.push(self.bytes.len() as u32);
        self.hashes.push(h);
        let hashes = &self.hashes;
        self.table.insert_unique(h, id, |&i| hashes[i as usize]);
        (id, true)
    }

    /// The id of `term`, if present.
    pub fn find(&self, term: &[u8]) -> Option<u32> {
        let h = hash_bytes(term);
        self.table
            .find(h, |&id| self.hashes[id as usize] == h && self.get(id) == term)
            .copied()
    }

    /// Ids sorted by term bytes.
    pub fn sorted_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..self.len() as u32).collect();
        ids.sort_unstable_by(|&a, &b| self.get(a).cmp(self.get(b)));
        ids
    }

    /// Heap bytes held.
    pub fn ram_bytes(&self) -> usize {
        self.table.capacity() * 5 + self.bytes.capacity() + self.starts.capacity() * 4 + self.hashes.capacity() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_find_sort() {
        let mut h = TermHash::new();
        let words = ["b", "a", "", "abc", "a", "zz", "b"];
        let ids: Vec<u32> = words.iter().map(|w| h.add(w.as_bytes()).0).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 1, 4, 0]);
        assert_eq!(h.len(), 5);
        assert_eq!(h.find(b"abc"), Some(3));
        assert_eq!(h.find(b"q"), None);
        let sorted: Vec<&[u8]> = h.sorted_ids().iter().map(|&i| h.get(i)).collect();
        assert_eq!(sorted, vec![&b""[..], b"a", b"abc", b"b", b"zz"]);
        for i in 0..10_000u32 {
            h.add(&i.to_le_bytes());
        }
        for i in 0..10_000u32 {
            assert!(h.find(&i.to_le_bytes()).is_some());
        }
    }
}
