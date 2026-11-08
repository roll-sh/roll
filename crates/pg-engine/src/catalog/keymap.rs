//! A heap model's rows (`heapsim::Heap`): each row key gets a row number, under which it keeps
//! the key's bytes and the TID of the row's current version; a hash table finds a key's row.
//! Every tuple version carries its row's number, so a TID gives its row's key
//! (`Heap::key_at`), an older version's too. A `HashMap<Vec<u8>, Tid>` took an allocation per
//! key and about 70 bytes per row; this takes the key's bytes plus 6 to 21, and nothing to
//! free per row.
//!
//! Rows added in key order (a load, a model laid out from a record: most of a big table's)
//! are the ascending rows `..asc`: found by searching them, not hashed; their keys back to
//! back (all of one length: no length or place kept per row) and their TIDs in 6 bytes each.
//! Keys that step evenly (a serial's, a dense integer key's: one length, the same bytes but
//! for the last 8, which go up by the same amount each row) keep no bytes at all: row i's key
//! is computed (`AscKeys::Arith`), so the map lends a row's key as a `RowKey`, not a slice.
//! The rows after them keep each key's place in an arena (a varint length, then the bytes),
//! their TID, and a slot in the hash table.
//!
//! A key removed (its row deleted, or moved to another key) keeps its row, and its bytes,
//! for the versions still naming it, until the heap compacts the rows (`Heap::compact_rows`).

use super::heapsim::{Tid, get_varint, put_varint};
use crate::exec::fasthash::FastState;
use std::hash::BuildHasher;
use std::sync::atomic::Ordering::Relaxed;

/// How far past the cursor (in rows, about twice this) a lookup searches the ascending rows
/// before asking the hash table.
const FORWARD_MAX: usize = 64;

/// A row of no key (a version written without one).
pub const NO_ROW: u32 = u32::MAX;

/// A row past the ascending ones.
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
/// `Row::part`'s bit for a row whose key is present (in the hash table), and `AscTid`'s.
const PRESENT: u16 = 1 << 15;

impl Row {
    fn new(pos: usize, tid: Tid) -> Row {
        Row { at: (pos % PART) as u32, part: (pos / PART) as u16 | PRESENT, off: tid.1, blk: tid.0 }
    }

    fn present(&self) -> bool {
        self.part & PRESENT != 0
    }
}

/// An ascending row's current version: its block's halves and its offset, whose top bit
/// (`PRESENT`, past any line pointer's offset) says its key is present.
#[derive(Clone, Copy, Debug)]
struct AscTid([u16; 3]);

impl AscTid {
    fn new(tid: Tid) -> AscTid {
        debug_assert!(tid.1 < PRESENT);
        AscTid([tid.0 as u16, (tid.0 >> 16) as u16, tid.1 | PRESENT])
    }

    fn tid(self) -> Tid {
        (self.0[0] as u32 | (self.0[1] as u32) << 16, self.0[2] & !PRESENT)
    }

    fn present(self) -> bool {
        self.0[2] & PRESENT != 0
    }

    fn set(&mut self, tid: Tid) {
        debug_assert!(tid.1 < PRESENT);
        *self = AscTid([tid.0 as u16, (tid.0 >> 16) as u16, tid.1 | (self.0[2] & PRESENT)]);
    }
}

/// How the ascending rows' keys are laid out in `KeyMap::asc_bytes`.
#[derive(Debug)]
enum AscKeys {
    /// All this many bytes long, back to back (0 with no ascending row yet).
    Fixed(usize),
    /// Each where its entry says: a varint length, then the bytes (keys of other lengths).
    Var(Vec<u32>),
    /// None kept: row i's key is the first's with its last 8 bytes (big-endian) plus
    /// `i * step` (`step` 0 while there is one row). The first key of another shape lays
    /// them out as `Fixed` (`unarith`).
    Arith(Arith),
}

/// `AscKeys::Arith`'s first key (`len` bytes, at most `ARITH_MAX`) and step.
#[derive(Clone, Copy, Debug)]
struct Arith {
    first: [u8; ARITH_MAX],
    len: u8,
    step: u64,
}

/// The longest key `AscKeys::Arith` computes (a `RowKey` holds it inline).
const ARITH_MAX: usize = 22;

