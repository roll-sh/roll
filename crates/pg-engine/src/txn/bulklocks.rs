//! Compact lock sets for a transaction holding many locks: the row locks and unique-key locks
//! of a bulk statement (an INSERT … SELECT into a table with a primary key, an UPDATE or
//! DELETE of millions of rows). Postgres keeps these in the tuples (xmax) and index entries
//! (an in-progress xmin a dirty snapshot sees), at no memory cost; the engine's writes stay
//! private until commit, so others find them through the lock manager. A lock-manager entry
//! per row (its key copied twice, a holder list, the transaction's own list of what to
//! release) is a few hundred bytes; past a few thousand of them a transaction's next ones go
//! into a set like this: each key once in one arena, with a byte of what is held, under an
//! open-addressing index of offsets (about 10 bytes plus the key).
//!
//! The lock manager consults these sets wherever it looks a row or a unique key up, so other
//! transactions see the locks exactly as before; a heavyweight wait on one (a unique key
//! another writer wants) first moves it into the main table (`LockManager::materialize`), so
//! queueing, deadlock detection and pg_locks go through the usual paths.

use super::locks::RowLockMode;
use crate::exec::fasthash::FastState;
use std::hash::BuildHasher;

/// How many row locks (and, apart, unique-key locks) a transaction takes the usual way before
/// its next ones go into its compact sets: 2,048, or `ROLL_COMPACT_LOCKS_AFTER` (0 for
/// tests: every lock that can be compact is).
pub(crate) fn compact_after() -> usize {
    static N: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
        std::env::var("ROLL_COMPACT_LOCKS_AFTER")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048)
    });
    *N
}

/// A transaction's compact locks, by the (sub)transaction holding them.
pub(crate) struct BulkLocks {
    /// The backend whose they are.
    pub proc: u64,
    pub segs: Vec<Seg>,
}

/// One (sub)transaction's compact locks.
pub(crate) struct Seg {
    /// Its lock id (whose transaction lock waiting for it waits on).
    pub holder: u64,
    /// Its transaction id (what a unique key's in-progress entry shows).
    pub xid: u64,
    /// Row locks: the tag is `row_tag`.
    pub rows: KeySet,
    /// Unique key locks, held Exclusive, each standing for an in-progress index entry of this
    /// (sub)transaction (`KeyHolder { holder, xid, token: None, row: None }`).
    pub ukeys: KeySet,
    /// Unique key locks moved into the main table since (`LockManager::materialize`): they
    /// go with the rest.
    pub moved: Vec<Vec<u8>>,
}

impl BulkLocks {
    /// The segment of `holder`, made when it has none.
    pub fn seg(&mut self, holder: u64, xid: u64) -> &mut Seg {
        let i = match self.segs.iter().position(|g| g.holder == holder) {
            Some(i) => i,
            None => {
                self.segs.push(Seg {
                    holder,
                    xid,
                    rows: KeySet::default(),
                    ukeys: KeySet::with_payloads(),
                    moved: Vec::new(),
                });
                self.segs.len() - 1
            }
        };
        let g = &mut self.segs[i];
        if g.xid == 0 {
            g.xid = xid;
        }
        g
    }

    pub fn len(&self) -> usize {
        self.segs.iter().map(|g| g.rows.len() + g.ukeys.len()).sum()
    }
}

/// A row lock's tag: its strength, and whether it is an update's or a delete's.
pub(crate) fn row_tag(mode: RowLockMode, modifies: bool) -> u8 {
    (match mode {
        RowLockMode::KeyShare => 0,
        RowLockMode::Share => 1,
        RowLockMode::NoKeyUpdate => 2,
        RowLockMode::Update => 3,
    }) | if modifies { MODIFIES } else { 0 }
}

pub(crate) fn untag(t: u8) -> (RowLockMode, bool) {
    let mode = match t & 3 {
        0 => RowLockMode::KeyShare,
        1 => RowLockMode::Share,
        2 => RowLockMode::NoKeyUpdate,
        _ => RowLockMode::Update,
    };
    (mode, t & MODIFIES != 0)
}

const MODIFIES: u8 = 0x10;

/// A set of byte keys, each with a tag byte, in one arena.
#[derive(Default)]
pub(crate) struct KeySet {
    /// Entries: the key's length (2 bytes, little-endian), the key, the tag.
    arena: Vec<u8>,
    /// A power of two of slots: 0 empty, else the key's hash's top 32 bits over its offset
    /// into `arena` + 1 (the arena is read only when those bits match).
    slots: Vec<u64>,
    /// Live keys.
    len: usize,
    /// Entries removed (tag `DEAD`), still in the arena and the slots.
    dead: usize,
    /// Entries carry a payload after the tag (its length byte, then its bytes).
    payloads: bool,
}

