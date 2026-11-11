//! What pgvector's memory limits are measured against, reproduced: the sizes PostgreSQL's
//! memory contexts allocate (`aset.c`, `generation.c`) for the allocations hnsw makes, and
//! simplehash's tid set (`tidhash`, `lib/simplehash.h`), whose growth is part of a scan's
//! memory. Only sizes are kept, not the memory: `MemoryContextMemAllocated` is what an
//! iterative scan (work_mem * hnsw.scan_mem_multiplier) and a build (maintenance_work_mem)
//! compare with their limits, so where their switch happens depends on these sums.
//!
//! x86-64 sizes: MAXALIGN 8, an 8-byte chunk header (MemoryChunk).

/// MAXALIGN.
pub(super) const fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

/// sizeof(MemoryContextData): the common context header.
const CONTEXT_HDR: usize = 80;
/// MAXALIGN(sizeof(AllocSetContext)).
const ASET_CONTEXT: usize = maxalign(CONTEXT_HDR + 8 + 11 * 8 + 4 * 4 + 4);
/// ALLOC_BLOCKHDRSZ.
const ASET_BLOCKHDR: usize = 40;
/// sizeof(MemoryChunk).
const CHUNKHDR: usize = 8;
/// ALLOC_MINBITS.
const MINBITS: u32 = 3;
/// ALLOC_CHUNK_LIMIT.
const ASET_CHUNK_LIMIT: usize = 1 << (11 - 1 + MINBITS);

/// A chunk of an `AllocSet`: on a freelist (by index) when freed, or a block of its own.
#[derive(Clone, Copy, Debug)]
pub(super) enum Chunk {
    Small(u8),
    Large(usize),
}

/// The sizes of an AllocSet context (`aset.c`).
pub(super) struct AllocSet {
    /// mem_allocated.
    pub mem: usize,
    /// Free bytes at the end of the active block.
    avail: usize,
    next_block: usize,
    max_block: usize,
    chunk_limit: usize,
    /// How many free chunks each freelist holds.
    free: [u32; 11],
}

/// AllocSetFreeIndex.
fn free_index(size: usize) -> usize {
    if size > 1 << MINBITS { (usize::BITS - (size - 1).leading_zeros()) as usize - MINBITS as usize } else { 0 }
}

impl AllocSet {
    /// AllocSetContextCreate (a fresh context, or one reset: only its keeper block).
    pub fn new(min_context: usize, init_block: usize, max_block: usize) -> AllocSet {
        let mut first = ASET_CONTEXT + ASET_BLOCKHDR + CHUNKHDR;
        first = if min_context != 0 { first.max(min_context) } else { first.max(init_block) };
        let mut chunk_limit = ASET_CHUNK_LIMIT;
        while chunk_limit + CHUNKHDR > (max_block - ASET_BLOCKHDR) / 4 {
            chunk_limit >>= 1;
        }
        AllocSet { mem: first, avail: first - ASET_CONTEXT - ASET_BLOCKHDR, next_block: init_block, max_block, chunk_limit, free: [0; 11] }
    }

    /// AllocSetAlloc.
    pub fn alloc(&mut self, size: usize) -> Chunk {
        if size > self.chunk_limit {
            // AllocSetAllocLarge
            let blk = maxalign(size) + ASET_BLOCKHDR + CHUNKHDR;
            self.mem += blk;
            return Chunk::Large(blk);
        }
        let fidx = free_index(size);
        if self.free[fidx] > 0 {
            self.free[fidx] -= 1;
            return Chunk::Small(fidx as u8);
        }
        let chunk = 8usize << fidx;
        if self.avail < chunk + CHUNKHDR {
            // AllocSetAllocFromNewBlock: the old block's rest goes to the freelists.
            while self.avail >= (1 << MINBITS) + CHUNKHDR {
                let mut availchunk = self.avail - CHUNKHDR;
                let mut a = free_index(availchunk);
                if availchunk != 8usize << a {
                    a -= 1;
                    availchunk = 8usize << a;
                }
                self.avail -= availchunk + CHUNKHDR;
                self.free[a] += 1;
            }
            let mut blk = self.next_block;
            self.next_block = (self.next_block << 1).min(self.max_block);
            let required = chunk + ASET_BLOCKHDR + CHUNKHDR;
            while blk < required {
                blk <<= 1;
            }
            self.mem += blk;
            self.avail = blk - ASET_BLOCKHDR;
        }
        self.avail -= chunk + CHUNKHDR;
        Chunk::Small(fidx as u8)
    }