impl Arith {
    fn new(key: &[u8]) -> Option<Arith> {
        (8..=ARITH_MAX).contains(&key.len()).then(|| {
            let mut first = [0; ARITH_MAX];
            first[..key.len()].copy_from_slice(key);
            Arith { first, len: key.len() as u8, step: 0 }
        })
    }

    fn first(&self) -> &[u8] {
        &self.first[..self.len as usize]
    }

    /// The last 8 bytes of a key of this shape (the rest the first's), as a number.
    fn num(&self, key: &[u8]) -> Option<u64> {
        let n = self.len as usize;
        (key.len() == n && key[..n - 8] == self.first[..n - 8]).then(|| u64::from_be_bytes(key[n - 8..].try_into().unwrap()))
    }

    /// Row i's key.
    fn key(&self, i: usize) -> RowKey<'static> {
        let n = self.len as usize;
        let mut buf = self.first;
        let v = self.num(self.first()).unwrap_or(0).wrapping_add((i as u64).wrapping_mul(self.step));
        buf[n - 8..n].copy_from_slice(&v.to_be_bytes());
        RowKey::Built(buf, self.len)
    }

    /// The row among `asc` whose key is `key`.
    fn find(&self, asc: usize, key: &[u8]) -> Option<usize> {
        let (a, v) = (self.num(self.first())?, self.num(key)?);
        let d = v.checked_sub(a)?;
        if self.step == 0 {
            return (d == 0 && asc > 0).then_some(0);
        }
        let i = usize::try_from(d / self.step).ok()?;
        (d % self.step == 0 && i < asc).then_some(i)
    }

    /// Whether `key` is row `asc`'s (the next) key, setting the step at the second row.
    fn next(&mut self, asc: usize, key: &[u8]) -> bool {
        let (Some(a), Some(v)) = (self.num(self.first()), self.num(key)) else { return false };
        if asc == 1 && self.step == 0 {
            if v <= a {
                return false;
            }
            self.step = v - a;
            return true;
        }
        self.step as u128 * asc as u128 + a as u128 == v as u128
    }
}

/// A row's key: the map's bytes, or (an `AscKeys::Arith` row's) computed. Derefs to them.
#[derive(Clone, Copy)]
pub enum RowKey<'a> {
    Kept(&'a [u8]),
    Built([u8; ARITH_MAX], u8),
}

impl std::ops::Deref for RowKey<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            RowKey::Kept(k) => k,
            RowKey::Built(b, n) => &b[..*n as usize],
        }
    }
}

impl std::fmt::Debug for RowKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", &self[..])
    }
}

impl Default for AscKeys {
    fn default() -> AscKeys {
        AscKeys::Fixed(0)
    }
}

#[derive(Default)]
pub struct KeyMap {
    /// The rows of the keys present past the ascending ones (`asc`), by their keys' hashes:
    /// each slot the row and the top half of its key's hash (`slot`), so growing the table
    /// rehashes from the slots alone, and a probe compares a key only when its hash's half
    /// matches. The ascending rows are found by searching them (`asc_row`): a table loaded or
    /// laid out in key order (most big ones) has no hash table at all, 18 bytes a row less.
    table: hashbrown::HashTable<u64>,
    /// Rows `asc..`, by row number less `asc`.
    rows: Vec<Row>,
    arena: Vec<u8>,
    state: FastState,
    /// The row last found or added: lookups in key order (a scan's rows, rows loaded in key
    /// order) find the next row's key right after it, without the hash table's random reads.
    cursor: std::sync::atomic::AtomicU32,
    /// The greatest key ever added: a key past it is new, added without a probe.
    max_key: Vec<u8>,
    /// Rows `..asc` have ascending keys (rows added in key order: a load, a model laid out
    /// from rows in key order): a lookup past the cursor's key searches forward from it, any
    /// other halves them. None of them is in `table`.
    asc: u32,
    /// The ascending rows whose keys are present.
    asc_present: u32,
    /// The ascending rows' current versions and keys.
    asc_tids: Vec<AscTid>,
    asc_bytes: Vec<u8>,
    asc_keys: AscKeys,
}

