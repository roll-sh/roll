//! The block cache's own memory for its blocks' buffers (`super::BlockCache::buffer`), as
//! PostgreSQL's shared buffers are its own: chunks the cache maps itself, cut into pieces of a
//! block's size, a piece given back to its shard's arena by whichever thread drops the block
//! last and taken by the next insert, whichever thread makes it.
//!
//! (From the allocator, a cache miss's buffer came from the reading thread's heap, often a
//! parallel scan worker's that exits when its scan ends. With CLOCK evicting the oldest blocks,
//! their buffers were freed into heaps no thread allocated from any more: at TPC-H SF 1 with a
//! 256 MB cache the process peaked at 695-712 MB, against 527-540 MB with each insert evicting
//! the block inserted before it, HANDOFF "Block cache memory".)
//!
//! A chunk is mapped when no free piece fits a buffer, and unmapped when its last piece is
//! freed: an empty cache holds no chunk. Free pieces are kept in lists by size (a two-level
//! segregated fit, as TLSF's), and a freed piece joins the free pieces either side of it. A
//! piece is its buffer's length rounded up to 16 bytes, after a 16-byte header.

use parking_lot::Mutex;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A piece's header: its size (header included), the size of the piece before it in its chunk
/// (0: it is the chunk's first) and whether it is free. A free piece's links to the other free
/// pieces of its list follow it.
#[repr(C, align(16))]
struct Hdr {
    size: u32,
    prev: u32,
    free: u32,
    _pad: u32,
}

#[repr(C)]
struct Links {
    next: *mut Hdr,
    back: *mut Hdr,
}

const HDR: usize = std::mem::size_of::<Hdr>();
/// The smallest piece: a header and a free piece's links.
const MIN: usize = HDR + std::mem::size_of::<Links>();
/// Pieces below 1 kB in lists 16 bytes apart; past it, 16 lists to each power of two.
const LINEAR: usize = 1024;
const LISTS: usize = LINEAR / 16 + (21 - 10) * 16;
/// Free pieces at least this big give back their pages (`MADV_DONTNEED`).
const PURGE: usize = 64 << 10;

fn list(size: usize) -> usize {
    if size < LINEAR {
        size / 16
    } else {
        let fl = (usize::BITS - 1 - size.leading_zeros()) as usize;
        LINEAR / 16 + (fl - 10) * 16 + ((size >> (fl - 4)) & 15)
    }
}

pub(crate) struct Arena {
    pieces: Mutex<Pieces>,
    /// Bytes a chunk maps.
    chunk: usize,
    /// The chunks' bytes it may map.
    limit: usize,
    /// The chunks' bytes it has mapped.
    mapped: AtomicUsize,
}

struct Pieces {
    heads: [*mut Hdr; LISTS],
    bits: [u64; LISTS.div_ceil(64)],
}

// SAFETY: the pieces' pointers are into chunks the arena owns, used under its mutex.
unsafe impl Send for Pieces {}

impl Arena {
    pub(crate) fn new(limit: usize) -> Arc<Arena> {
        // (A chunk a sixteenth of what the shard holds, between 64 kB and 1 MB: a small cache
        // maps little, a big one few chunks.)
        let chunk = (limit / 16).clamp(64 << 10, 1 << 20).next_power_of_two();
        Arc::new(Arena {
            pieces: Mutex::new(Pieces {
                heads: [std::ptr::null_mut(); LISTS],
                bits: [0; LISTS.div_ceil(64)],
            }),
            chunk,
            limit: limit.max(chunk),
            mapped: AtomicUsize::new(0),
        })
    }

    /// The bytes a buffer of `len` takes in a piece, or None when it is too big for one (a
    /// quarter of a chunk).
    pub(crate) fn piece_bytes(&self, len: usize) -> Option<usize> {
        let n = (HDR + len.next_multiple_of(16)).max(MIN);
        (n <= self.chunk / 4).then_some(n)
    }

    #[cfg(test)]
    fn mapped_bytes(&self) -> usize {
        self.mapped.load(Ordering::Relaxed)
    }