/// The tag of a removed entry.
const DEAD: u8 = 0xff;

fn hash(key: &[u8]) -> u64 {
    FastState::default().hash_one(key)
}

fn slot_of(h: u64, off: usize) -> u64 {
    (h >> 32) << 32 | (off as u64 + 1)
}

impl KeySet {
    pub fn len(&self) -> usize {
        self.len
    }

    fn key_at(&self, off: usize) -> (&[u8], u8) {
        let n = u16::from_le_bytes([self.arena[off], self.arena[off + 1]]) as usize;
        (&self.arena[off + 2..off + 2 + n], self.arena[off + 2 + n])
    }

    /// The payload of the entry at `off` (empty when it has none).
    fn payload_at(&self, off: usize) -> &[u8] {
        if !self.payloads {
            return &[];
        }
        let n = u16::from_le_bytes([self.arena[off], self.arena[off + 1]]) as usize;
        let p = off + 3 + n;
        &self.arena[p + 1..p + 1 + self.arena[p] as usize]
    }

    /// The length of the entry at `off`.
    fn entry_len(&self, off: usize) -> usize {
        let n = u16::from_le_bytes([self.arena[off], self.arena[off + 1]]) as usize;
        if !self.payloads {
            return 3 + n;
        }
        4 + n + self.arena[off + 3 + n] as usize
    }

    /// A set whose entries carry payloads (`insert_with`): a length byte more each.
    pub fn with_payloads() -> KeySet {
        KeySet { payloads: true, ..Default::default() }
    }

    /// The slot holding `key` (and its entry's offset), or the empty slot it would go in.
    fn find(&self, key: &[u8], h: u64) -> (usize, Option<usize>) {
        let mask = self.slots.len() - 1;
        let mut i = h as usize & mask;
        loop {
            let s = self.slots[i];
            if s == 0 {
                return (i, None);
            }
            if s >> 32 == h >> 32 {
                let off = (s as u32 - 1) as usize;
                let (k, tag) = self.key_at(off);
                if tag != DEAD && k == key {
                    return (i, Some(off));
                }
            }
            i = (i + 1) & mask;
        }
    }

