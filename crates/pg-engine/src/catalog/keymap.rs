//! A heap model's rows (`heapsim::Heap`): each row key gets a row number, under which it keeps
//! the key's bytes (in one arena: a varint length, then the bytes) and the TID of the row's
//! current version; a hash table finds a key's row. Every tuple version carries its row's
//! number, so a TID gives its row's key (`Heap::key_at`), an older version's too. A
//! `HashMap<Vec<u8>, Tid>` took an allocation per key and about 70 bytes per row; this takes
//! the key's bytes plus about 20, and nothing to free per row.
//!
//! A key removed (its row deleted, or moved to another key) keeps its row, and its bytes,
//! for the versions still naming it, until the heap compacts the rows (`Heap::compact_rows`).

use super::heapsim::{Tid, get_varint, put_varint};
use crate::exec::fasthash::FastState;
use std::hash::BuildHasher;

/// A row of no key (a version written without one).
pub const NO_ROW: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Row {
    /// Where the key's length and bytes start in the arena (`part` counts 4 GB parts of it:
    /// arenas past one are rare enough to spend two bytes on rather than widen every row).
    at: u32,
    part: u16,
    off: u16,
    blk: u32,
}

const PART: usize = 1 << 32;

#[derive(Default)]
pub struct KeyMap {
    /// The rows of the keys present, by their keys' hashes.
    table: hashbrown::HashTable<u32>,
    rows: Vec<Row>,
    arena: Vec<u8>,
    state: FastState,
}

impl std::fmt::Debug for KeyMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyMap({} keys, {} rows)", self.table.len(), self.rows.len())
    }
}

fn key_at<'a>(arena: &'a [u8], r: &Row) -> &'a [u8] {
    let mut b = &arena[r.part as usize * PART + r.at as usize..];
    let n = get_varint(&mut b).unwrap_or(0) as usize;
    &b[..n]
}

impl KeyMap {
    fn hash(&self, key: &[u8]) -> u64 {
        self.state.hash_one(key)
    }

    /// Keys present.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Rows numbered, keys present or not.
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// Room for `n` more keys of about `bytes` bytes in all.
    pub fn reserve(&mut self, n: usize, bytes: usize) {
        let (rows, arena, state) = (&self.rows, &self.arena, &self.state);
        self.table.reserve(n, |&r| state.hash_one(key_at(arena, &rows[r as usize])));
        self.rows.reserve(n);
        self.arena.reserve(bytes + n);
    }

    /// The row under `key`.
    pub fn find(&self, key: &[u8]) -> Option<u32> {
        let (rows, arena) = (&self.rows, &self.arena);
        self.table.find(self.hash(key), |&r| key_at(arena, &rows[r as usize]) == key).copied()
    }

    /// The TID of the current version of the row under `key`.
    pub fn get(&self, key: &[u8]) -> Option<Tid> {
        self.find(key).map(|r| self.tid(r))
    }

    /// Row `row`'s current version.
    pub fn tid(&self, row: u32) -> Tid {
        let r = &self.rows[row as usize];
        (r.blk, r.off)
    }

    /// Row `row`'s key (present or removed).
    pub fn key(&self, row: u32) -> Option<&[u8]> {
        self.rows.get(row as usize).map(|r| key_at(&self.arena, r))
    }

    /// The row under `key` now has its current version at `tid` (a new row if no row is
    /// under it). Returns the row.
    pub fn insert(&mut self, key: &[u8], tid: Tid) -> u32 {
        let h = self.hash(key);
        let (rows, arena, state) = (&self.rows, &self.arena, &self.state);
        let entry = self.table.entry(
            h,
            |&r| key_at(arena, &rows[r as usize]) == key,
            |&r| state.hash_one(key_at(arena, &rows[r as usize])),
        );
        match entry {
            hashbrown::hash_table::Entry::Occupied(e) => {
                let row = *e.get();
                let r = &mut self.rows[row as usize];
                (r.blk, r.off) = tid;
                row
            }
            hashbrown::hash_table::Entry::Vacant(e) => {
                let row = u32::try_from(self.rows.len()).ok().filter(|&n| n != NO_ROW).expect("rows");
                e.insert(row);
                let pos = self.arena.len();
                self.rows.push(Row {
                    at: (pos % PART) as u32,
                    part: (pos / PART) as u16,
                    off: tid.1,
                    blk: tid.0,
                });
                put_varint(&mut self.arena, key.len() as u64);
                self.arena.extend_from_slice(key);
                row
            }
        }
    }

