//! `RsDict`'s layout for keys the caller hashes itself (RediSearch's value sets, hashed with
//! `RSValue_Hash`), for the sets read back in iteration order (TOLIST). Entries are ids the
//! caller maps to its values and dedupes itself.

#[derive(Clone, Debug, Default)]
pub struct HashDict {
    /// `ht[0]` and `ht[1]`: buckets of (hash, id), each chain head first.
    t: [Vec<Vec<(u64, usize)>>; 2],
    used: [usize; 2],
    rehashidx: Option<usize>,
}

/// `DICT_HT_INITIAL_SIZE`.
const INITIAL: usize = 4;

impl HashDict {
    /// `dictAdd`: the rehash step and expansion happen on every add, the existing keys
    /// included; `id` is `None` when the key is already present.
    pub fn add(&mut self, hash: u64, id: Option<usize>) {
        if self.rehashidx.is_some() {
            self.rehash_step();
        }
        self.expand_if_needed();
        if let Some(id) = id {
            let ti = usize::from(self.rehashidx.is_some());
            let mask = self.t[ti].len() - 1;
            self.t[ti][hash as usize & mask].insert(0, (hash, id));
            self.used[ti] += 1;
        }
    }

    /// `_dictExpandIfNeeded`.
    fn expand_if_needed(&mut self) {
        if self.rehashidx.is_some() {
            return;
        }
        if self.t[0].is_empty() {
            self.expand(INITIAL);
        } else if self.used[0] >= self.t[0].len() {
            self.expand(self.used[0] * 2);
        }
    }

    /// `dictExpand`.
    fn expand(&mut self, size: usize) {
        if self.rehashidx.is_some() || self.used[0] > size {
            return;
        }
        let real = size.max(INITIAL).next_power_of_two();
        if real == self.t[0].len() {
            return;
        }
        let n = vec![Vec::new(); real];
        if self.t[0].is_empty() {
            self.t[0] = n;
        } else {
            self.t[1] = n;
            self.rehashidx = Some(0);
        }
    }

    /// `dictRehash(d, 1)`.
    fn rehash_step(&mut self) {
        let Some(mut idx) = self.rehashidx else { return };
        let mut empty_visits = 10;
        if self.used[0] != 0 {
            while self.t[0][idx].is_empty() {
                idx += 1;
                empty_visits -= 1;
                if empty_visits == 0 {
                    self.rehashidx = Some(idx);
                    return;
                }
            }
            let mask = self.t[1].len() - 1;
            for e in std::mem::take(&mut self.t[0][idx]) {
                self.t[1][e.0 as usize & mask].insert(0, e);
                self.used[0] -= 1;
                self.used[1] += 1;
            }
            idx += 1;
        }
        self.rehashidx = Some(idx);
        if self.used[0] == 0 {
            self.t[0] = std::mem::take(&mut self.t[1]);
            self.used = [self.used[1], 0];
            self.rehashidx = None;
        }
    }

    /// `dictNext` from a fresh iterator: `ht[0]`'s buckets, then `ht[1]`'s while rehashing.
    pub fn ids(&self) -> impl Iterator<Item = usize> + '_ {
        let tables = if self.rehashidx.is_some() { 2 } else { 1 };
        self.t[..tables].iter().flatten().flatten().map(|&(_, id)| id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn keeps_every_id_through_rehashing() {
        let mut d = HashDict::default();
        let mut x = 7u64;
        for i in 0..500 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            d.add(x, Some(i));
            d.add(x, None);
        }
        let mut ids: Vec<usize> = d.ids().collect();
        ids.sort();
        assert_eq!(ids, (0..500).collect::<Vec<_>>());
    }
}
