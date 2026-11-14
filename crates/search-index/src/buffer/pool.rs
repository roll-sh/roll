//! Byte slices for in-memory postings (Lucene's `ByteSlicePool`).
//!
//! Every term of the indexing buffer appends to one or two byte streams (docs+freqs,
//! positions+offsets+payloads) that grow while the buffer fills. Giving each a `Vec` would cost
//! an allocation per term and per growth; instead all streams live interleaved in 32 KiB
//! blocks. A stream starts as a 5-byte slice; when a write hits a slice's end marker (non-zero
//! byte: `16 | level`) a slice of the next level (14, 20, 30, … 200 bytes) is allocated, the
//! last 3 data bytes move there and the old slice's last 4 bytes become a forwarding address.
//! Blocks are zero-filled, so a zero byte means "free". A stream costs 5 bytes when it holds
//! one byte, and at most ~2% overhead once large.

const BLOCK_SHIFT: u32 = 15;
const BLOCK_SIZE: usize = 1 << BLOCK_SHIFT;
const BLOCK_MASK: u32 = (BLOCK_SIZE as u32) - 1;

const LEVEL_SIZE: [usize; 10] = [5, 14, 20, 30, 40, 40, 80, 80, 120, 200];
const NEXT_LEVEL: [usize; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 9];

/// The blocks backing every stream of one indexing buffer.
pub struct BytePool {
    blocks: Vec<Box<[u8]>>,
    /// Write offset in the last block.
    upto: usize,
}

impl Default for BytePool {
    fn default() -> Self {
        Self::new()
    }
}

impl BytePool {
    pub fn new() -> BytePool {
        BytePool { blocks: Vec::new(), upto: BLOCK_SIZE }
    }

    /// Heap bytes held.
    pub fn ram_bytes(&self) -> usize {
        self.blocks.len() * BLOCK_SIZE + self.blocks.capacity() * 16
    }

    fn reserve(&mut self, size: usize) {
        if self.upto + size > BLOCK_SIZE {
            self.blocks.push(vec![0u8; BLOCK_SIZE].into_boxed_slice());
            self.upto = 0;
        }
    }

    #[inline]
    fn addr(&self) -> u32 {
        (((self.blocks.len() - 1) as u32) << BLOCK_SHIFT) | self.upto as u32
    }

    /// Starts a new stream; returns its start address (which is also its first write address).
    pub fn new_stream(&mut self) -> u32 {
        self.reserve(LEVEL_SIZE[0]);
        let a = self.addr();
        self.upto += LEVEL_SIZE[0];
        let b = self.blocks.len() - 1;
        self.blocks[b][self.upto - 1] = 16;
        a
    }

    #[inline]
    fn byte_mut(&mut self, addr: u32) -> &mut u8 {
        &mut self.blocks[(addr >> BLOCK_SHIFT) as usize][(addr & BLOCK_MASK) as usize]
    }

    #[inline]
    fn byte(&self, addr: u32) -> u8 {
        self.blocks[(addr >> BLOCK_SHIFT) as usize][(addr & BLOCK_MASK) as usize]
    }

    /// Allocates the next slice after the one whose end marker is at `end`; returns the new
    /// write address.
    fn grow(&mut self, end: u32) -> u32 {
        let level = (self.byte(end) & 15) as usize;
        let nl = NEXT_LEVEL[level];
        let size = LEVEL_SIZE[nl];
        self.reserve(size);
        let new = self.addr();
        self.upto += size;
        // Move the last three data bytes; their place takes the forwarding address.
        let moved = [self.byte(end - 3), self.byte(end - 2), self.byte(end - 1)];
        let b = self.blocks.len() - 1;
        let off = (new & BLOCK_MASK) as usize;
        self.blocks[b][off..off + 3].copy_from_slice(&moved);
        self.blocks[b][off + size - 1] = 16 | nl as u8;
        let fwd = new.to_le_bytes();
        for (i, v) in fwd.iter().enumerate() {
            *self.byte_mut(end - 3 + i as u32) = *v;
        }
        new + 3
    }