    /// A piece for a buffer of `len` bytes, a chunk mapped for it while the arena is below its
    /// limit. None when no free piece fits and no chunk may be mapped.
    pub(crate) fn take(self: &Arc<Self>, len: usize) -> Option<Buf> {
        let n = self.piece_bytes(len)?;
        let mut p = self.pieces.lock();
        let h = match p.find(n) {
            Some(h) => h,
            None => {
                if self.mapped.load(Ordering::Relaxed) + self.chunk > self.limit {
                    return None;
                }
                let c = self.map()?;
                // SAFETY: a new chunk, ours: its one piece, then the end's header.
                unsafe {
                    let size = self.chunk - HDR;
                    c.write(Hdr { size: size as u32, prev: 0, free: 1, _pad: 0 });
                    end(c, size).write(Hdr { size: HDR as u32, prev: size as u32, free: 0, _pad: 0 });
                    p.push(c);
                }
                c
            }
        };
        // SAFETY: `h` is a free piece of at least `n` bytes, out of its list after `find` /
        // now, and the pieces either side of it are this arena's.
        unsafe {
            p.unlink(h);
            let size = (*h).size as usize;
            if size - n >= MIN {
                let rest = end(h, n);
                rest.write(Hdr { size: (size - n) as u32, prev: n as u32, free: 1, _pad: 0 });
                (*end(rest, size - n)).prev = (size - n) as u32;
                (*h).size = n as u32;
                p.push(rest);
            }
            (*h).free = 0;
            let piece = (*h).size as usize;
            Some(Buf { arena: self.clone(), h: NonNull::new_unchecked(h), len, piece })
        }
    }

    fn map(&self) -> Option<*mut Hdr> {
        // SAFETY: an anonymous private mapping, nothing of ours replaced.
        let c = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.chunk,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if c == libc::MAP_FAILED {
            return None;
        }
        // (Not in huge pages: khugepaged would fill a 2 MB page a few pieces of which are
        // used.)
        #[cfg(target_os = "linux")]
        // SAFETY: the chunk just mapped.
        unsafe {
            libc::madvise(c, self.chunk, libc::MADV_NOHUGEPAGE);
        }
        let mapped = self.mapped.fetch_add(self.chunk, Ordering::Relaxed) + self.chunk;
        tracing::debug!(mapped, chunk = self.chunk, "block cache arena: chunk mapped");
        Some(c.cast())
    }

    /// SAFETY: `h` is a piece `take` gave out, not freed since.
    unsafe fn free(&self, mut h: *mut Hdr) {
        let mut p = self.pieces.lock();
        unsafe {
            let mut size = (*h).size as usize;
            let next = end(h, size);
            if (*next).free != 0 {
                p.unlink(next);
                size += (*next).size as usize;
            }
            let prev = (*h).prev as usize;
            if prev != 0 {
                let before = h.byte_sub(prev);
                if (*before).free != 0 {
                    p.unlink(before);
                    size += prev;
                    h = before;
                }
            }
            (*h).size = size as u32;
            (*h).free = 1;
            (*end(h, size)).prev = size as u32;
            if (*h).prev == 0 && size == self.chunk - HDR {
                drop(p);
                libc::munmap(h.cast(), self.chunk);
                let mapped = self.mapped.fetch_sub(self.chunk, Ordering::Relaxed) - self.chunk;
                tracing::debug!(mapped, "block cache arena: chunk unmapped");
                return;
            }
            if size >= PURGE {
                // (The whole pages past its header and links.)
                let lo = (h as usize + MIN).next_multiple_of(4096);
                let hi = (h as usize + size) & !4095;
                if hi > lo {
                    libc::madvise(lo as *mut libc::c_void, hi - lo, libc::MADV_DONTNEED);
                }
            }
            p.push(h);
        }
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // (Every buffer holds its arena: when it goes, every piece is free and every chunk
        // unmapped.)
        debug_assert_eq!(self.mapped.load(Ordering::Relaxed), 0);
    }
}

/// The header after `h`'s `size` bytes.
unsafe fn end(h: *mut Hdr, size: usize) -> *mut Hdr {
    unsafe { h.byte_add(size) }
}

impl Pieces {
    /// A free piece of at least `n` bytes: one of its own list's first few, else any of a
    /// list past it (each fits).
    fn find(&self, n: usize) -> Option<*mut Hdr> {
        let i = list(n);
        let mut h = self.heads[i];
        for _ in 0..8 {
            if h.is_null() {
                break;
            }
            // SAFETY: a free piece in a list, its links after it.
            unsafe {
                if (*h).size as usize >= n {
                    return Some(h);
                }
                h = (*links(h)).next;
            }
        }
        let (w, b) = ((i + 1) / 64, (i + 1) % 64);
        let mut word = self.bits.get(w)? & (!0u64).checked_shl(b as u32).unwrap_or(0);
        let mut w = w;
        loop {
            if word != 0 {
                return Some(self.heads[w * 64 + word.trailing_zeros() as usize]);
            }
            w += 1;
            word = *self.bits.get(w)?;
        }
    }

