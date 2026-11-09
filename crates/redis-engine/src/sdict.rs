//! The layout of Redis 8's `dict` for the server's small dicts whose iteration order shows
//! (latency events, configs, modules, function libraries): chained buckets under the
//! per-process hash seed, four buckets at first, doubled at a 1:1 load and halved below 1:8,
//! rehashed incrementally (a step on each add, lookup and delete, the visited bucket first), new
//! entries at a bucket's head. Only the keys are kept; the values live with the caller.
//!
//! Entries live in flat arrays and buckets are the heads of intrusive chains, so a dict is a few
//! allocations whatever its size, and iterating it allocates nothing.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::LazyLock;

/// `dict_hash_function_seed`.
static KEY: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// `DICT_HT_INITIAL_SIZE`.
const INITIAL: usize = 4;
/// `HASHTABLE_MIN_FILL`.
const MIN_FILL: usize = 8;
/// No entry: the end of a chain, an empty bucket.
const NIL: u32 = u32::MAX;

/// The key's hash (`dictSdsHash` / `dictSdsCaseHash`), without allocating for the case fold.
fn hash_key(nocase: bool, key: &[u8]) -> u64 {
    let mut h = KEY.build_hasher();
    if nocase {
        let mut buf = [0u8; 64];
        for chunk in key.chunks(64) {
            let lower = &mut buf[..chunk.len()];
            lower.copy_from_slice(chunk);
            lower.make_ascii_lowercase();
            h.write(lower);
        }
    } else {
        h.write(key);
    }
    h.write_usize(key.len());
    h.finish()
}

#[derive(Clone, Debug, Default)]
pub struct ServerDict {
    /// `ht_table[0]` and `[1]`: each bucket's first entry.
    heads: [Vec<u32>; 2],
    used: [usize; 2],
    rehashidx: Option<usize>,
    /// Keys hash and compare case-insensitively (`dictSdsCaseHash`).
    nocase: bool,
    /// Entries: the next in their chain, their hash and key.
    next: Vec<u32>,
    hash: Vec<u64>,
    key: Vec<Box<[u8]>>,
    /// Entry slots freed by deletes.
    free: Vec<u32>,
}

impl ServerDict {
    pub fn new(nocase: bool) -> Self {
        ServerDict { nocase, ..Default::default() }
    }

    fn hash(&self, key: &[u8]) -> u64 {
        hash_key(self.nocase, key)
    }

    fn same(&self, a: &[u8], b: &[u8]) -> bool {
        if self.nocase { a.eq_ignore_ascii_case(b) } else { a == b }
    }

