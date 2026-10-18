//! The layout of Redis 8's `dict` for the server's small dicts whose iteration order shows
//! (latency events, ...): chained buckets under the per-process hash seed, four buckets at
//! first, doubled at a 1:1 load and halved below 1:8, rehashed incrementally (a step on each
//! add, lookup and delete, the visited bucket first), new entries at a bucket's head. Only the
//! keys are kept; the values live with the caller.

use std::hash::{BuildHasher, RandomState};
use std::sync::LazyLock;

/// `dict_hash_function_seed`.
static KEY: LazyLock<RandomState> = LazyLock::new(RandomState::new);

/// `DICT_HT_INITIAL_SIZE`.
const INITIAL: usize = 4;
/// `HASHTABLE_MIN_FILL`.
const MIN_FILL: usize = 8;

/// A bucket's chain of (hash, key), head first.
type Chain = Vec<(u64, Vec<u8>)>;

#[derive(Clone, Debug, Default)]
pub struct ServerDict {
    /// `ht_table[0]` and `[1]`.
    t: [Vec<Chain>; 2],
    used: [usize; 2],
    rehashidx: Option<usize>,
    /// Keys hash and compare case-insensitively (`dictSdsCaseHash`).
    nocase: bool,
}

impl ServerDict {
    pub fn new(nocase: bool) -> Self {
        ServerDict { nocase, ..Default::default() }
    }

    fn hash(&self, key: &[u8]) -> u64 {
        if self.nocase { KEY.hash_one(key.to_ascii_lowercase()) } else { KEY.hash_one(key) }
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
        self.t[0].len().saturating_sub(1)
    }

    /// `rehashEntriesInBucketAtIndex`.
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

    /// `dictCheckRehashingCompleted`.
    fn check_completed(&mut self) -> bool {
        if self.used[0] != 0 {
            return false;
        }
        self.t[0] = std::mem::take(&mut self.t[1]);
        self.used = [self.used[1], 0];
        self.rehashidx = None;
        true
    }

    /// `dictRehash(d, 1)`.
    fn rehash_one(&mut self) {
        let Some(mut idx) = self.rehashidx else { return };
        if self.used[0] != 0 {
            let mut empty_visits = 10;
            while self.t[0][idx].is_empty() {
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
        if visited >= idx && !self.t[0][visited].is_empty() {
            self.move_bucket(visited);
            self.check_completed();
        } else {
            self.rehash_one();
        }
    }

    /// `_dictResize`: an empty or new table directly, else the target of a rehash.
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

    /// `dictExpandIfNeeded`.
    fn expand_if_needed(&mut self) {
        if self.rehashidx.is_some() {
            return;
        }
        if self.t[0].is_empty() {
            self.resize(INITIAL);
        } else if self.used[0] >= self.t[0].len() {
            self.resize(self.used[0] + 1);
        }
    }

    /// `dictShrinkIfNeeded`.
    fn shrink_if_needed(&mut self) {
        if self.rehashidx.is_some() || self.t[0].len() <= INITIAL {
            return;
        }
        if self.used[0] * MIN_FILL <= self.t[0].len() {
            self.resize(self.used[0]);
        }
    }

    /// `dictExpand`: room for `n` (a fresh dict gets its table at once).
    pub fn expand(&mut self, n: usize) {
        if self.rehashidx.is_some() || self.used[0] > n || self.t[0].len() >= n {
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
        let mask = self.t[ti].len() - 1;
        self.t[ti][h as usize & mask].insert(0, (h, key.to_vec()));
        self.used[ti] += 1;
        true
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

    /// `dictFind` (with its rehash step). Returns the stored key.
    pub fn find(&mut self, key: &[u8]) -> Option<&[u8]> {
        if self.is_empty() {
            return None;
        }
        let h = self.hash(key);
        self.step(h as usize & self.mask0());
        let (ti, b, i) = self.position(key, h)?;
        Some(&self.t[ti][b][i].1)
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
        let Some((ti, b, i)) = self.position(key, h) else { return false };
        self.t[ti][b].remove(i);
        self.used[ti] -= 1;
        self.shrink_if_needed();
        true
    }

    /// `dictEmpty`.
    pub fn clear(&mut self) {
        *self = ServerDict::new(self.nocase);
    }

    /// The keys in iteration order (`dictNext`: `ht_table[0]` from the rehash index, then `[1]`).
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let from = self.rehashidx.unwrap_or(0);
        let mut out: Vec<Vec<u8>> = self.t[0].iter().skip(from).flatten().map(|(_, k)| k.clone()).collect();
        if self.rehashidx.is_some() {
            out.extend(self.t[1].iter().flatten().map(|(_, k)| k.clone()));
        }
        out
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

    #[test]
    fn keeps_every_key_through_growth_and_shrinking() {
        let mut d = ServerDict::new(false);
        for i in 0..100 {
            assert!(d.add(format!("k{i}").as_bytes()));
        }
        assert!(!d.add(b"k5"));
        let mut keys = d.keys();
        keys.sort();
        assert_eq!(keys.len(), 100);
        for i in 0..95 {
            assert!(d.delete(format!("k{i}").as_bytes(), false));
            assert!(d.find(format!("k{i}").as_bytes()).is_none());
        }
        assert_eq!(d.keys().len(), 5);
        assert!(d.find(b"k99").is_some());
    }

    #[test]
    fn nocase_keys() {
        let mut d = ServerDict::new(true);
        d.add(b"Abc");
        assert_eq!(d.find(b"aBC"), Some(&b"Abc"[..]));
    }
}