    /// The tag of `key`, if it is in the set.
    pub fn get(&self, key: &[u8]) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let (_, off) = self.find(key, hash(key));
        off.map(|o| self.arena[o + 2 + key.len()])
    }

    /// The payload `key` was put in with (`insert_with`), if it is in the set.
    pub fn payload(&self, key: &[u8]) -> Option<&[u8]> {
        if self.len == 0 {
            return None;
        }
        let (_, off) = self.find(key, hash(key));
        off.map(|o| self.payload_at(o))
    }

    /// Put `key` in with `tag`, or give it `tag` when it is in already. Keys longer than
    /// 64 kB, or an arena past 4 GB, don't fit: false (the caller locks it the usual way).
    #[cfg(test)]
    pub fn insert(&mut self, key: &[u8], tag: u8) -> bool {
        self.upsert(key, |_| tag)
    }

    /// `insert`, the entry carrying `payload` (up to 255 bytes; a longer one is left out)
    /// when it is new.
    pub fn insert_with(&mut self, key: &[u8], tag: u8, payload: &[u8]) -> bool {
        self.upsert_with(key, payload, |_| tag)
    }

    /// Put `key` in, its tag `f(the tag it had)` (None: not in). False as `insert`.
    pub fn upsert(&mut self, key: &[u8], f: impl FnOnce(Option<u8>) -> u8) -> bool {
        self.upsert_with(key, &[], f)
    }

    fn upsert_with(&mut self, key: &[u8], payload: &[u8], f: impl FnOnce(Option<u8>) -> u8) -> bool {
        let payload = if payload.len() > u8::MAX as usize { &[][..] } else { payload };
        if key.len() > u16::MAX as usize || self.arena.len() + key.len() + payload.len() + 4 >= u32::MAX as usize {
            return false;
        }
        if (self.len + self.dead + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        let h = hash(key);
        let (i, found) = self.find(key, h);
        if let Some(off) = found {
            let at = off + 2 + key.len();
            self.arena[at] = f(Some(self.arena[at]));
            debug_assert!(self.arena[at] != DEAD);
            return true;
        }
        let tag = f(None);
        debug_assert!(tag != DEAD);
        let off = self.arena.len();
        self.arena.extend_from_slice(&(key.len() as u16).to_le_bytes());
        self.arena.extend_from_slice(key);
        self.arena.push(tag);
        if self.payloads {
            self.arena.push(payload.len() as u8);
            self.arena.extend_from_slice(payload);
        }
        self.slots[i] = slot_of(h, off);
        self.len += 1;
        true
    }

    /// Give `key`, when it is in, the tag `f(its tag)`; whether it was in.
    pub fn update(&mut self, key: &[u8], f: impl FnOnce(u8) -> u8) -> bool {
        if self.len == 0 {
            return false;
        }
        let (_, found) = self.find(key, hash(key));
        let Some(off) = found else { return false };
        let at = off + 2 + key.len();
        self.arena[at] = f(self.arena[at]);
        true
    }

    /// Take `key` out; whether it was in.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        if self.len == 0 {
            return false;
        }
        let (_, found) = self.find(key, hash(key));
        let Some(off) = found else { return false };
        self.arena[off + 2 + key.len()] = DEAD;
        self.len -= 1;
        self.dead += 1;
        true
    }

    /// The live keys with their tags, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u8)> {
        self.entries().map(|(k, t, _)| (k, t))
    }

    /// The live keys with their tags and payloads, in no particular order.
    pub fn entries(&self) -> impl Iterator<Item = (&[u8], u8, &[u8])> {
        let mut off = 0;
        std::iter::from_fn(move || {
            while off < self.arena.len() {
                let at = off;
                let (k, tag) = self.key_at(at);
                off += self.entry_len(at);
                if tag != DEAD {
                    return Some((k, tag, self.payload_at(at)));
                }
            }
            None
        })
    }

    /// Double the slots (or drop the dead entries, when they are most of them) and index
    /// the entries again.
    fn grow(&mut self) {
        if self.dead > self.len {
            let live: Vec<(Vec<u8>, u8, Vec<u8>)> =
                self.entries().map(|(k, t, p)| (k.to_vec(), t, p.to_vec())).collect();
            self.arena.clear();
            self.dead = 0;
            self.len = 0;
            self.slots = vec![0; self.slots.len().max(16)];
            for (k, t, p) in live {
                self.insert_with(&k, t, &p);
            }
            return;
        }
        let n = (self.slots.len() * 2).max(16);
        let mut slots = vec![0u64; n];
        let mask = n - 1;
        let mut off = 0;
        while off < self.arena.len() {
            let (k, _) = self.key_at(off);
            let h = hash(k);
            let mut i = h as usize & mask;
            while slots[i] != 0 {
                i = (i + 1) & mask;
            }
            slots[i] = slot_of(h, off);
            off += self.entry_len(off);
        }
        self.slots = slots;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Inserts, re-tags and removals against a map, past several growths and a compaction.
    #[test]
    fn keyset_matches_a_map() {
        let mut s = KeySet::default();
        let mut m: HashMap<Vec<u8>, u8> = HashMap::new();
        let mut x: u64 = 7;
        for i in 0..200_000u64 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let k = (x >> 40) % 50_000;
            let key: Vec<u8> = format!("row{k}").into_bytes();
            match i % 7 {
                0 | 1 => {
                    assert_eq!(s.remove(&key), m.remove(&key).is_some());
                }
                _ => {
                    let tag = (x >> 8) as u8 & 0x7f;
                    assert!(s.insert(&key, tag));
                    m.insert(key.clone(), tag);
                }
            }
            if i % 9973 == 0 {
                assert_eq!(s.len(), m.len());
                for (k, t) in &m {
                    assert_eq!(s.get(k), Some(*t));
                }
            }
        }
        assert_eq!(s.len(), m.len());
        let mut seen: Vec<(Vec<u8>, u8)> = s.iter().map(|(k, t)| (k.to_vec(), t)).collect();
        seen.sort();
        let mut want: Vec<(Vec<u8>, u8)> = m.into_iter().collect();
        want.sort();
        assert_eq!(seen, want);
        assert_eq!(s.get(b"absent"), None);
        assert!(!s.insert(&vec![0; 70_000], 1));
    }

    /// Payloads stay with their keys past growths, compactions and re-tags.
    #[test]
    fn keyset_payloads() {
        let mut s = KeySet::with_payloads();
        for i in 0..50_000u32 {
            let key = format!("k{i}").into_bytes();
            assert!(s.insert_with(&key, (i % 100) as u8, &i.to_le_bytes()[..(i % 5) as usize]));
            if i % 3 == 0 {
                s.remove(&key);
            } else if i % 3 == 1 {
                s.insert(&key, 7);
            }
        }
        for i in 0..50_000u32 {
            let key = format!("k{i}").into_bytes();
            let bytes = i.to_le_bytes();
            let want = (i % 3 != 0).then(|| &bytes[..(i % 5) as usize]);
            assert_eq!(s.payload(&key), want, "{i}");
        }
        assert_eq!(s.entries().count(), s.len());
        assert!(s.insert_with(b"long", 1, &[1; 300]));
        assert_eq!(s.payload(b"long"), Some(&[][..]));
    }
}