    /// Appends a byte to the stream whose write address is `*w`.
    #[inline]
    pub fn write_byte(&mut self, w: &mut u32, v: u8) {
        if self.byte(*w) != 0 {
            *w = self.grow(*w);
        }
        *self.byte_mut(*w) = v;
        *w += 1;
    }

    #[inline]
    pub fn write_vu32(&mut self, w: &mut u32, mut v: u32) {
        while v >= 0x80 {
            self.write_byte(w, (v as u8) | 0x80);
            v >>= 7;
        }
        self.write_byte(w, v as u8);
    }

    pub fn write_bytes(&mut self, w: &mut u32, b: &[u8]) {
        for &x in b {
            self.write_byte(w, x);
        }
    }

    /// A reader of the stream `start..end` (`end` = its current write address).
    pub fn reader(&self, start: u32, end: u32) -> SliceReader<'_> {
        let mut r = SliceReader { pool: self, pos: start, limit: 0, level: 0, end };
        r.limit = if start + LEVEL_SIZE[0] as u32 >= end { end } else { start + LEVEL_SIZE[0] as u32 - 4 };
        r
    }
}

/// Reads one stream of a [`BytePool`].
pub struct SliceReader<'a> {
    pool: &'a BytePool,
    pos: u32,
    limit: u32,
    level: usize,
    end: u32,
}

impl SliceReader<'_> {
    #[inline]
    pub fn eof(&self) -> bool {
        self.pos == self.end
    }

    #[inline]
    pub fn read_byte(&mut self) -> u8 {
        if self.pos == self.limit && self.pos != self.end {
            self.next_slice();
        }
        let b = self.pool.byte(self.pos);
        self.pos += 1;
        b
    }

    fn next_slice(&mut self) {
        let mut a = [0u8; 4];
        for (i, v) in a.iter_mut().enumerate() {
            *v = self.pool.byte(self.limit + i as u32);
        }
        let next = u32::from_le_bytes(a);
        self.level = NEXT_LEVEL[self.level];
        let size = LEVEL_SIZE[self.level] as u32;
        self.pos = next;
        self.limit = if next + size >= self.end && next <= self.end { self.end } else { next + size - 4 };
    }

    #[inline]
    pub fn read_vu32(&mut self) -> u32 {
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = self.read_byte();
            v |= ((b & 0x7f) as u32) << shift;
            if b < 0x80 {
                return v;
            }
            shift += 7;
        }
    }

    pub fn read_into(&mut self, out: &mut Vec<u8>, n: usize) {
        for _ in 0..n {
            out.push(self.read_byte());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    fn interleaved_streams_round_trip() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut pool = BytePool::new();
        let n = 300;
        let mut starts = Vec::new();
        let mut writes = Vec::new();
        let mut expect: Vec<Vec<u32>> = vec![Vec::new(); n];
        for _ in 0..n {
            let s = pool.new_stream();
            starts.push(s);
            writes.push(s);
        }
        for _ in 0..60_000 {
            let i = rng.gen_range(0..n);
            let v: u32 = if rng.gen_bool(0.5) { rng.gen_range(0..200) } else { rng.r#gen() };
            pool.write_vu32(&mut writes[i], v);
            expect[i].push(v);
        }
        for i in 0..n {
            let mut r = pool.reader(starts[i], writes[i]);
            for &v in &expect[i] {
                assert_eq!(r.read_vu32(), v);
            }
            assert!(r.eof());
        }
    }

    #[test]
    fn single_byte_and_empty() {
        let mut pool = BytePool::new();
        let s = pool.new_stream();
        let r = pool.reader(s, s);
        assert!(r.eof());
        let mut w = s;
        pool.write_byte(&mut w, 9);
        let mut r = pool.reader(s, w);
        assert_eq!(r.read_byte(), 9);
        assert!(r.eof());
    }
}