    /// SAFETY: `h` a free piece of ours, in no list.
    unsafe fn push(&mut self, h: *mut Hdr) {
        unsafe {
            let i = list((*h).size as usize);
            let head = self.heads[i];
            links(h).write(Links { next: head, back: std::ptr::null_mut() });
            if !head.is_null() {
                (*links(head)).back = h;
            }
            self.heads[i] = h;
            self.bits[i / 64] |= 1 << (i % 64);
        }
    }

    /// SAFETY: `h` a free piece of ours, in its list.
    unsafe fn unlink(&mut self, h: *mut Hdr) {
        unsafe {
            let i = list((*h).size as usize);
            let Links { next, back } = links(h).read();
            if back.is_null() {
                self.heads[i] = next;
                if next.is_null() {
                    self.bits[i / 64] &= !(1 << (i % 64));
                }
            } else {
                (*links(back)).next = next;
            }
            if !next.is_null() {
                (*links(next)).back = back;
            }
        }
    }
}

unsafe fn links(h: *mut Hdr) -> *mut Links {
    unsafe { h.add(1).cast() }
}

/// A piece of an arena with a buffer's bytes: written once (`as_mut`), then frozen into the
/// block's `Bytes`. The piece goes back to its arena when the last of them drops.
pub(crate) struct Buf {
    arena: Arc<Arena>,
    h: NonNull<Hdr>,
    len: usize,
    /// The piece's bytes, its header too: what the cache counts of the buffer.
    pub(crate) piece: usize,
}

// SAFETY: the piece is the buffer's alone: written through `&mut`, then only read.
unsafe impl Send for Buf {}
unsafe impl Sync for Buf {}

impl Buf {
    pub(crate) fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: the piece's bytes past its header, at least `len`, ours alone.
        unsafe { std::slice::from_raw_parts_mut(self.h.as_ptr().add(1).cast(), self.len) }
    }

    pub(crate) fn freeze(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(self)
    }

    /// The bytes the `Bytes` made of a buffer (`freeze`) holds besides its piece: its owner's
    /// allocation, at the allocator's size.
    pub(crate) fn owner_bytes() -> usize {
        super::alloc_bytes(2 * std::mem::size_of::<usize>() + std::mem::size_of::<Buf>())
    }
}

impl AsRef<[u8]> for Buf {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: as `as_mut`, shared.
        unsafe { std::slice::from_raw_parts(self.h.as_ptr().add(1).cast(), self.len) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        // SAFETY: the piece `take` gave this buffer.
        unsafe { self.arena.free(self.h.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Buffers of mixed sizes taken and dropped in a scrambled order never overlap, keep
    /// their bytes, and when the last goes every chunk is unmapped; a full arena takes no more.
    #[test]
    fn pieces_come_back_whole() {
        let a = Arena::new(4 << 20);
        let mut held: Vec<(Buf, u8)> = Vec::new();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for round in 0..20_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            if held.len() > 300 || (x.is_multiple_of(3) && !held.is_empty()) {
                let (b, v) = held.swap_remove((x >> 8) as usize % held.len());
                assert!(b.as_ref().iter().all(|&c| c == v));
                drop(b);
            } else {
                let len = [4200, 8300, 8192, 16500, 700, 40, 1][(x >> 20) as usize % 7] + (x >> 40) as usize % 300;
                let Some(mut b) = a.take(len) else { continue };
                let v = round as u8;
                b.as_mut().fill(v);
                let n = a.piece_bytes(len).unwrap();
                assert!(b.piece >= n && b.piece < n + MIN && b.as_ref().len() == len);
                held.push((b, v));
            }
            assert!(a.mapped_bytes() <= 4 << 20);
        }
        let mut spans: Vec<(usize, usize)> = held.iter().map(|(b, _)| (b.h.as_ptr() as usize, b.piece)).collect();
        spans.sort();
        assert!(spans.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0));
        drop(held);
        assert_eq!(a.mapped_bytes(), 0);
        // (Full: past the limit no chunk is mapped.)
        let big: Vec<Buf> = std::iter::from_fn(|| a.take(60_000)).take(1000).collect();
        assert_eq!(a.mapped_bytes(), 4 << 20);
        assert!(big.len() >= 60 && a.take(60_000).is_none());
        drop(big);
        assert_eq!(a.mapped_bytes(), 0);
    }
}
