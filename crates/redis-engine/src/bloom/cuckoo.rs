//! Cuckoo filters (RedisBloom's `CuckooFilter`): 8-bit fingerprints in buckets of
//! `bucket_size` slots, with sub-filters added as it fills (each `expansion` times larger).

use super::hash::murmur64a;
use super::sb::{MAX_ALLOC, alloc};

/// `CF_MAX_NUM_BUCKETS`: bucket counts are 56-bit fields.
pub const MAX_BUCKETS: u64 = 0x00FF_FFFF_FFFF_FFFF;
/// The serialized header: items, buckets, deletes, filters, bucket size, max iterations,
/// expansion.
pub const HEADER: usize = 8 * 4 + 2 * 3;

#[derive(Clone, Debug, PartialEq)]
pub struct SubFilter {
    pub num_buckets: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cuckoo {
    pub num_buckets: u64,
    pub num_items: u64,
    pub num_deletes: u64,
    pub bucket_size: u16,
    pub max_iterations: u16,
    pub expansion: u16,
    pub filters: Vec<SubFilter>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    Inserted,
    Exists,
    NoSpace,
    MemAllocFailed,
}

#[derive(Clone, Copy)]
struct Lookup {
    h1: u64,
    h2: u64,
    fp: u8,
}

/// `CUCKOO_GEN_HASH`.
pub fn hash(item: &[u8]) -> u64 {
    murmur64a(item, 0)
}

fn next_pow2(n: u64) -> u64 {
    let mut n = n.wrapping_sub(1);
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    n |= n >> 32;
    n.wrapping_add(1)
}

fn alt_hash(fp: u8, index: u64) -> u64 {
    index ^ (fp as u64).wrapping_mul(0x5bd1e995)
}

fn lookup(hash: u64) -> Lookup {
    let fp = (hash % 255 + 1) as u8;
    Lookup { h1: hash, h2: alt_hash(fp, hash), fp }
}

/// `pow(expansion, n)` as a C `size_t` (aarch64 conversions saturate, like `as`).
fn growth(expansion: u16, n: usize) -> u64 {
    libm::pow(expansion as f64, n as f64) as u64
}

impl SubFilter {
    /// `SubCF_GetIndex`: the first slot of the hash's bucket (a 32-bit result).
    fn index(&self, hash: u64, bucket_size: u16) -> usize {
        ((hash % self.num_buckets).wrapping_mul(bucket_size as u64) as u32) as usize
    }

    fn bucket(&self, at: usize, bucket_size: u16) -> &[u8] {
        &self.data[at..at + bucket_size as usize]
    }

    fn find(&self, p: &Lookup, bs: u16) -> bool {
        self.bucket(self.index(p.h1, bs), bs).contains(&p.fp)
            || self.bucket(self.index(p.h2, bs), bs).contains(&p.fp)
    }

    fn count(&self, p: &Lookup, bs: u16) -> u64 {
        let c = |at: usize| self.bucket(at, bs).iter().filter(|&&x| x == p.fp).count() as u64;
        c(self.index(p.h1, bs)) + c(self.index(p.h2, bs))
    }

    fn delete(&mut self, p: &Lookup, bs: u16) -> bool {
        for h in [p.h1, p.h2] {
            let at = self.index(h, bs);
            if let Some(slot) = self.data[at..at + bs as usize].iter_mut().find(|x| **x == p.fp) {
                *slot = 0;
                return true;
            }
        }
        false
    }

    /// `Filter_FindAvailable`: an empty slot in either bucket.
    fn available(&self, p: &Lookup, bs: u16) -> Option<usize> {
        for h in [p.h1, p.h2] {
            let at = self.index(h, bs);
            if let Some(i) = self.bucket(at, bs).iter().position(|&x| x == 0) {
                return Some(at + i);
            }
        }
        None
    }
}

impl Cuckoo {
    /// `CuckooFilter_Init`; `None` when the first filter can't be allocated.
    pub fn new(capacity: u64, bucket_size: u16, max_iterations: u16, expansion: u16) -> Option<Cuckoo> {
        let mut num_buckets = next_pow2(capacity / bucket_size as u64);
        if num_buckets == 0 {
            num_buckets = 1;
        }
        let mut cf = Cuckoo {
            num_buckets,
            num_items: 0,
            num_deletes: 0,
            bucket_size,
            max_iterations,
            expansion: next_pow2(expansion as u64) as u16,
            filters: Vec::new(),
        };
        cf.grow().then_some(cf)
    }

    /// `CuckooFilter_Grow`.
    fn grow(&mut self) -> bool {
        if self.filters.len() >= u16::MAX as usize {
            return false;
        }
        let g = growth(self.expansion, self.filters.len());
        if g > MAX_BUCKETS / self.num_buckets {
            return false;
        }
        let num_buckets = self.num_buckets * g;
        let Some(bytes) = num_buckets.checked_mul(self.bucket_size as u64) else { return false };
        if bytes > MAX_ALLOC {
            return false;
        }
        let Some(data) = alloc(bytes) else { return false };
        self.filters.push(SubFilter { num_buckets, data });
        true
    }