    /// No row is under `key` any more (its row keeps the key's bytes). Returns its row.
    pub fn remove(&mut self, key: &[u8]) -> Option<u32> {
        let h = self.hash(key);
        let (rows, arena) = (&self.rows, &self.arena);
        let (row, _) = self.table.find_entry(h, |&r| key_at(arena, &rows[r as usize]) == key).ok()?.remove();
        Some(row)
    }

    /// Row `row` is no longer under its key if it is the one there with its current version
    /// at `tid` (a deleted row's last version freed). Returns whether it was.
    pub fn remove_row_at(&mut self, row: u32, tid: Tid) -> bool {
        let Some(r) = self.rows.get(row as usize) else { return false };
        if (r.blk, r.off) != tid {
            return false;
        }
        let h = self.hash(key_at(&self.arena, r));
        match self.table.find_entry(h, |&x| x == row) {
            Ok(e) => {
                e.remove();
                true
            }
            Err(_) => false,
        }
    }

    /// Keep the keys whose current versions' TIDs `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(Tid) -> bool) {
        let rows = &self.rows;
        self.table.retain(|&mut r| keep((rows[r as usize].blk, rows[r as usize].off)));
    }

    /// Every key present, its row and its current version's TID, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u32, Tid)> {
        self.table.iter().map(|&r| {
            let row = &self.rows[r as usize];
            (key_at(&self.arena, row), r, (row.blk, row.off))
        })
    }

    /// Whether removed keys' rows are so many that compacting them is worth a pass over the
    /// heap's versions.
    pub fn wasteful(&self) -> bool {
        let dead = self.rows.len() - self.table.len();
        dead >= 1 << 16 && dead >= self.table.len()
    }

    /// Keep only the rows `keep` marks (by row number; the keys present are always kept),
    /// renumbered in order. Returns each old row's new number (`NO_ROW`: dropped).
    pub fn compact(&mut self, keep: &[bool]) -> Vec<u32> {
        let mut map = vec![NO_ROW; self.rows.len()];
        for &r in self.table.iter() {
            map[r as usize] = 0;
        }
        let mut rows = Vec::with_capacity(self.table.len());
        let mut arena = Vec::new();
        for (i, r) in self.rows.iter().enumerate() {
            if map[i] == NO_ROW && !keep.get(i).copied().unwrap_or(false) {
                continue;
            }
            let k = key_at(&self.arena, r);
            let pos = arena.len();
            put_varint(&mut arena, k.len() as u64);
            arena.extend_from_slice(k);
            map[i] = rows.len() as u32;
            rows.push(Row { at: (pos % PART) as u32, part: (pos / PART) as u16, ..*r });
        }
        for r in self.table.iter_mut() {
            *r = map[*r as usize];
        }
        self.rows = rows;
        self.arena = arena;
        map
    }

    /// Bytes held (the table's slots and control bytes, the rows, the arena).
    pub fn bytes(&self) -> usize {
        self.table.capacity() * (size_of::<u32>() + 1) + self.rows.capacity() * size_of::<Row>() + self.arena.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The map answers as a `HashMap` does through inserts, overwrites, removals, retains and
    /// compactions; a removed key's row keeps its key until compacted away.
    #[test]
    fn matches_a_hash_map() {
        let mut m = KeyMap::default();
        let mut r = std::collections::HashMap::new();
        let mut x = 7u64;
        for i in 0..200_000u32 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = format!("key{}", (x >> 40) % 50_000).into_bytes();
            let tid = (i, (x % 300) as u16);
            match (x >> 20) % 4 {
                0 => {
                    let row = m.remove(&k);
                    assert_eq!(row.is_some(), r.remove(&k).is_some());
                    if let Some(row) = row {
                        assert_eq!(m.key(row), Some(&k[..]));
                    }
                }
                1 if i % 1000 == 0 => {
                    let cut = i / 2;
                    m.retain(|t| t.0 > cut);
                    r.retain(|_, t: &mut Tid| t.0 > cut);
                    if m.wasteful() {
                        let map = m.compact(&[]);
                        assert!(map.iter().filter(|&&n| n != NO_ROW).count() == m.len());
                    }
                }
                _ => {
                    let row = m.insert(&k, tid);
                    assert_eq!(m.key(row), Some(&k[..]));
                    r.insert(k.clone(), tid);
                }
            }
            assert_eq!(m.get(&k), r.get(&k).copied());
        }
        assert_eq!(m.len(), r.len());
        let mut a: Vec<(Vec<u8>, Tid)> = m.iter().map(|(k, _, t)| (k.to_vec(), t)).collect();
        let mut b: Vec<(Vec<u8>, Tid)> = r.into_iter().collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }
}