    pub fn len(&self) -> usize {
        self.used[0] + self.used[1]
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_rehashing(&self) -> bool {
        self.rehashidx.is_some()
    }

    fn mask0(&self) -> usize {
        self.heads[0].len().saturating_sub(1)
    }

    /// `rehashEntriesInBucketAtIndex`: the chain, head first, onto the heads of its new buckets.
    fn move_bucket(&mut self, idx: usize) {
        let grow = self.heads[1].len() > self.heads[0].len();
        let mask = self.heads[1].len() - 1;
        let mut e = std::mem::replace(&mut self.heads[0][idx], NIL);
        while e != NIL {
            let after = self.next[e as usize];
            let b = if grow { self.hash[e as usize] as usize & mask } else { idx & mask };
            self.next[e as usize] = self.heads[1][b];
            self.heads[1][b] = e;
            self.used[0] -= 1;
            self.used[1] += 1;
            e = after;
        }
    }

    /// `dictCheckRehashingCompleted`.
    fn check_completed(&mut self) -> bool {
        if self.used[0] != 0 {
            return false;
        }
        self.heads[0] = std::mem::take(&mut self.heads[1]);
        self.used = [self.used[1], 0];
        self.rehashidx = None;
        true
    }

    /// `dictRehash(d, 1)`.
    fn rehash_one(&mut self) {
        let Some(mut idx) = self.rehashidx else { return };
        if self.used[0] != 0 {
            let mut empty_visits = 10;
            while self.heads[0][idx] == NIL {
                idx += 1;
                empty_visits -= 1;
                if empty_visits == 0 {
                    self.rehashidx = Some(idx);
                    return;
                }
            }
            self.move_bucket(idx);
            self.rehashidx = Some(idx + 1);
        }
        self.check_completed();
    }

    /// `_dictRehashStepIfNeeded`: the visited bucket if it still has entries, else a step.
    fn step(&mut self, visited: usize) {
        let Some(idx) = self.rehashidx else { return };
        if visited >= idx && self.heads[0][visited] != NIL {
            self.move_bucket(visited);
            self.check_completed();
        } else {
            self.rehash_one();
        }
    }

    /// `_dictResize`: an empty or new table directly, else the target of a rehash.
    fn resize(&mut self, size: usize) {
        let n = size.max(INITIAL).next_power_of_two();
        if n == self.heads[0].len() {
            return;
        }
        if self.heads[0].is_empty() || self.used[0] == 0 {
            self.heads[0] = vec![NIL; n];
            self.used[0] = 0;
            return;
        }
        self.heads[1] = vec![NIL; n];
        self.rehashidx = Some(0);
    }

    /// `dictExpandIfNeeded`.
    fn expand_if_needed(&mut self) {
        if self.rehashidx.is_some() {
            return;
        }
        if self.heads[0].is_empty() {
            self.resize(INITIAL);
        } else if self.used[0] >= self.heads[0].len() {
            self.resize(self.used[0] + 1);
        }
    }

    /// `dictShrinkIfNeeded`.
    fn shrink_if_needed(&mut self) {
        if self.rehashidx.is_some() || self.heads[0].len() <= INITIAL {
            return;
        }
        if self.used[0] * MIN_FILL <= self.heads[0].len() {
            self.resize(self.used[0]);
        }
    }

    /// `dictExpand`: room for `n` (a fresh dict gets its table at once).
    pub fn expand(&mut self, n: usize) {
        if self.rehashidx.is_some() || self.used[0] > n || self.heads[0].len() >= n {
            return;
        }
        self.resize(n);
    }

    /// `dictAdd`: false (with the rehash step and expansion taken) when the key exists.
    pub fn add(&mut self, key: &[u8]) -> bool {
        let h = self.hash(key);
        self.step(h as usize & self.mask0());
        self.expand_if_needed();
        if self.position(key, h).is_some() {
            return false;
        }
        let ti = usize::from(self.rehashidx.is_some());
        let b = h as usize & (self.heads[ti].len() - 1);
        let e = match self.free.pop() {
            Some(e) => {
                self.hash[e as usize] = h;
                self.key[e as usize] = key.into();
                e
            }
            None => {
                self.next.push(NIL);
                self.hash.push(h);
                self.key.push(key.into());
                (self.key.len() - 1) as u32
            }
        };
        self.next[e as usize] = self.heads[ti][b];
        self.heads[ti][b] = e;
        self.used[ti] += 1;
        true
    }

    /// The entry holding `key`: its table, bucket, the entry before it in the chain and itself.
    fn position(&self, key: &[u8], h: u64) -> Option<(usize, usize, u32, u32)> {
        let idx0 = h as usize & self.mask0();
        for ti in 0..2 {
            if self.heads[ti].is_empty() {
                break;
            }
            if ti == 0 && self.rehashidx.is_some_and(|r| idx0 < r) {
                continue;
            }
            let b = h as usize & (self.heads[ti].len() - 1);
            let (mut prev, mut e) = (NIL, self.heads[ti][b]);
            while e != NIL {
                if self.hash[e as usize] == h && self.same(&self.key[e as usize], key) {
                    return Some((ti, b, prev, e));
                }
                prev = e;
                e = self.next[e as usize];
            }
            if self.rehashidx.is_none() {
                break;
            }
        }
        None
    }

    /// `dictFind` (with its rehash step). Returns the stored key.
    pub fn find(&mut self, key: &[u8]) -> Option<&[u8]> {
        if self.is_empty() {
            return None;
        }
        let h = self.hash(key);
        self.step(h as usize & self.mask0());
        let (_, _, _, e) = self.position(key, h)?;
        Some(&self.key[e as usize])
    }

    /// Is `key` there? (A lookup Redis doesn't make: no rehash step.)
    pub fn contains(&self, key: &[u8]) -> bool {
        !self.is_empty() && self.position(key, self.hash(key)).is_some()
    }

    /// `dictDelete`; `paused` is a safe iterator in progress (no rehash step).
    pub fn delete(&mut self, key: &[u8], paused: bool) -> bool {
        if self.is_empty() {
            return false;
        }
        let h = self.hash(key);
        if !paused {
            self.step(h as usize & self.mask0());
        }
        let Some((ti, b, prev, e)) = self.position(key, h) else { return false };
        let after = self.next[e as usize];
        if prev == NIL {
            self.heads[ti][b] = after;
        } else {
            self.next[prev as usize] = after;
        }
        self.key[e as usize] = Box::default();
        self.free.push(e);
        self.used[ti] -= 1;
        self.shrink_if_needed();
        true
    }

    /// `dictEmpty`.
    pub fn clear(&mut self) {
        *self = ServerDict::new(self.nocase);
    }

    /// The keys in iteration order (`dictNext`: `ht_table[0]` from the rehash index, then `[1]`).
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> + '_ {
        let from = self.rehashidx.unwrap_or(0);
        let tables = if self.rehashidx.is_some() { 2 } else { 1 };
        (0..tables).flat_map(move |ti| {
            let skip = if ti == 0 { from } else { 0 };
            self.heads[ti].iter().skip(skip).flat_map(move |&head| {
                std::iter::successors((head != NIL).then_some(head), move |&e| {
                    let n = self.next[e as usize];
                    (n != NIL).then_some(n)
                })
                .map(move |e| &self.key[e as usize][..])
            })
        })
    }

