//! Process-wide block cache shared by every open database, bounded by bytes (CLOCK eviction).
//! Idle databases' blocks simply age out.
//!
//! A hit takes its shard's lock shared (its block's `referenced` an atomic): readers never wait
//! for each other, only for an insert or eviction. (With the hit under the shard's mutex, 32
//! clients on four CPUs spun and yielded behind a reader preempted inside it: 70% of their
//! `sched_yield` stalls, HANDOFF "Point and range reads, round 6".) An evicted block is dropped
//! at once, under the lock: the memory the cache counts is what it holds, its blocks'
//! allocations as the allocator sizes them (`Block::mem_bytes`) and its own tables. (Counting a
//! block's length and struct, a 256 MB cache held 268 MB at TPC-H SF 1.)
//!
//! A block's buffer is in memory the cache keeps (`arena`): a piece of a shard's chunks, which
//! the next insert reuses whichever thread freed it. A shard's arena maps no more than the
//! shard may hold; at that limit a miss's buffer is made by evicting (`BlockCache::buffer`).
//!
//! A bulk read (`storage::bulk_read`: a sequential or parallel scan) fills a cache with room. In
//! a full one its blocks go through a small ring per shard, as PostgreSQL's bulk-read strategy
//! puts a big scan's pages in a ring of shared buffers: a scan's block stays in the cache while
//! the ring holds it, where any reader finds it, and the scan's next block takes the place of
//! its oldest one when nobody else read that one. A ring block somebody else read stays in the
//! cache under CLOCK. (Dropping a full cache's bulk blocks at once, TPC-H q21 at 256 MB read
//! every lineitem block twice a run: its scan, then its EXISTS probes of the same orders.)

mod arena;

use super::sst::Block;
pub(crate) use arena::Buf;
use arena::Arena;
use parking_lot::RwLock;
// (foldhash: a lookup per block read, SipHash was a fifth of it.)
use hashbrown::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type CachedBlock = Arc<Block>;

const SHARDS: usize = 16;
/// Blocks a miss evicts at most to make room for its buffer in a full arena (else its buffer
/// is the heap's).
const EVICT_FOR_ROOM: usize = 16;
/// A shard's ring for bulk reads in a full cache: this part of what the shard holds.
const RING_SHARE: usize = 64;

/// The size the server's allocator (mimalloc) gives an allocation of `n` bytes: 8, then
/// 16-byte steps up to 64, one of four steps between powers of two up to 64 kB, past it whole
/// 4 kB pages.
pub(crate) fn alloc_bytes(n: usize) -> usize {
    match n {
        0..=8 => 8,
        9..=64 => n.next_multiple_of(16),
        65..=0x10000 => n.next_multiple_of(1 << (usize::BITS - 3 - (n - 1).leading_zeros())),
        _ => n.next_multiple_of(4096),
    }
}

struct Entry {
    block: CachedBlock,
    size: usize,
    referenced: AtomicBool,
}

type Key = (u64, u32);

#[derive(Default)]
struct Shard {
    map: HashMap<Key, Entry>,
    clock: Vec<Key>,
    hand: usize,
    bytes: usize,
    /// Bulk reads' blocks in a full cache, oldest first (not in `clock`): `ring_bytes` of them.
    ring: VecDeque<Key>,
    ring_bytes: usize,
    /// Evicted since it last had room: a bulk read's block goes into the ring.
    full: bool,
}

impl Shard {
    /// Its blocks' bytes and its tables'.
    fn held(&self) -> usize {
        self.bytes
            + self.map.allocation_size()
            + (self.clock.capacity() + self.ring.capacity()) * std::mem::size_of::<Key>()
    }

    /// Evict one block for a bulk read: its ring's oldest block that nobody else read, when the
    /// ring holds `ring_max` bytes or more (one somebody read leaves the ring for the clock, its
    /// bit set); else as any read (CLOCK). False when it holds none.
    fn evict_for_bulk(&mut self, ring_max: usize) -> bool {
        while self.ring_bytes >= ring_max
            && let Some(key) = self.ring.pop_front()
        {
            let size = self.map[&key].size;
            self.ring_bytes -= size;
            if self.map[&key].referenced.load(Ordering::Relaxed) {
                self.clock.push(key);
            } else {
                self.map.remove(&key);
                self.bytes -= size;
                self.full = true;
                return true;
            }
        }
        self.evict_one()
    }

    /// Evict one block: the first one not referenced from the hand on, clearing the referenced
    /// ones' bits on the way (CLOCK); the ring's oldest when the clock holds none. False when it
    /// holds none.
    fn evict_one(&mut self) -> bool {
        while !self.clock.is_empty() {
            let i = self.hand % self.clock.len();
            let key = self.clock[i];
            if let Some(e) = self.map.get(&key)
                && e.referenced.load(Ordering::Relaxed)
            {
                e.referenced.store(false, Ordering::Relaxed);
            } else {
                // (The newest block moves into the slot: the hand goes past it, as CLOCK puts a
                // new block behind the hand. Left on it, the hand evicted each insert's block at
                // the next, and the cache kept only its oldest blocks.)
                if let Some(e) = self.map.remove(&key) {
                    self.bytes -= e.size;
                }
                self.clock.swap_remove(i);
                self.hand = i + 1;
                self.full = true;
                return true;
            }
            self.hand = i + 1;
        }
        let Some(key) = self.ring.pop_front() else { return false };
        let e = self.map.remove(&key).unwrap();
        self.ring_bytes -= e.size;
        self.bytes -= e.size;
        true
    }
}

pub struct BlockCache {
    shards: Vec<RwLock<Shard>>,
    /// Each shard's blocks' buffers.
    arenas: Vec<Arc<Arena>>,
    capacity_per_shard: usize,
    /// A shard's ring for bulk reads holds this much at most.
    ring_max: usize,
    next_id: AtomicU64,
}