    pub fn check(&self, hash: u64) -> bool {
        let p = lookup(hash);
        self.filters.iter().any(|f| f.find(&p, self.bucket_size))
    }

    pub fn count(&self, hash: u64) -> u64 {
        let p = lookup(hash);
        self.filters.iter().map(|f| f.count(&p, self.bucket_size)).sum()
    }

    /// `CuckooFilter_Delete`.
    pub fn delete(&mut self, hash: u64) -> bool {
        let p = lookup(hash);
        let bs = self.bucket_size;
        for i in (0..self.filters.len()).rev() {
            if self.filters[i].delete(&p, bs) {
                self.num_items = self.num_items.wrapping_sub(1);
                self.num_deletes += 1;
                if self.filters.len() > 1 && self.num_deletes as f64 > self.num_items as f64 * 0.10 {
                    self.compact(false);
                }
                return true;
            }
        }
        false
    }

    pub fn insert(&mut self, hash: u64) -> Insert {
        self.insert_fp(&lookup(hash))
    }

    pub fn insert_unique(&mut self, hash: u64) -> Insert {
        let p = lookup(hash);
        if self.filters.iter().any(|f| f.find(&p, self.bucket_size)) {
            return Insert::Exists;
        }
        self.insert_fp(&p)
    }

    fn insert_fp(&mut self, p: &Lookup) -> Insert {
        loop {
            let bs = self.bucket_size;
            for f in self.filters.iter_mut().rev() {
                if let Some(slot) = f.available(p, bs) {
                    f.data[slot] = p.fp;
                    self.num_items += 1;
                    return Insert::Inserted;
                }
            }
            if self.ko_insert(p) {
                self.num_items += 1;
                return Insert::Inserted;
            }
            if self.expansion == 0 {
                return Insert::NoSpace;
            }
            if !self.grow() {
                return Insert::MemAllocFailed;
            }
        }
    }

    /// `Filter_KOInsert`: kick fingerprints around the last filter; undone when it fails.
    fn ko_insert(&mut self, p: &Lookup) -> bool {
        let max = self.max_iterations;
        let bs = self.bucket_size as u64;
        let f = self.filters.last_mut().expect("a cuckoo filter has a sub-filter");
        let n = f.num_buckets;
        let mut fp = p.fp;
        let mut victim = 0u64;
        let mut ii = p.h1 % n;
        for _ in 0..max {
            let at = (ii.wrapping_mul(bs) + victim) as usize;
            std::mem::swap(&mut f.data[at], &mut fp);
            ii = alt_hash(fp, ii) % n;
            let start = ii.wrapping_mul(bs) as usize;
            if let Some(i) = f.data[start..start + bs as usize].iter().position(|&x| x == 0) {
                f.data[start + i] = fp;
                return true;
            }
            victim = (victim + 1) % bs;
        }
        for _ in 0..max {
            victim = (victim + bs - 1) % bs;
            ii = alt_hash(fp, ii) % n;
            let at = (ii.wrapping_mul(bs) + victim) as usize;
            std::mem::swap(&mut f.data[at], &mut fp);
        }
        false
    }

    /// `CuckooFilter_Compact`: move fingerprints into older filters, dropping emptied last
    /// filters. With `cont`, keep going past a filter that couldn't be emptied.
    pub fn compact(&mut self, cont: bool) {
        let mut i = self.filters.len();
        while i > 1 {
            i -= 1;
            if !self.compact_single(i) && !cont {
                break;
            }
        }
        self.num_deletes = 0;
    }

    fn compact_single(&mut self, ix: usize) -> bool {
        let bs = self.bucket_size;
        let (older, rest) = self.filters.split_at_mut(ix);
        let cur = &mut rest[0];
        let mut ok = true;
        for bucket in 0..cur.num_buckets {
            for slot in 0..bs as u64 {
                let at = (bucket * bs as u64 + slot) as usize;
                let fp = cur.data[at];
                if fp == 0 {
                    continue;
                }
                let p = Lookup { h1: bucket, h2: alt_hash(fp, bucket), fp };
                let mut moved = false;
                for f in older.iter_mut() {
                    if let Some(s) = f.available(&p, bs) {
                        f.data[s] = fp;
                        cur.data[at] = 0;
                        moved = true;
                        break;
                    }
                }
                if !moved {
                    ok = false;
                }
            }
        }
        if ok && ix == self.filters.len() - 1 {
            self.filters.pop();
        }
        ok
    }

    /// `CFMemUsage`: the filter, its sub-filter array and the buckets.
    pub fn mem_usage(&self) -> u64 {
        40 + 16 * self.filters.len() as u64
            + self.filters.iter().map(|f| f.data.len() as u64).sum::<u64>()
    }