    /// AllocSetFree.
    pub fn free(&mut self, c: Chunk) {
        match c {
            Chunk::Small(f) => self.free[f as usize] += 1,
            Chunk::Large(blk) => self.mem -= blk,
        }
    }

    /// AllocSetFree of a chunk of `size` (not a block of its own).
    pub fn free_size(&mut self, size: usize) {
        self.free[free_index(size)] += 1;
    }

    /// AllocSetRealloc.
    pub fn realloc(&mut self, c: Chunk, size: usize) -> Chunk {
        match c {
            Chunk::Large(blk) => {
                let nb = maxalign(size) + ASET_BLOCKHDR + CHUNKHDR;
                self.mem = self.mem - blk + nb;
                Chunk::Large(nb)
            }
            Chunk::Small(f) if 8usize << f >= size => c,
            Chunk::Small(_) => {
                let n = self.alloc(size);
                self.free(c);
                n
            }
        }
    }
}

/// A `List` (`pg_list.h`) as its memory goes: the header with its initial cells, and the
/// cells array it moves to when it outgrows them (`new_list`, `enlarge_list`).
#[derive(Default)]
pub(super) struct ListMem {
    head: Option<Chunk>,
    cells: Option<Chunk>,
    pub len: usize,
    max: usize,
}

/// offsetof(List, initial_elements).
const LIST_HDR: usize = 24;
/// LIST_HEADER_OVERHEAD.
const LIST_OVERHEAD: usize = (LIST_HDR - 1) / 8 + 1;

impl ListMem {
    /// lappend.
    pub fn append(&mut self, m: &mut AllocSet) {
        if self.head.is_none() {
            let max = (8usize).max(1 + LIST_OVERHEAD).next_power_of_two() - LIST_OVERHEAD;
            self.head = Some(m.alloc(LIST_HDR + max * 8));
            self.max = max;
            self.len = 1;
            return;
        }
        if self.len == self.max {
            let n = (16usize).max(self.len + 1).next_power_of_two();
            self.cells = Some(match self.cells {
                None => m.alloc(n * 8),
                Some(c) => m.realloc(c, n * 8),
            });
            self.max = n;
        }
        self.len += 1;
    }

    /// list_delete_last (freeing the list when it was the last cell).
    pub fn delete_last(&mut self, m: &mut AllocSet) {
        if self.len <= 1 {
            self.free(m);
        } else {
            self.len -= 1;
        }
    }

    /// list_free.
    pub fn free(&mut self, m: &mut AllocSet) {
        if let Some(c) = self.cells.take() {
            m.free(c);
        }
        if let Some(h) = self.head.take() {
            m.free(h);
        }
        *self = ListMem::default();
    }
}

/// MAXALIGN(sizeof(GenerationContext)).
const GEN_CONTEXT: usize = maxalign(CONTEXT_HDR + 4 * 4 + 8 + 8 + 16);
/// Generation_BLOCKHDRSZ.
const GEN_BLOCKHDR: usize = maxalign(16 + 8 + 8 + 4 + 4 + 8 + 8);

/// A Generation context (`generation.c`) that only allocates (hnsw's build graph): its
/// mem_allocated, and where each chunk lands (which block, what offset), which orders the
/// elements' addresses: pgvector breaks ties between equal distances by element address.
pub(super) struct Generation {
    pub mem: usize,
    block: u32,
    off: usize,
    free: usize,
    next_block: usize,
    max_block: usize,
    chunk_limit: usize,
}

