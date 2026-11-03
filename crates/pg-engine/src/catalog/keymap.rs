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
    /// Where the key's length and bytes start in the arena (`part` counts 4 GB parts of it,
    /// below `PRESENT`: arenas past one are rare enough to spend two bytes on rather than
    /// widen every row).
    at: u32,
    part: u16,
    off: u16,
    blk: u32,
}

const PART: usize = 1 << 32;
/// `Row::part`'s bit for a row whose key is present (in the hash table).
const PRESENT: u16 = 1 << 15;

impl Row {
    fn new(pos: usize, tid: Tid) -> Row {
        Row { at: (pos % PART) as u32, part: (pos / PART) as u16 | PRESENT, off: tid.1, blk: tid.0 }
    }

    fn present(&self) -> bool {
        self.part & PRESENT != 0
    }
}

#[derive(Default)]
pub struct KeyMap {
    /// The rows of the keys present, by their keys' hashes.
    table: hashbrown::HashTable<u32>,
    rows: Vec<Row>,
    arena: Vec<u8>,
    state: FastState,
    /// The row last found or added: lookups in key order (a scan's rows, rows loaded in key
    /// order) find the next row's key right after it, without the hash table's random reads.
    cursor: std::sync::atomic::AtomicU32,
    /// The greatest key ever added: a key past it is new, added without a probe.
    max_key: Vec<u8>,
    /// Rows `..asc` have ascending keys (rows added in key order: a load, a model laid out
    /// from rows in key order): a lookup past the cursor's key searches forward from it.
    asc: u32,
}

impl std::fmt::Debug for KeyMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyMap({} keys, {} rows)", self.table.len(), self.rows.len())
    }
}