    /// `fillCFHeader`.
    pub fn header(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER);
        out.extend_from_slice(&self.num_items.to_le_bytes());
        out.extend_from_slice(&self.num_buckets.to_le_bytes());
        out.extend_from_slice(&self.num_deletes.to_le_bytes());
        out.extend_from_slice(&(self.filters.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.bucket_size.to_le_bytes());
        out.extend_from_slice(&self.max_iterations.to_le_bytes());
        out.extend_from_slice(&self.expansion.to_le_bytes());
        out
    }

    /// `CuckooFilter_ValidateIntegrity` for `filters` sub-filters.
    pub fn valid_params(&self, filters: u64) -> bool {
        (1..=255).contains(&self.bucket_size)
            && (1..=65536).contains(&filters)
            && self.max_iterations >= 1
            && self.expansion <= 32768
            && !(self.expansion == 0 && filters > 1)
            && self.num_buckets != 0
            && self.num_buckets <= MAX_BUCKETS
            && self.num_buckets.is_power_of_two()
    }

    /// `CFHeader_Load`.
    pub fn from_header(h: &[u8]) -> Option<Cuckoo> {
        if h.len() != HEADER {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().expect("8 bytes"));
        let u16_at = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]);
        let filters = u64_at(24) as u16 as u64;
        let mut cf = Cuckoo {
            num_items: u64_at(0),
            num_buckets: u64_at(8),
            num_deletes: u64_at(16),
            bucket_size: u16_at(32),
            max_iterations: u16_at(34),
            expansion: u16_at(36),
            filters: Vec::new(),
        };
        if !cf.valid_params(filters) {
            return None;
        }
        for i in 0..filters as usize {
            let nb = ((cf.num_buckets as f64 * libm::pow(cf.expansion as f64, i as f64)) as u64) & MAX_BUCKETS;
            if nb == 0 {
                return None;
            }
            let bytes = nb.checked_mul(cf.bucket_size as u64)?;
            cf.filters.push(SubFilter { num_buckets: nb, data: alloc(bytes)? });
        }
        Some(cf)
    }

    /// `CF_GetEncodedChunk`: the next position and the chunk (`None` past the end).
    pub fn chunk(&self, pos: i64, limit: u64) -> Option<(i64, &[u8])> {
        let mut offset = pos - 1;
        for f in &self.filters {
            let size = f.data.len() as i64;
            if offset < size {
                if size as u64 <= limit {
                    return Some((pos + size, &f.data));
                }
                let len = ((size - offset) as u64).min(limit) as i64;
                return Some((pos + len, &f.data[offset as usize..(offset + len) as usize]));
            }
            offset -= size;
        }
        None
    }

    /// `CF_LoadEncodedChunk`.
    pub fn load_chunk(&mut self, pos: i64, data: &[u8]) -> bool {
        let len = data.len() as i64;
        if data.is_empty() || pos <= 0 || ((pos - 1) as u64) < data.len() as u64 {
            return false;
        }
        let mut offset = pos - len - 1;
        let mut target = None;
        for (i, f) in self.filters.iter().enumerate() {
            target = Some(i);
            let size = f.data.len() as i64;
            if offset < size {
                break;
            }
            offset -= size;
        }
        let Some(i) = target else { return false };
        let f = &mut self.filters[i];
        if (f.data.len() as i64) < offset + len {
            return false;
        }
        f.data[offset as usize..(offset + len) as usize].copy_from_slice(data);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_check_delete() {
        let mut cf = Cuckoo::new(1024, 2, 20, 1).unwrap();
        assert_eq!(cf.num_buckets, 512);
        for i in 0..100 {
            assert_eq!(cf.insert(hash(format!("k{i}").as_bytes())), Insert::Inserted);
        }
        assert!(cf.check(hash(b"k5")));
        assert_eq!(cf.insert_unique(hash(b"k5")), Insert::Exists);
        assert_eq!(cf.count(hash(b"k5")), 1);
        assert!(cf.delete(hash(b"k5")));
        assert!(!cf.delete(hash(b"k5")));
        assert_eq!(cf.num_items, 99);
        assert_eq!(cf.mem_usage(), 40 + 16 + 1024);
    }

    #[test]
    fn grows_and_round_trips() {
        let mut cf = Cuckoo::new(4, 2, 20, 2).unwrap();
        for i in 0..64 {
            assert_eq!(cf.insert(hash(format!("{i}").as_bytes())), Insert::Inserted);
        }
        assert!(cf.filters.len() > 1);
        let mut copy = Cuckoo::from_header(&cf.header()).unwrap();
        let mut pos = 1;
        while let Some((next, data)) = cf.chunk(pos, 3) {
            assert!(copy.load_chunk(next, data));
            pos = next;
        }
        assert_eq!(copy, cf);
    }

    #[test]
    fn full_without_expansion() {
        let mut cf = Cuckoo::new(4, 2, 5, 0).unwrap();
        let results: Vec<Insert> = (0..20).map(|i| cf.insert(hash(format!("{i}").as_bytes()))).collect();
        assert!(results.contains(&Insert::NoSpace));
    }
}