impl Generation {
    /// GenerationContextCreate.
    pub fn new(min_context: usize, init_block: usize, max_block: usize) -> Generation {
        let mut alloc = GEN_CONTEXT + GEN_BLOCKHDR + CHUNKHDR;
        alloc = if min_context != 0 { alloc.max(min_context) } else { alloc.max(init_block) };
        let first = alloc - GEN_CONTEXT;
        let mut chunk_limit = max_block.min((1 << 30) - 1);
        while chunk_limit + CHUNKHDR > (max_block - GEN_BLOCKHDR) / 8 {
            chunk_limit >>= 1;
        }
        Generation { mem: first, block: 0, off: GEN_CONTEXT + GEN_BLOCKHDR, free: first - GEN_BLOCKHDR, next_block: init_block, max_block, chunk_limit }
    }

    /// GenerationAlloc: the chunk's place, as (block number, offset in the block).
    pub fn alloc(&mut self, size: usize) -> (u32, usize) {
        let chunk = maxalign(size);
        let required = chunk + CHUNKHDR;
        if chunk > self.chunk_limit {
            // GenerationAllocLarge: a block of its own (the current one stays current).
            self.mem += required + GEN_BLOCKHDR;
            return (u32::MAX, 0);
        }
        if self.free < required {
            // GenerationAllocFromNewBlock
            let mut blk = self.next_block;
            self.next_block = (self.next_block << 1).min(self.max_block);
            if blk < required + GEN_BLOCKHDR {
                blk = (required + GEN_BLOCKHDR).next_power_of_two();
            }
            self.mem += blk;
            self.block += 1;
            self.off = GEN_BLOCKHDR;
            self.free = blk - GEN_BLOCKHDR;
        }
        let at = (self.block, self.off + CHUNKHDR);
        self.off += required;
        self.free -= required;
        at
    }
}

/// murmurhash64 (`common/hashfn.h`).
#[inline(always)]
pub(super) fn murmurhash64(data: u64) -> u64 {
    let mut h = data;
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
    h ^= h >> 33;
    h
}

/// hash_tid: the ItemPointerData's six bytes (bi_hi, bi_lo, posid) as a zero-padded uint64.
#[inline(always)]
fn hash_tid(tid: u64) -> u32 {
    murmurhash64(tid) as u32
}

/// A tid as hash_tid reads its bytes.
#[inline(always)]
pub(super) fn tid_key(blkno: u32, offno: u16) -> u64 {
    (blkno >> 16) as u64 | ((blkno & 0xffff) as u64) << 16 | (offno as u64) << 32
}

/// No entry (a key no tid has: posid 0xFFFF, block 0xFFFFFFFF).
const EMPTY: u64 = u64::MAX;

/// simplehash's `tidhash`: a set of index tids, inserted as simplehash inserts (robin hood
/// with its growth rules), its memory taken from an `AllocSet`.
pub(super) struct TidHash {
    data: Vec<u64>,
    sizemask: u32,
    members: u32,
    grow_threshold: u32,
    chunk: Chunk,
}

/// sizeof(TidHashEntry).
const TIDHASH_ENTRY: usize = 8;
/// sizeof(tidhash_hash).
const TIDHASH_HDR: usize = 48;

/// SH_COMPUTE_SIZE.
fn compute_size(n: u64) -> u64 {
    n.max(2).next_power_of_two()
}

impl TidHash {
    /// tidhash_create.
    pub fn create(m: &mut AllocSet, nelements: u32) -> TidHash {
        m.alloc(TIDHASH_HDR);
        let size = compute_size(((nelements as f64) / 0.9).min((u32::MAX as u64 + 1) as f64) as u64);
        let chunk = m.alloc(TIDHASH_ENTRY * size as usize);
        let mut t = TidHash { data: vec![EMPTY; size as usize], sizemask: 0, members: 0, grow_threshold: 0, chunk };
        t.update_parameters(size);
        t
    }

    /// SH_UPDATE_PARAMETERS.
    fn update_parameters(&mut self, size: u64) {
        self.sizemask = (size - 1) as u32;
        self.grow_threshold = if size == u32::MAX as u64 + 1 { (size as f64 * 0.98) as u32 } else { (size as f64 * 0.9) as u32 };
    }

    fn size(&self) -> u64 {
        self.sizemask as u64 + 1
    }

    #[inline(always)]
    fn bucket(&self, hash: u32) -> u32 {
        hash & self.sizemask
    }

    #[inline(always)]
    fn dist(&self, optimal: u32, bucket: u32) -> u32 {
        if optimal <= bucket { bucket - optimal } else { (self.size() as u32).wrapping_add(bucket).wrapping_sub(optimal) }
    }