impl std::fmt::Debug for KeyMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyMap({} keys, {} rows)", self.len(), self.rows())
    }
}

/// A table slot of row `row` whose key hashes to `h`.
fn slot(row: u32, h: u64) -> u64 {
    (h & !(u32::MAX as u64)) | row as u64
}

/// The table's hash of a slot or a key hash: from the top half alone (all a slot keeps).
fn table_hash(h: u64) -> u64 {
    (h >> 32).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Whether slot `s` is the one of a key hashing to `h` (its key still to compare).
fn same_half(s: u64, h: u64) -> bool {
    s >> 32 == h >> 32
}

fn key_at<'a>(arena: &'a [u8], r: &Row) -> &'a [u8] {
    let mut b = &arena[(r.part & !PRESENT) as usize * PART + r.at as usize..];
    let n = get_varint(&mut b).unwrap_or(0) as usize;
    &b[..n]
}

/// The key of hashed row `row` (past `asc`).
fn hashed_key<'a>(rows: &[Row], arena: &'a [u8], asc: u32, row: u32) -> &'a [u8] {
    key_at(arena, &rows[(row - asc) as usize])
}

impl KeyMap {
    fn hash(&self, key: &[u8]) -> u64 {
        self.state.hash_one(key)
    }

    /// Keys present.
    pub fn len(&self) -> usize {
        self.table.len() + self.asc_present as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rows numbered, keys present or not.
    pub fn rows(&self) -> usize {
        self.asc as usize + self.rows.len()
    }

    /// Ascending row `i`'s key.
    fn asc_key(&self, i: usize) -> RowKey<'_> {
        match &self.asc_keys {
            AscKeys::Fixed(n) => RowKey::Kept(&self.asc_bytes[i * n..(i + 1) * n]),
            AscKeys::Var(at) => {
                let mut b = &self.asc_bytes[at[i] as usize..];
                let n = get_varint(&mut b).unwrap_or(0) as usize;
                RowKey::Kept(&b[..n])
            }
            AscKeys::Arith(a) => a.key(i),
        }
    }