impl BlockCache {
    pub fn new(capacity_bytes: usize) -> Arc<Self> {
        let capacity_per_shard = (capacity_bytes / SHARDS).max(1 << 20);
        Arc::new(BlockCache {
            shards: (0..SHARDS).map(|_| RwLock::new(Shard::default())).collect(),
            arenas: (0..SHARDS).map(|_| Arena::new(capacity_per_shard)).collect(),
            capacity_per_shard,
            ring_max: capacity_per_shard / RING_SHARE,
            next_id: AtomicU64::new(1),
        })
    }

    pub fn next_file_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn shard_of(file: u64, block: u32) -> usize {
        (file.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ block as u64) as usize % SHARDS
    }

    fn shard(&self, file: u64, block: u32) -> &RwLock<Shard> {
        &self.shards[Self::shard_of(file, block)]
    }

    /// A buffer of `len` bytes for block `block` of `file`, which is going into the cache, in
    /// its shard's arena. Where the arena is at its limit with no free piece that fits, it
    /// evicts until one does: CLOCK, or for a bulk read's block (`bulk`) the ring first
    /// (`Shard::evict_for_bulk`). None: the buffer is the heap's.
    pub(crate) fn buffer(&self, file: u64, block: u32, len: usize, bulk: bool) -> Option<Buf> {
        let k = Self::shard_of(file, block);
        let a = &self.arenas[k];
        a.piece_bytes(len)?;
        if let Some(b) = a.take(len) {
            return Some(b);
        }
        let mut s = self.shards[k].write();
        for _ in 0..EVICT_FOR_ROOM {
            if !(if bulk { s.evict_for_bulk(self.ring_max) } else { s.evict_one() }) {
                break;
            }
            if let Some(b) = a.take(len) {
                return Some(b);
            }
        }
        None
    }

    pub fn get(&self, file: u64, block: u32) -> Option<CachedBlock> {
        let s = self.shard(file, block).read();
        let e = s.map.get(&(file, block))?;
        if !e.referenced.load(Ordering::Relaxed) {
            e.referenced.store(true, Ordering::Relaxed);
        }
        Some(e.block.clone())
    }

    pub fn insert(&self, file: u64, block: u32, b: CachedBlock, size: usize) {
        let mut s = self.shard(file, block).write();
        if s.map.contains_key(&(file, block)) {
            return;
        }
        let tables = s.held() - s.bytes;
        while s.bytes + tables + size > self.capacity_per_shard && s.evict_one() {}
        s.map.insert(
            (file, block),
            Entry {
                block: b,
                size,
                referenced: AtomicBool::new(false),
            },
        );
        s.clock.push((file, block));
        s.bytes += size;
    }

    /// `insert` for a bulk read's block (`storage::bulk_read`): a cache with room takes it as
    /// any block; a full one in its shard's ring, where it takes the place of the ring's oldest
    /// block (`Shard::evict_for_bulk`), so a big scan pushes out of the cache only the blocks
    /// another reader read while the ring held them.
    pub fn insert_bulk(&self, file: u64, block: u32, b: CachedBlock, size: usize) {
        let mut s = self.shard(file, block).write();
        if s.map.contains_key(&(file, block)) {
            return;
        }
        let tables = s.held() - s.bytes;
        let ring = s.full || s.bytes + tables + size > self.capacity_per_shard;
        while s.bytes + tables + size > self.capacity_per_shard && s.evict_for_bulk(self.ring_max) {}
        s.map.insert(
            (file, block),
            Entry {
                block: b,
                size,
                referenced: AtomicBool::new(false),
            },
        );
        if ring {
            s.ring.push_back((file, block));
            s.ring_bytes += size;
        } else {
            s.clock.push((file, block));
        }
        s.bytes += size;
    }

    /// Drop every block of a closed file.
    pub fn forget_file(&self, file: u64) {
        for shard in &self.shards {
            let mut s = shard.write();
            if !s.map.keys().any(|(f, _)| *f == file) {
                continue;
            }
            let ring_freed: usize = s.ring.iter().filter(|(f, _)| *f == file).map(|k| s.map[k].size).sum();
            s.ring_bytes -= ring_freed;
            s.ring.retain(|(f, _)| *f != file);
            let mut freed = 0;
            s.map.retain(|(f, _), e| {
                if *f == file {
                    freed += e.size;
                    false
                } else {
                    true
                }
            });
            s.bytes -= freed;
            s.clock.retain(|(f, _)| *f != file);
            // (A shard left empty gives its tables back: an empty cache holds nothing.)
            if s.map.is_empty() {
                *s = Shard::default();
            } else if s.held() * 2 <= self.capacity_per_shard {
                s.full = false;
            }
        }
    }

    /// The bytes the cache may hold.
    pub fn capacity_bytes(&self) -> usize {
        self.capacity_per_shard * SHARDS
    }

    pub fn used_bytes(&self) -> usize {
        self.shards.iter().map(|s| s.read().held()).sum()
    }
    /// Whether it holds block `block` of `file` (without marking it read).
    #[cfg(test)]
    pub(crate) fn contains(&self, file: u64, block: u32) -> bool {
        self.shard(file, block).read().map.contains_key(&(file, block))
    }
}

#[cfg(test)]
mod tests {
    /// What the cache counts of an allocation is what the server's allocator makes of it.
    #[test]
    fn alloc_bytes_is_the_allocators_size() {
        for n in 1..=256 << 10 {
            // SAFETY: mi_good_size only computes a size.
            assert_eq!(super::alloc_bytes(n), unsafe { libmimalloc_sys::mi_good_size(n) }, "{n}");
        }
    }
}