    /// SH_GROW.
    fn grow(&mut self, m: &mut AllocSet, newsize: u64) {
        let oldsize = self.size() as usize;
        let newsize = compute_size(newsize);
        let chunk = m.alloc(TIDHASH_ENTRY * newsize as usize);
        let old = std::mem::replace(&mut self.data, vec![EMPTY; newsize as usize]);
        self.update_parameters(newsize);
        // Start at an element that is empty or at its optimal place, so that no element
        // wraps around ahead of where it belongs.
        let mut start = 0;
        for (i, &k) in old.iter().enumerate() {
            if k == EMPTY || (hash_tid(k) & (oldsize as u32 - 1)) == i as u32 {
                start = i;
                break;
            }
        }
        let mut copy = start;
        for _ in 0..oldsize {
            let k = old[copy];
            if k != EMPTY {
                let mut cur = self.bucket(hash_tid(k));
                while self.data[cur as usize] != EMPTY {
                    cur = (cur + 1) & self.sizemask;
                }
                self.data[cur as usize] = k;
            }
            copy += 1;
            if copy >= oldsize {
                copy = 0;
            }
        }
        m.free(std::mem::replace(&mut self.chunk, chunk));
    }

    /// tidhash_insert: whether the tid was there already.
    pub fn insert(&mut self, m: &mut AllocSet, key: u64) -> bool {
        let hash = hash_tid(key);
        'restart: loop {
            let mut insertdist = 0u32;
            if self.members >= self.grow_threshold {
                self.grow(m, self.size() * 2);
            }
            let start = self.bucket(hash);
            let mut cur = start;
            loop {
                let k = self.data[cur as usize];
                if k == EMPTY {
                    self.members += 1;
                    self.data[cur as usize] = key;
                    return false;
                }
                if k == key {
                    return true;
                }
                let curdist = self.dist(self.bucket(hash_tid(k)), cur);
                if insertdist > curdist {
                    // Robin hood: shift the run up to the next empty slot by one.
                    let mut empty = cur;
                    let mut emptydist = 0;
                    loop {
                        empty = (empty + 1) & self.sizemask;
                        if self.data[empty as usize] == EMPTY {
                            break;
                        }
                        emptydist += 1;
                        if emptydist > 150 && (self.members as f64 / self.size() as f64) >= 0.1 {
                            self.grow_threshold = 0;
                            continue 'restart;
                        }
                    }
                    let mut mv = empty;
                    while mv != cur {
                        let prev = mv.wrapping_sub(1) & self.sizemask;
                        self.data[mv as usize] = self.data[prev as usize];
                        mv = prev;
                    }
                    self.members += 1;
                    self.data[cur as usize] = key;
                    return false;
                }
                cur = (cur + 1) & self.sizemask;
                insertdist += 1;
                if insertdist > 25 && (self.members as f64 / self.size() as f64) >= 0.1 {
                    self.grow_threshold = 0;
                    continue 'restart;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aset_sizes() {
        let mut m = AllocSet::new(0, 8 * 1024, 256 * 1024);
        assert_eq!(m.mem, 8192);
        // The keeper block holds 7952 bytes: 58 chunks of 136 bytes, then a new 8 KB block.
        for _ in 0..58 {
            m.alloc(128);
        }
        assert_eq!(m.mem, 8192);
        m.alloc(128);
        assert_eq!(m.mem, 8192 + 8192);
        let big = m.alloc(16384);
        assert_eq!(m.mem, 16384 + 16384 + 48);
        m.free(big);
        assert_eq!(m.mem, 16384);
    }

    #[test]
    fn tidhash_set() {
        let mut m = AllocSet::new(0, 8 * 1024, 256 * 1024);
        let mut t = TidHash::create(&mut m, 4);
        for b in 0..1000u32 {
            assert!(!t.insert(&mut m, tid_key(b, 1)));
        }
        for b in 0..1000u32 {
            assert!(t.insert(&mut m, tid_key(b, 1)));
        }
        assert_eq!(t.members, 1000);
        assert_eq!(t.size(), 2048);
    }
}