    /// Row `row`'s key (present or removed).
    fn key_of(&self, row: u32) -> RowKey<'_> {
        if row < self.asc { self.asc_key(row as usize) } else { RowKey::Kept(hashed_key(&self.rows, &self.arena, self.asc, row)) }
    }

    /// The ascending rows' keys laid out back to back (`Fixed`), from computed ones: at the
    /// first key that doesn't step as they do.
    fn unarith(&mut self) {
        let AscKeys::Arith(a) = self.asc_keys else { return };
        let mut bytes = Vec::with_capacity(self.asc as usize * a.len as usize);
        for i in 0..self.asc as usize {
            bytes.extend_from_slice(&a.key(i));
        }
        (self.asc_bytes, self.asc_keys) = (bytes, AscKeys::Fixed(a.len as usize));
    }

    fn present(&self, row: u32) -> bool {
        if row < self.asc { self.asc_tids[row as usize].present() } else { self.rows[(row - self.asc) as usize].present() }
    }

    /// Row `row`'s current version.
    pub fn tid(&self, row: u32) -> Tid {
        if row < self.asc {
            self.asc_tids[row as usize].tid()
        } else {
            let r = &self.rows[(row - self.asc) as usize];
            (r.blk, r.off)
        }
    }

    fn set_row_tid(&mut self, row: u32, tid: Tid) {
        if row < self.asc {
            self.asc_tids[row as usize].set(tid);
        } else {
            let r = &mut self.rows[(row - self.asc) as usize];
            (r.blk, r.off) = tid;
        }
    }

    /// Present row `row`'s key is no longer present (the hash table's slot aside).
    fn unpresent(&mut self, row: u32) {
        if row < self.asc {
            self.asc_tids[row as usize].0[2] &= !PRESENT;
            self.asc_present -= 1;
        } else {
            self.rows[(row - self.asc) as usize].part &= !PRESENT;
        }
    }

    /// The first ascending row in `lo..hi` whose key is at least `key`.
    fn asc_search(&self, mut lo: usize, mut hi: usize, key: &[u8]) -> usize {
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if *self.asc_key(mid) < *key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The ascending row whose key is `key`, present or not.
    fn asc_row(&self, key: &[u8]) -> Option<u32> {
        let n = self.asc as usize;
        if let AscKeys::Arith(a) = &self.asc_keys {
            return a.find(n, key).map(|i| i as u32);
        }
        if n == 0 || key > &*self.asc_key(n - 1) {
            return None;
        }
        let (lo, hi) = self.asc_guess(n, key).unwrap_or((0, n));
        let at = self.asc_search(lo, hi, key);
        (at < n && *self.asc_key(at) == *key).then_some(at as u32)
    }

    /// Where among `n` ascending keys of one length `key` (of that length, within them) is,
    /// by interpolation on the 8 bytes after the first and last keys' common prefix: a range
    /// `lo..hi` holding the first key at least `key`, a probe or two around the guess where
    /// keys spread evenly (a serial's, a load's), instead of halving all of them (a random
    /// lookup's cache misses at every step). None: no guess (keys of other lengths, too few).
    fn asc_guess(&self, n: usize, key: &[u8]) -> Option<(usize, usize)> {
        let AscKeys::Fixed(len) = self.asc_keys else { return None };
        if n < 64 || key.len() != len {
            return None;
        }
        let (first, last) = (self.asc_key(0), self.asc_key(n - 1));
        let (first, last) = (&*first, &*last);
        let p = first.iter().zip(last).take_while(|(a, b)| a == b).count();
        let num = |k: &[u8]| {
            let mut b = [0u8; 8];
            let s = &k[p.min(k.len())..];
            b[..s.len().min(8)].copy_from_slice(&s[..s.len().min(8)]);
            u64::from_be_bytes(b)
        };
        let (a, z, k) = (num(first), num(last), num(key));
        if key[..p] != first[..p] || k < a || z <= a {
            return None;
        }
        let guess = (((k - a) as u128 * (n - 1) as u128) / (z - a) as u128) as usize;
        // (Out from the guess, doubling, until the key is between the bounds.)
        let (mut lo, mut hi, mut step) = (guess, guess + 1, 1);
        while lo > 0 && *self.asc_key(lo) > *key {
            hi = lo + 1;
            lo = lo.saturating_sub(step);
            step *= 2;
        }
        while hi < n && *self.asc_key(hi - 1) < *key {
            lo = hi - 1;
            hi = (hi + step).min(n);
            step *= 2;
        }
        Some((lo, hi))
    }

    /// The row of present key `key` in the hash table.
    fn table_row(&self, key: &[u8]) -> Option<u32> {
        if self.table.is_empty() {
            return None;
        }
        let (rows, arena, asc) = (&self.rows, &self.arena, self.asc);
        let h = self.hash(key);
        self.table.find(table_hash(h), |&s| same_half(s, h) && hashed_key(rows, arena, asc, s as u32) == key).map(|&s| s as u32)
    }

    /// Room for `n` more keys of about `bytes` bytes in all (in the hash table only once rows
    /// stopped ascending: a load in key order needs none).
    pub fn reserve(&mut self, n: usize, bytes: usize) {
        if self.rows.is_empty() {
            self.asc_tids.reserve(n);
            // (Keys that step evenly take no bytes: from the second row on, the rows so far
            // say whether they do; before, the room goes once they do, `push_asc`.)
            if self.asc < 2 || !matches!(self.asc_keys, AscKeys::Arith(_)) {
                self.asc_bytes.reserve(bytes);
            }
            if let AscKeys::Var(at) = &mut self.asc_keys {
                at.reserve(n);
            }
        } else {
            self.table.reserve(n, |&s| table_hash(s));
            self.rows.reserve(n);
            self.arena.reserve(bytes + n);
        }
    }

    /// The row under `key`.
    pub fn find(&self, key: &[u8]) -> Option<u32> {
        let cur = self.cursor.load(Relaxed);
        let c = cur.wrapping_add(1);
        if (c as usize) < self.rows() && self.present(c) && *self.key_of(c) == *key {
            self.cursor.store(c, Relaxed);
            return Some(c);
        }
        if let Some(r) = self.forward(cur, key) {
            self.cursor.store(r, Relaxed);
            return Some(r);
        }
        if let Some(r) = self.asc_row(key).filter(|&r| self.present(r)) {
            self.cursor.store(r, Relaxed);
            return Some(r);
        }
        let found = self.table_row(key);
        if let Some(r) = found {
            self.cursor.store(r, Relaxed);
        }
        found
    }

    /// The row under `key` found by searching forward from row `cur` within the ascending
    /// rows (`asc`): galloping, then halving. None: not there (or not present: the hash table
    /// has the key's row, if any).
    fn forward(&self, cur: u32, key: &[u8]) -> Option<u32> {
        let end = self.asc as usize;
        let cur = cur as usize;
        if cur >= end || *self.asc_key(cur) >= *key {
            return None;
        }
        if let AscKeys::Arith(a) = &self.asc_keys {
            return a.find(end, key).filter(|&at| self.asc_tids[at].present()).map(|at| at as u32);
        }
        // (Rows cur+1..; the first whose key is at least `key`. Only near the cursor: a key
        // further on is a random lookup, which the hash table answers in a probe or two
        // where a search of the rows' keys would miss the cache at every step.)
        let (mut lo, mut step) = (cur + 1, 1);
        let mut hi = lo;
        while hi < end && *self.asc_key(hi) < *key {
            if step > FORWARD_MAX {
                return None;
            }
            lo = hi + 1;
            hi = (hi + step).min(end);
            step *= 2;
        }
        let hi = hi.min(end);
        let at = self.asc_search(lo, hi, key);
        (at < end && self.asc_tids[at].present() && *self.asc_key(at) == *key).then_some(at as u32)
    }

    /// The TID of the current version of the row under `key`.
    pub fn get(&self, key: &[u8]) -> Option<Tid> {
        self.find(key).map(|r| self.tid(r))
    }

    /// Row `row`'s key (present or removed).
    pub fn key(&self, row: u32) -> Option<RowKey<'_>> {
        ((row as usize) < self.rows()).then(|| self.key_of(row))
    }

    /// The row under `key` now has its current version at `tid` (a new row if no row is
    /// under it). Returns the row.
    pub fn insert(&mut self, key: &[u8], tid: Tid) -> u32 {
        // (A key past every key added so far is new: no probe for it; an ascending row's is in
        // no hash table, a later one's has only its slot added.)
        if self.rows() == 0 || key > self.max_key.as_slice() {
            let row = self.push_row(key, tid);
            if row >= self.asc {
                let h = self.hash(key);
                self.table.insert_unique(table_hash(h), slot(row, h), |&s| table_hash(s));
            }
            return row;
        }
        if let Some(row) = self.asc_row(key).filter(|&r| self.present(r)) {
            self.set_tid(row, tid);
            return row;
        }
        let h = self.hash(key);
        // (The row a new key would get.)
        let next = self.rows() as u32;
        let (rows, arena, asc) = (&self.rows, &self.arena, self.asc);
        let entry = self.table.entry(
            table_hash(h),
            |&s| same_half(s, h) && hashed_key(rows, arena, asc, s as u32) == key,
            |&s| table_hash(s),
        );
        match entry {
            hashbrown::hash_table::Entry::Occupied(e) => {
                let row = *e.get() as u32;
                self.set_tid(row, tid);
                row
            }
            hashbrown::hash_table::Entry::Vacant(e) => {
                e.insert(slot(next, h));
                self.push_row(key, tid)
            }
        }
    }

    /// A new row for `key` (present): an ascending one when it is the greatest key yet and
    /// every row so far is one, else one with its key's bytes in the arena.
    fn push_row(&mut self, key: &[u8], tid: Tid) -> u32 {
        let row = u32::try_from(self.rows()).ok().filter(|&n| n != NO_ROW).expect("rows");
        let greatest = row == 0 || key > self.max_key.as_slice();
        if !(greatest && row == self.asc && self.push_asc(key, tid)) {
            self.rows.push(Row::new(self.arena.len(), tid));
            put_varint(&mut self.arena, key.len() as u64);
            self.arena.extend_from_slice(key);
        }
        if greatest {
            self.max_key.clear();
            self.max_key.extend_from_slice(key);
        }
        self.cursor.store(row, Relaxed);
        row
    }

    /// `key` laid out as the next ascending row (false: it can't be, its keys past 4 GB).
    fn push_asc(&mut self, key: &[u8], tid: Tid) -> bool {
        if self.asc == 0 {
            self.asc_keys = match Arith::new(key) {
                Some(a) => AscKeys::Arith(a),
                None if key.is_empty() => AscKeys::Var(Vec::new()),
                None => AscKeys::Fixed(key.len()),
            };
        } else if let AscKeys::Arith(a) = &mut self.asc_keys
            && !a.next(self.asc as usize, key)
        {
            self.unarith();
        }
        if let AscKeys::Fixed(n) = self.asc_keys
            && n != key.len()
        {
            // (A key of another length: every key's length and place from now on.)
            let old = std::mem::take(&mut self.asc_bytes);
            let mut at = Vec::with_capacity(self.asc as usize + 1);
            for k in old.chunks_exact(n) {
                at.push(self.asc_bytes.len() as u32);
                put_varint(&mut self.asc_bytes, n as u64);
                self.asc_bytes.extend_from_slice(k);
            }
            self.asc_keys = AscKeys::Var(at);
        }
        match &mut self.asc_keys {
            AscKeys::Arith(_) => {
                // (The room `reserve` made before the second key showed they step evenly.)
                if self.asc > 0 && self.asc_bytes.capacity() != 0 {
                    self.asc_bytes = Vec::new();
                }
            }
            AscKeys::Fixed(_) => self.asc_bytes.extend_from_slice(key),
            AscKeys::Var(at) => {
                let Ok(pos) = u32::try_from(self.asc_bytes.len() + key.len() + 10) else { return false };
                at.push(pos - key.len() as u32 - 10);
                put_varint(&mut self.asc_bytes, key.len() as u64);
                self.asc_bytes.extend_from_slice(key);
            }
        }
        self.asc_tids.push(AscTid::new(tid));
        self.asc += 1;
        self.asc_present += 1;
        true
    }

    /// Row `row` (one `find` gave) now has its current version at `tid`: `insert` of its
    /// key, without the lookup.
    pub fn set_tid(&mut self, row: u32, tid: Tid) {
        self.set_row_tid(row, tid);
        self.cursor.store(row, std::sync::atomic::Ordering::Relaxed);
    }

    /// No row is under `key` any more (its row keeps the key's bytes). Returns its row.
    pub fn remove(&mut self, key: &[u8]) -> Option<u32> {
        if let Some(row) = self.asc_row(key).filter(|&r| self.present(r)) {
            self.unpresent(row);
            return Some(row);
        }
        if self.table.is_empty() {
            return None;
        }
        let h = self.hash(key);
        let (rows, arena, asc) = (&self.rows, &self.arena, self.asc);
        let (s, _) = self
            .table
            .find_entry(table_hash(h), |&s| same_half(s, h) && hashed_key(rows, arena, asc, s as u32) == key)
            .ok()?
            .remove();
        let row = s as u32;
        self.unpresent(row);
        Some(row)
    }

    /// Row `row` is no longer under its key if it is the one there with its current version
    /// at `tid` (a deleted row's last version freed). Returns whether it was.
    pub fn remove_row_at(&mut self, row: u32, tid: Tid) -> bool {
        if row as usize >= self.rows() || self.tid(row) != tid || !self.present(row) {
            return false;
        }
        if row < self.asc {
            self.unpresent(row);
            return true;
        }
        let h = self.hash(&self.key_of(row));
        match self.table.find_entry(table_hash(h), |&s| s as u32 == row) {
            Ok(e) => {
                e.remove();
                self.unpresent(row);
                true
            }
            Err(_) => false,
        }
    }

    /// Keep the keys whose current versions' TIDs `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(Tid) -> bool) {
        for a in &mut self.asc_tids {
            if a.present() && !keep(a.tid()) {
                a.0[2] &= !PRESENT;
                self.asc_present -= 1;
            }
        }
        let (rows, asc) = (&mut self.rows, self.asc);
        self.table.retain(|&mut s| {
            let row = &mut rows[(s as u32 - asc) as usize];
            let kept = keep((row.blk, row.off));
            if !kept {
                row.part &= !PRESENT;
            }
            kept
        });
    }

    /// Every key present, its row and its current version's TID, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (RowKey<'_>, u32, Tid)> {
        let asc = (0..self.asc).filter(move |&i| self.asc_tids[i as usize].present());
        asc.chain(self.table.iter().map(|&s| s as u32)).map(move |i| (self.key_of(i), i, self.tid(i)))
    }

    /// Whether removed keys' rows are so many that compacting them is worth a pass over the
    /// heap's versions.
    pub fn wasteful(&self) -> bool {
        let dead = self.rows() - self.len();
        dead >= 1 << 16 && dead >= self.len()
    }

    /// Keep only the rows `keep` marks (by row number; the keys present are always kept),
    /// renumbered in order. Returns each old row's new number (`NO_ROW`: dropped).
    pub fn compact(&mut self, keep: &[bool]) -> Vec<u32> {
        // (The rows kept needn't step evenly: laid out first.)
        self.unarith();
        let total = self.rows();
        let mut map = vec![NO_ROW; total];
        for &s in self.table.iter() {
            map[s as u32 as usize] = 0;
        }
        for (i, r) in self.asc_tids.iter().enumerate() {
            if r.present() {
                map[i] = 0;
            }
        }
        let var = matches!(self.asc_keys, AscKeys::Var(_));
        let (mut asc_tids, mut asc_bytes, mut at) = (Vec::new(), Vec::new(), Vec::new());
        let mut rows = Vec::with_capacity(self.len());
        let mut arena = Vec::new();
        let mut n = 0;
        for (i, m) in map.iter_mut().enumerate() {
            if *m == NO_ROW && !keep.get(i).copied().unwrap_or(false) {
                continue;
            }
            let k = self.key_of(i as u32);
            // (Rows kept from the ascending ones still ascend, and come first.)
            if i < self.asc as usize {
                if var {
                    at.push(asc_bytes.len() as u32);
                    put_varint(&mut asc_bytes, k.len() as u64);
                }
                asc_bytes.extend_from_slice(&k);
                asc_tids.push(self.asc_tids[i]);
            } else {
                let r = self.rows[i - self.asc as usize];
                let pos = arena.len();
                put_varint(&mut arena, k.len() as u64);
                arena.extend_from_slice(&k);
                rows.push(Row { at: (pos % PART) as u32, part: (pos / PART) as u16 | (r.part & PRESENT), ..r });
            }
            *m = n;
            n += 1;
        }
        for s in self.table.iter_mut() {
            *s = (*s & !(u32::MAX as u64)) | map[*s as u32 as usize] as u64;
        }
        if var {
            self.asc_keys = AscKeys::Var(at);
        }
        self.asc = asc_tids.len() as u32;
        (self.asc_tids, self.asc_bytes, self.rows, self.arena) = (asc_tids, asc_bytes, rows, arena);
        self.cursor.store(0, Relaxed);
        map
    }

    /// Bytes held (the table's slots and control bytes, the rows, the keys).
    pub fn bytes(&self) -> usize {
        let at = match &self.asc_keys {
            AscKeys::Var(at) => at.capacity() * 4,
            AscKeys::Fixed(_) | AscKeys::Arith(_) => 0,
        };
        self.table.capacity() * (size_of::<u64>() + 1)
            + self.rows.capacity() * size_of::<Row>()
            + self.arena.capacity()
            + self.asc_tids.capacity() * size_of::<AscTid>()
            + self.asc_bytes.capacity()
            + at
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
                        assert_eq!(m.key(row).as_deref(), Some(&k[..]));
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
                    assert_eq!(m.key(row).as_deref(), Some(&k[..]));
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
        // (Searched, not hashed: no hash table for them.)
        assert_eq!((m.table.len(), m.table.capacity(), m.len()), (0, 0, 50_000));
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

    /// Ascending keys of other lengths: each key's place kept from the first one of another
    /// length on, found, removed and compacted as keys of one length are.
    #[test]
    fn ascending_rows_of_other_lengths() {
        let key = |i: u32| (if i < 1000 { format!("a{i:05}") } else { format!("b{i:07}") }).into_bytes();
        let mut m = KeyMap::default();
        for i in 0..3000u32 {
            assert_eq!(m.insert(&key(i), (i, 1)), i);
        }
        assert_eq!((m.asc, m.table.len()), (3000, 0));
        for i in 0..3000u32 {
            assert_eq!(m.get(&key(i)), Some((i, 1)));
            assert_eq!(m.key(i).as_deref(), Some(&key(i)[..]));
        }
        assert_eq!(m.remove(&key(5)), Some(5));
        assert_eq!(m.get(&key(5)), None);
        let map = m.compact(&[]);
        assert_eq!((map[5], map[6]), (NO_ROW, 5));
        for i in (0..3000u32).filter(|&i| i != 5) {
            assert_eq!(m.get(&key(i)), Some((i, 1)));
        }
        assert_eq!(m.insert(b"c", (9, 9)), 2999);
        assert_eq!((m.asc, m.get(b"c")), (3000, Some((9, 9))));
    }

    /// Keys stepping evenly (an integer key's encoding: a tag, then 8 bytes) keep no bytes,
    /// and answer as a `HashMap` does through lookups between and past them, removals, a key
    /// that breaks the step (laid out from then on), and a compaction.
    #[test]
    fn ascending_rows_stepping_evenly_keep_no_bytes() {
        let key = |v: i64| {
            let mut k = vec![3u8];
            k.extend_from_slice(&((v as u64) ^ (1 << 63)).to_be_bytes());
            k
        };
        let mut m = KeyMap::default();
        let mut r = std::collections::HashMap::new();
        m.reserve(20_000, 9 * 20_000);
        for i in 0..20_000i64 {
            m.insert(&key(i * 3 - 30_000), (i as u32, 1));
            r.insert(key(i * 3 - 30_000), (i as u32, 1));
        }
        assert!(matches!(m.asc_keys, AscKeys::Arith(_)) && m.asc_bytes.capacity() == 0, "{:?}", m.asc_keys);
        let check = |m: &KeyMap, r: &std::collections::HashMap<Vec<u8>, Tid>| {
            for v in -30_010..30_010i64 {
                assert_eq!(m.get(&key(v)), r.get(&key(v)).copied(), "key {v}");
            }
            assert_eq!(m.get(&key(-30_000)[..8]), None);
            for (k, row, tid) in m.iter() {
                assert_eq!((r.get(&k[..]), m.key(row).as_deref()), (Some(&tid), Some(&k[..])));
            }
            assert_eq!(m.len(), r.len());
        };
        check(&m, &r);
        for v in (-30_000..30_000i64).step_by(21) {
            assert_eq!(m.remove(&key(v)).is_some(), r.remove(&key(v)).is_some());
        }
        check(&m, &r);
        assert!(matches!(m.asc_keys, AscKeys::Arith(_)));
        // (Past the last key but off the step: the keys laid out, this one ascending too.)
        m.insert(&key(40_001), (7, 7));
        r.insert(key(40_001), (7, 7));
        assert!(matches!(m.asc_keys, AscKeys::Fixed(9)) && m.asc == 20_001);
        check(&m, &r);
        let _ = m.compact(&[]);
        check(&m, &r);
    }

    /// Random lookups of ascending keys of one length spread unevenly (squares, a common
    /// prefix, a cluster) find what halving finds, keys between them none.
    #[test]
    fn ascending_rows_found_by_interpolation() {
        let key = |v: u64| format!("p{v:015}").into_bytes();
        let vals: Vec<u64> = (0..5000u64).map(|i| if i < 4000 { i * i * 7 } else { 112_000_000 + i }).collect();
        let mut m = KeyMap::default();
        for (i, &v) in vals.iter().enumerate() {
            m.insert(&key(v), (i as u32, 1));
        }
        assert_eq!(m.asc, 5000);
        let mut x = 11u64;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let i = (x >> 33) as usize % vals.len();
            // (Far from the cursor: not found by its forward search.)
            m.cursor.store(((i + 2500) % vals.len()) as u32, Relaxed);
            assert_eq!(m.get(&key(vals[i])), Some((i as u32, 1)));
            assert_eq!(m.get(&key(vals[i] + 1)), vals.binary_search(&(vals[i] + 1)).ok().map(|j| (j as u32, 1)));
        }
    }
}
