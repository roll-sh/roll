//! klib's `khash` for 64-bit keys (`KHASH_MAP_INIT_INT64`), as RediSearch's grouper keeps its
//! groups in one: open addressing with quadratic probing, the 0.77 load bound, and the in-place
//! "kick-out" rehash. Only the layout matters here (puts and bucket-order iteration), since that
//! is the order the grouper yields its groups in.

const UPPER: f64 = 0.77;

/// Bucket states (`__ac_isempty` / `__ac_isdel`; neither = occupied).
const EMPTY: u8 = 2;
const DEL: u8 = 1;
const FULL: u8 = 0;

#[derive(Clone, Debug, Default)]
pub struct KhU64 {
    n_buckets: u32,
    size: u32,
    n_occupied: u32,
    upper_bound: u32,
    keys: Vec<u64>,
    flags: Vec<u8>,
}

/// `kh_int64_hash_func`.
fn hash(key: u64) -> u32 {
    ((key >> 33) ^ key ^ (key << 11)) as u32
}

/// `kroundup32`.
fn roundup32(x: u32) -> u32 {
    let mut x = x.wrapping_sub(1);
    x |= x >> 1;
    x |= x >> 2;
    x |= x >> 4;
    x |= x >> 8;
    x |= x >> 16;
    x.wrapping_add(1)
}

impl KhU64 {
    pub fn len(&self) -> usize {
        self.size as usize
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// `kh_resize`.
    fn resize(&mut self, new_n_buckets: u32) {
        let mut new_n = roundup32(new_n_buckets);
        if new_n < 4 {
            new_n = 4;
        }
        if self.size >= (new_n as f64 * UPPER + 0.5) as u32 {
            return; // requested size is too small
        }
        let mut new_flags = vec![EMPTY; new_n as usize];
        if self.n_buckets < new_n {
            self.keys.resize(new_n as usize, 0);
        }
        let new_mask = new_n - 1;
        for j in 0..self.n_buckets {
            if self.flags[j as usize] != FULL {
                continue;
            }
            let mut key = self.keys[j as usize];
            self.flags[j as usize] = DEL;
            loop {
                // the kick-out process, sort of like in Cuckoo hashing
                let mut step = 0u32;
                let mut i = hash(key) & new_mask;
                while new_flags[i as usize] != EMPTY {
                    step += 1;
                    i = i.wrapping_add(step) & new_mask;
                }
                new_flags[i as usize] = FULL;
                if i < self.n_buckets && self.flags[i as usize] == FULL {
                    std::mem::swap(&mut self.keys[i as usize], &mut key);
                    self.flags[i as usize] = DEL;
                } else {
                    self.keys[i as usize] = key;
                    break;
                }
            }
        }
        if self.n_buckets > new_n {
            self.keys.truncate(new_n as usize);
        }
        self.flags = new_flags;
        self.n_buckets = new_n;
        self.n_occupied = self.size;
        self.upper_bound = (new_n as f64 * UPPER + 0.5) as u32;
    }

    /// `kh_put`: inserts `key` if absent. Returns whether it was new.
    pub fn put(&mut self, key: u64) -> bool {
        if self.n_occupied >= self.upper_bound {
            if self.n_buckets > (self.size << 1) {
                self.resize(self.n_buckets - 1); // clear "deleted" elements
            } else {
                self.resize(self.n_buckets + 1); // expand the hash table
            }
        }
        let mask = self.n_buckets - 1;
        let mut x = self.n_buckets;
        let mut site = self.n_buckets;
        let mut i = hash(key) & mask;
        if self.flags[i as usize] == EMPTY {
            x = i;
        } else {
            let last = i;
            let mut step = 0u32;
            while self.flags[i as usize] != EMPTY && (self.flags[i as usize] == DEL || self.keys[i as usize] != key) {
                if self.flags[i as usize] == DEL {
                    site = i;
                }
                step += 1;
                i = i.wrapping_add(step) & mask;
                if i == last {
                    x = site;
                    break;
                }
            }
            if x == self.n_buckets {
                x = if self.flags[i as usize] == EMPTY && site != self.n_buckets { site } else { i };
            }
        }
        match self.flags[x as usize] {
            EMPTY => {
                self.keys[x as usize] = key;
                self.flags[x as usize] = FULL;
                self.size += 1;
                self.n_occupied += 1;
                true
            }
            DEL => {
                self.keys[x as usize] = key;
                self.flags[x as usize] = FULL;
                self.size += 1;
                true
            }
            _ => false,
        }
    }

    /// The keys in bucket order (`kh_begin` .. `kh_end`, `kh_exist`).
    pub fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.n_buckets as usize).filter(|&i| self.flags[i] == FULL).map(|i| self.keys[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_every_key_once_through_resizes() {
        let mut h = KhU64::default();
        let mut x = 0x9e3779b97f4a7c15u64;
        let mut keys = Vec::new();
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            assert!(h.put(x));
            assert!(!h.put(x));
            keys.push(x);
        }
        let mut got: Vec<u64> = h.keys().collect();
        got.sort();
        keys.sort();
        assert_eq!(got, keys);
        assert_eq!(h.len(), 1000);
    }

    #[test]
    fn small_tables_order_by_bucket() {
        // Four buckets until a fourth key: (key ^ key<<11 ^ key>>33) & 3 is the bucket.
        let mut h = KhU64::default();
        for k in [3u64, 1, 2] {
            h.put(k);
        }
        let order: Vec<u64> = h.keys().collect();
        let mut by_bucket = vec![3u64, 1, 2];
        by_bucket.sort_by_key(|&k| hash(k) & 3);
        assert_eq!(order, by_bucket);
    }
}