fn key_at<'a>(arena: &'a [u8], r: &Row) -> &'a [u8] {
    let mut b = &arena[(r.part & !PRESENT) as usize * PART + r.at as usize..];
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
        use std::sync::atomic::Ordering::Relaxed;
        let (rows, arena) = (&self.rows, &self.arena);
        let cur = self.cursor.load(Relaxed);
        let c = cur.wrapping_add(1);
        if let Some(r) = rows.get(c as usize)
            && r.present()
            && key_at(arena, r) == key
        {
            self.cursor.store(c, Relaxed);
            return Some(c);
        }
        if let Some(r) = self.forward(cur, key) {
            self.cursor.store(r, Relaxed);
            return Some(r);
        }
        let found = self.table.find(self.hash(key), |&r| key_at(arena, &rows[r as usize]) == key).copied();
        if let Some(r) = found {
            self.cursor.store(r, Relaxed);
        }
        found
    }

    /// The row under `key` found by searching forward from row `cur` within the ascending
    /// rows (`asc`): galloping, then halving. None: not there (or not present: the hash table
    /// has the key's row, if any).
    fn forward(&self, cur: u32, key: &[u8]) -> Option<u32> {
        let (rows, arena) = (&self.rows, &self.arena);
        let end = self.asc as usize;
        let cur = cur as usize;
        if cur >= end || key_at(arena, &rows[cur]) >= key {
            return None;
        }
        // (Rows cur+1..; the first whose key is at least `key`.)
        let (mut lo, mut step) = (cur + 1, 1);
        let mut hi = lo;
        while hi < end && key_at(arena, &rows[hi]) < key {
            lo = hi + 1;
            hi = (hi + step).min(end);
            step *= 2;
        }
        let hi = hi.min(end);
        let at = lo + rows[lo..hi].partition_point(|r| key_at(arena, r) < key);
        let r = rows.get(at).filter(|_| at < end)?;
        (r.present() && key_at(arena, r) == key).then_some(at as u32)
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
        // (A key past every key added so far is new: no probe for it, only its slot.)
        if self.rows.is_empty() || key > self.max_key.as_slice() {
            let row = self.push_row(key, tid);
            let (rows, arena, state) = (&self.rows, &self.arena, &self.state);
            self.table.insert_unique(h, row, |&r| state.hash_one(key_at(arena, &rows[r as usize])));
            return row;
        }
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
                self.cursor.store(row, std::sync::atomic::Ordering::Relaxed);
                row
            }
            hashbrown::hash_table::Entry::Vacant(e) => {
                let row = u32::try_from(self.rows.len()).ok().filter(|&n| n != NO_ROW).expect("rows");
                e.insert(row);
                self.push_row(key, tid)
            }
        }
    }

    /// A new row for `key` (present), its key's bytes in the arena.
    fn push_row(&mut self, key: &[u8], tid: Tid) -> u32 {
        let row = u32::try_from(self.rows.len()).ok().filter(|&n| n != NO_ROW).expect("rows");
        self.rows.push(Row::new(self.arena.len(), tid));
        put_varint(&mut self.arena, key.len() as u64);
        self.arena.extend_from_slice(key);
        if key > self.max_key.as_slice() || row == 0 {
            if self.asc == row {
                self.asc = row + 1;
            }
            self.max_key.clear();
            self.max_key.extend_from_slice(key);
        }
        self.cursor.store(row, std::sync::atomic::Ordering::Relaxed);
        row
    }

    /// No row is under `key` any more (its row keeps the key's bytes). Returns its row.
    pub fn remove(&mut self, key: &[u8]) -> Option<u32> {
        let h = self.hash(key);
        let (rows, arena) = (&self.rows, &self.arena);
        let (row, _) = self.table.find_entry(h, |&r| key_at(arena, &rows[r as usize]) == key).ok()?.remove();
        self.rows[row as usize].part &= !PRESENT;
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
                self.rows[row as usize].part &= !PRESENT;
                true
            }
            Err(_) => false,
        }
    }

    /// Keep the keys whose current versions' TIDs `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(Tid) -> bool) {
        let rows = &mut self.rows;
        self.table.retain(|&mut r| {
            let row = &mut rows[r as usize];
            let kept = keep((row.blk, row.off));
            if !kept {
                row.part &= !PRESENT;
            }
            kept
        });
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
        let mut asc = 0;
        for (i, r) in self.rows.iter().enumerate() {
            if map[i] == NO_ROW && !keep.get(i).copied().unwrap_or(false) {
                continue;
            }
            let k = key_at(&self.arena, r);
            let pos = arena.len();
            put_varint(&mut arena, k.len() as u64);
            arena.extend_from_slice(k);
            map[i] = rows.len() as u32;
            // (Rows kept from the ascending ones still ascend.)
            if i < self.asc as usize {
                asc = rows.len() as u32 + 1;
            }
            rows.push(Row { at: (pos % PART) as u32, part: (pos / PART) as u16 | (r.part & PRESENT), ..*r });
        }
        for r in self.table.iter_mut() {
            *r = map[*r as usize];
        }
        self.rows = rows;
        self.arena = arena;
        self.asc = asc;
        self.cursor.store(0, std::sync::atomic::Ordering::Relaxed);
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

    /// Keys loaded in key order are found by their forward searches as the hash table finds
    /// them: lookups in order with gaps, removed keys (and keys added again under new rows),
    /// out-of-order lookups, and after a compaction.
    #[test]
    fn ascending_rows_found_forward() {
        let key = |i: u32| format!("k{i:08}").into_bytes();
        let mut m = KeyMap::default();
        let mut r = std::collections::HashMap::new();
        for i in 0..50_000u32 {
            m.insert(&key(i), (i, 1));
            r.insert(key(i), (i, 1));
        }
        assert_eq!(m.asc, 50_000);
        for i in (0..50_000).step_by(10) {
            assert_eq!(m.remove(&key(i)).is_some(), r.remove(&key(i)).is_some());
        }
        for i in (0..50_000).step_by(30) {
            m.insert(&key(i), (i, 2));
            r.insert(key(i), (i, 2));
        }
        let check = |m: &KeyMap, r: &std::collections::HashMap<Vec<u8>, Tid>| {
            for step in [1, 3, 7, 10, 997] {
                for i in (0..50_010u32).step_by(step) {
                    assert_eq!(m.get(&key(i)), r.get(&key(i)).copied(), "key {i} step {step}");
                }
            }
            for i in (0..50_000u32).rev().step_by(13) {
                assert_eq!(m.get(&key(i)), r.get(&key(i)).copied());
            }
        };
        check(&m, &r);
        let keep = vec![false; m.rows()];
        let _ = m.compact(&keep);
        check(&m, &r);
    }
}