    /// [`ServerDict::iter`], owned.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.iter().map(<[u8]>::to_vec).collect()
    }

    /// Delete the keys `pred` picks while iterating with a safe iterator (rehashing paused, as
    /// LATENCY RESET does). Returns how many went.
    pub fn delete_while_iterating(&mut self, mut pred: impl FnMut(&[u8]) -> bool) -> usize {
        let mut n = 0;
        // A shrink started mid-way leaves an empty new table, so the snapshot is what the
        // iterator visits.
        for k in self.keys() {
            if pred(&k) && self.delete(&k, true) {
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward layout (a chain per bucket, entries moved by value), kept to check
    /// the compact one against.
    type Chain = Vec<(u64, Vec<u8>)>;

    #[derive(Default)]
    struct Model {
        t: [Vec<Chain>; 2],
        used: [usize; 2],
        rehashidx: Option<usize>,
        nocase: bool,
    }

    impl Model {
        fn mask0(&self) -> usize {
            self.t[0].len().saturating_sub(1)
        }
        fn same(&self, a: &[u8], b: &[u8]) -> bool {
            if self.nocase { a.eq_ignore_ascii_case(b) } else { a == b }
        }
        fn move_bucket(&mut self, idx: usize) {
            let grow = self.t[1].len() > self.t[0].len();
            let mask = self.t[1].len() - 1;
            for e in std::mem::take(&mut self.t[0][idx]) {
                let h = if grow { e.0 as usize & mask } else { idx & mask };
                self.t[1][h].insert(0, e);
                self.used[0] -= 1;
                self.used[1] += 1;
            }
        }
        fn check_completed(&mut self) {
            if self.used[0] == 0 {
                self.t[0] = std::mem::take(&mut self.t[1]);
                self.used = [self.used[1], 0];
                self.rehashidx = None;
            }
        }
        fn step(&mut self, visited: usize) {
            let Some(mut idx) = self.rehashidx else { return };
            if visited >= idx && !self.t[0][visited].is_empty() {
                self.move_bucket(visited);
                self.check_completed();
                return;
            }
            if self.used[0] != 0 {
                let mut empty = 10;
                while self.t[0][idx].is_empty() {
                    idx += 1;
                    empty -= 1;
                    if empty == 0 {
                        self.rehashidx = Some(idx);
                        return;
                    }
                }
                self.move_bucket(idx);
                self.rehashidx = Some(idx + 1);
            }
            self.check_completed();
        }
        fn resize(&mut self, size: usize) {
            let n = size.max(INITIAL).next_power_of_two();
            if n == self.t[0].len() {
                return;
            }
            if self.t[0].is_empty() || self.used[0] == 0 {
                self.t[0] = vec![Vec::new(); n];
                self.used[0] = 0;
                return;
            }
            self.t[1] = vec![Vec::new(); n];
            self.rehashidx = Some(0);
        }
        fn position(&self, key: &[u8], h: u64) -> Option<(usize, usize, usize)> {
            let idx0 = h as usize & self.mask0();
            for ti in 0..2 {
                if self.t[ti].is_empty() {
                    break;
                }
                if ti == 0 && self.rehashidx.is_some_and(|r| idx0 < r) {
                    continue;
                }
                let b = h as usize & (self.t[ti].len() - 1);
                if let Some(i) = self.t[ti][b].iter().position(|(_, k)| self.same(k, key)) {
                    return Some((ti, b, i));
                }
                if self.rehashidx.is_none() {
                    break;
                }
            }
            None
        }
        fn add(&mut self, key: &[u8]) -> bool {
            let h = hash_key(self.nocase, key);
            self.step(h as usize & self.mask0());
            if self.rehashidx.is_none() {
                if self.t[0].is_empty() {
                    self.resize(INITIAL);
                } else if self.used[0] >= self.t[0].len() {
                    self.resize(self.used[0] + 1);
                }
            }
            if self.position(key, h).is_some() {
                return false;
            }
            let ti = usize::from(self.rehashidx.is_some());
            let mask = self.t[ti].len() - 1;
            self.t[ti][h as usize & mask].insert(0, (h, key.to_vec()));
            self.used[ti] += 1;
            true
        }
        fn find(&mut self, key: &[u8]) -> bool {
            if self.used[0] + self.used[1] == 0 {
                return false;
            }
            let h = hash_key(self.nocase, key);
            self.step(h as usize & self.mask0());
            self.position(key, h).is_some()
        }
        fn delete(&mut self, key: &[u8]) -> bool {
            if self.used[0] + self.used[1] == 0 {
                return false;
            }
            let h = hash_key(self.nocase, key);
            self.step(h as usize & self.mask0());
            let Some((ti, b, i)) = self.position(key, h) else { return false };
            self.t[ti][b].remove(i);
            self.used[ti] -= 1;
            if self.rehashidx.is_none() && self.t[0].len() > INITIAL && self.used[0] * MIN_FILL <= self.t[0].len() {
                self.resize(self.used[0]);
            }
            true
        }
        fn keys(&self) -> Vec<Vec<u8>> {
            let from = self.rehashidx.unwrap_or(0);
            let mut out: Vec<Vec<u8>> = self.t[0].iter().skip(from).flatten().map(|(_, k)| k.clone()).collect();
            if self.rehashidx.is_some() {
                out.extend(self.t[1].iter().flatten().map(|(_, k)| k.clone()));
            }
            out
        }
    }

    #[test]
    fn same_layout_as_the_model() {
        for nocase in [false, true] {
            let mut d = ServerDict::new(nocase);
            let mut m = Model { nocase, ..Default::default() };
            let mut x = 0x2545F4914F6CDD1Du64;
            for step in 0..20_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let key = format!("K{}", x % 300);
                let key = if nocase && x.is_multiple_of(7) { key.to_ascii_lowercase() } else { key };
                match x % 10 {
                    0..=4 => assert_eq!(d.add(key.as_bytes()), m.add(key.as_bytes()), "add {step}"),
                    5..=6 => assert_eq!(d.find(key.as_bytes()).is_some(), m.find(key.as_bytes()), "find {step}"),
                    _ => assert_eq!(d.delete(key.as_bytes(), false), m.delete(key.as_bytes()), "delete {step}"),
                }
                assert_eq!(d.keys(), m.keys(), "keys {step}");
            }
        }
    }

    #[test]
    fn nocase_keys() {
        let mut d = ServerDict::new(true);
        d.add(b"Abc");
        assert_eq!(d.find(b"aBC"), Some(&b"Abc"[..]));
    }
}
