//! `RsDict`'s layout for keys the caller hashes itself (RediSearch's value sets, hashed with
//! `RSValue_Hash`), for the sets read back in iteration order (TOLIST). Entries are the ids
//! `0, 1, 2…` of the values the caller keeps (and dedupes) itself, in the order it adds them.
//!
//! Buckets are the heads of intrusive chains over flat arrays: a set is a few allocations
//! whatever its size.

/// `DICT_HT_INITIAL_SIZE`.
const INITIAL: usize = 4;
/// No entry: the end of a chain, an empty bucket.
const NIL: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub struct HashDict {
    /// `ht[0]` and `ht[1]`: each bucket's first entry.
    heads: [Vec<u32>; 2],
    used: [usize; 2],
    rehashidx: Option<usize>,
    /// Per entry (by id): the next in its chain, and its hash.
    next: Vec<u32>,
    hash: Vec<u64>,
}

impl HashDict {
    /// `dictAdd`: the rehash step and expansion happen on every add, the existing keys
    /// included; `id` is `None` when the key is already present, else the next id.
    pub fn add(&mut self, hash: u64, id: Option<usize>) {
        if self.rehashidx.is_some() {
            self.rehash_step();
        }
        self.expand_if_needed();
        if let Some(id) = id {
            debug_assert_eq!(id, self.next.len());
            let ti = usize::from(self.rehashidx.is_some());
            let b = hash as usize & (self.heads[ti].len() - 1);
            self.next.push(self.heads[ti][b]);
            self.hash.push(hash);
            self.heads[ti][b] = id as u32;
            self.used[ti] += 1;
        }
    }

    /// `_dictExpandIfNeeded`.
    fn expand_if_needed(&mut self) {
        if self.rehashidx.is_some() {
            return;
        }
        if self.heads[0].is_empty() {
            self.expand(INITIAL);
        } else if self.used[0] >= self.heads[0].len() {
            self.expand(self.used[0] * 2);
        }
    }

    /// `dictExpand`.
    fn expand(&mut self, size: usize) {
        if self.rehashidx.is_some() || self.used[0] > size {
            return;
        }
        let real = size.max(INITIAL).next_power_of_two();
        if real == self.heads[0].len() {
            return;
        }
        if self.heads[0].is_empty() {
            self.heads[0] = vec![NIL; real];
        } else {
            self.heads[1] = vec![NIL; real];
            self.rehashidx = Some(0);
        }
    }

    /// `dictRehash(d, 1)`.
    fn rehash_step(&mut self) {
        let Some(mut idx) = self.rehashidx else { return };
        let mut empty_visits = 10;
        if self.used[0] != 0 {
            while self.heads[0][idx] == NIL {
                idx += 1;
                empty_visits -= 1;
                if empty_visits == 0 {
                    self.rehashidx = Some(idx);
                    return;
                }
            }
            let mask = self.heads[1].len() - 1;
            let mut e = std::mem::replace(&mut self.heads[0][idx], NIL);
            while e != NIL {
                let after = self.next[e as usize];
                let b = self.hash[e as usize] as usize & mask;
                self.next[e as usize] = self.heads[1][b];
                self.heads[1][b] = e;
                self.used[0] -= 1;
                self.used[1] += 1;
                e = after;
            }
            idx += 1;
        }
        self.rehashidx = Some(idx);
        if self.used[0] == 0 {
            self.heads[0] = std::mem::take(&mut self.heads[1]);
            self.used = [self.used[1], 0];
            self.rehashidx = None;
        }
    }

    /// `dictNext` from a fresh iterator: `ht[0]`'s buckets, then `ht[1]`'s while rehashing.
    pub fn ids(&self) -> impl Iterator<Item = usize> + '_ {
        let tables = if self.rehashidx.is_some() { 2 } else { 1 };
        self.heads[..tables].iter().flatten().flat_map(move |&head| {
            std::iter::successors((head != NIL).then_some(head), move |&e| {
                let n = self.next[e as usize];
                (n != NIL).then_some(n)
            })
            .map(|e| e as usize)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward layout (a chain per bucket), kept to check the compact one against.
    #[derive(Default)]
    struct Model {
        t: [Vec<Vec<(u64, usize)>>; 2],
        used: [usize; 2],
        rehashidx: Option<usize>,
    }

    impl Model {
        fn add(&mut self, hash: u64, id: Option<usize>) {
            if let Some(mut idx) = self.rehashidx {
                let mut empty = 10;
                let mut done = false;
                if self.used[0] != 0 {
                    while self.t[0][idx].is_empty() {
                        idx += 1;
                        empty -= 1;
                        if empty == 0 {
                            self.rehashidx = Some(idx);
                            done = true;
                            break;
                        }
                    }
                    if !done {
                        let mask = self.t[1].len() - 1;
                        for e in std::mem::take(&mut self.t[0][idx]) {
                            self.t[1][e.0 as usize & mask].insert(0, e);
                            self.used[0] -= 1;
                            self.used[1] += 1;
                        }
                        idx += 1;
                    }
                }
                if !done {
                    self.rehashidx = Some(idx);
                    if self.used[0] == 0 {
                        self.t[0] = std::mem::take(&mut self.t[1]);
                        self.used = [self.used[1], 0];
                        self.rehashidx = None;
                    }
                }
            }
            if self.rehashidx.is_none() {
                let size = if self.t[0].is_empty() {
                    Some(INITIAL)
                } else if self.used[0] >= self.t[0].len() {
                    Some(self.used[0] * 2)
                } else {
                    None
                };
                if let Some(size) = size {
                    let real = size.max(INITIAL).next_power_of_two();
                    if real != self.t[0].len() {
                        if self.t[0].is_empty() {
                            self.t[0] = vec![Vec::new(); real];
                        } else {
                            self.t[1] = vec![Vec::new(); real];
                            self.rehashidx = Some(0);
                        }
                    }
                }
            }
            if let Some(id) = id {
                let ti = usize::from(self.rehashidx.is_some());
                let mask = self.t[ti].len() - 1;
                self.t[ti][hash as usize & mask].insert(0, (hash, id));
                self.used[ti] += 1;
            }
        }

        fn ids(&self) -> Vec<usize> {
            let tables = if self.rehashidx.is_some() { 2 } else { 1 };
            self.t[..tables].iter().flatten().flatten().map(|&(_, id)| id).collect()
        }
    }

    #[test]
    fn iterates_by_bucket_heads_first() {
        let mut d = HashDict::default();
        // Four buckets: 1 and 5 share bucket 1, the later one at its head.
        for (i, h) in [1u64, 2, 5].into_iter().enumerate() {
            d.add(h, Some(i));
        }
        assert_eq!(d.ids().collect::<Vec<_>>(), vec![2, 0, 1]);
    }

    #[test]
    fn same_layout_as_the_model() {
        let (mut d, mut m) = (HashDict::default(), Model::default());
        let mut x = 7u64;
        let mut n = 0;
        for _ in 0..5000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let id = (x >> 60 != 0).then_some(n);
            if id.is_some() {
                n += 1;
            }
            d.add(x >> 3, id);
            m.add(x >> 3, id);
            assert_eq!(d.ids().collect::<Vec<_>>(), m.ids());
        }
    }
}
