//! The two chunk kinds a series keeps its samples in: Gorilla-compressed (`gorilla.c`,
//! `compressed_chunk.c`) and plain arrays of samples (`chunk.c`), ported operation for
//! operation so the bits written, the sizes allocated and the samples kept match the module's.

use crate::client::jemalloc_size;

/// `sizeof(Sample)`.
pub const SAMPLE_SIZE: u64 = 16;
/// `sizeof(CompressedChunk)` and `sizeof(Chunk)`.
const COMPRESSED_STRUCT: usize = 80;
const UNCOMPRESSED_STRUCT: usize = 32;
const CHUNK_RESIZE_STEP: u64 = 32;
/// All NaNs are stored as this one.
const CANONICAL_NAN: u64 = 0x7ff8_0000_0000_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sample {
    pub ts: u64,
    pub v: f64,
}

/// `ChunkResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cr {
    Ok,
    Err,
    End,
}

/// `DuplicatePolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp {
    None = 0,
    Block = 1,
    Last = 2,
    First = 3,
    Min = 4,
    Max = 5,
    Sum = 6,
}

impl Dp {
    pub fn from_u64(v: u64) -> Option<Dp> {
        Some(match v {
            0 => Dp::None,
            1 => Dp::Block,
            2 => Dp::Last,
            3 => Dp::First,
            4 => Dp::Min,
            5 => Dp::Max,
            6 => Dp::Sum,
            _ => return None,
        })
    }

    /// `DuplicatePolicyFromString`.
    pub fn parse(s: &[u8]) -> Option<Dp> {
        let l = s.to_ascii_lowercase();
        Some(match &l[..] {
            b"min" => Dp::Min,
            b"max" => Dp::Max,
            b"sum" => Dp::Sum,
            b"last" => Dp::Last,
            b"block" => Dp::Block,
            b"first" => Dp::First,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Dp::None => "none",
            Dp::Block => "block",
            Dp::Last => "last",
            Dp::First => "first",
            Dp::Min => "min",
            Dp::Max => "max",
            Dp::Sum => "sum",
        }
    }
}

/// `handleDuplicateSample`: what to keep for a timestamp already there (in `new`), or Err.
pub fn handle_duplicate(policy: Dp, old: Sample, new: &mut Sample) -> Cr {
    let has_nan = old.v.is_nan() || new.v.is_nan();
    if old.v.is_nan() != new.v.is_nan() && matches!(policy, Dp::Min | Dp::Max | Dp::Sum) {
        return Cr::Err;
    }
    if has_nan && policy != Dp::Block {
        if new.v.is_nan() {
            new.v = old.v;
        }
        return Cr::Ok;
    }
    match policy {
        Dp::Block => return Cr::Err,
        Dp::First => *new = old,
        Dp::Last => {}
        Dp::Min => {
            if old.v < new.v {
                new.v = old.v;
            }
        }
        Dp::Max => {
            if old.v > new.v {
                new.v = old.v;
            }
        }
        Dp::Sum => new.v += old.v,
        Dp::None => return Cr::Err,
    }
    Cr::Ok
}

// ---------------------------------------------------------------------------------------------
// Gorilla
// ---------------------------------------------------------------------------------------------

#[inline]
fn lsb(x: u64, bits: u32) -> u64 {
    if bits >= 64 { x } else { x & ((1u64 << bits) - 1) }
}

#[inline]
fn bin2int(bin: u64, l: u32) -> i64 {
    if bin & (1u64 << (l - 1)) == 0 { bin as i64 } else { (bin as i64).wrapping_sub(1i64 << l) }
}

#[inline]
fn in_range(x: i64, nbits: u32) -> bool {
    x >= -(1i64 << (nbits - 1)) && x < (1i64 << (nbits - 1))
}

#[inline]
fn bit_off(bins: &[u64], bit: u64) -> bool {
    bins[(bit / 64) as usize] & (1u64 << (bit % 64)) == 0
}

/// `readBits`.
#[inline]
fn read_bits(bins: &[u64], start: u64, len: u32) -> u64 {
    let lbit = (start % 64) as u32;
    let available = 64 - lbit;
    let w = (start / 64) as usize;
    if available >= len {
        lsb(bins[w] >> lbit, len)
    } else {
        lsb(bins[w] >> lbit, available) | lsb(bins[w + 1], len - available) << available
    }
}

/// A Gorilla chunk (`CompressedChunk`).
#[derive(Clone, Debug)]
pub struct Compressed {
    /// Bytes of `data`.
    pub size: u64,
    pub count: u64,
    /// Bits written.
    pub idx: u64,
    pub base_value: u64,
    pub base_ts: u64,
    pub data: Vec<u64>,
    pub prev_ts: u64,
    pub prev_delta: i64,
    pub prev_value: u64,
    pub prev_leading: u8,
    pub prev_trailing: u8,
}

impl Compressed {
    /// `Compressed_NewChunk`.
    pub fn new(size: u64) -> Self {
        Compressed {
            size,
            count: 0,
            idx: 0,
            base_value: 0,
            base_ts: 0,
            data: vec![0; size.div_ceil(8) as usize],
            prev_ts: 0,
            prev_delta: 0,
            prev_value: 0,
            prev_leading: 32,
            prev_trailing: 32,
        }
    }

    fn space(&self, n: u32) -> bool {
        n as u64 <= (self.size * 8).wrapping_sub(self.idx)
    }

    /// `appendBits`.
    fn append(&mut self, data: u64, len: u32) {
        let lbit = (self.idx % 64) as u32;
        let available = 64 - lbit;
        let w = (self.idx / 64) as usize;
        if available >= len {
            self.data[w] |= lsb(data, len) << lbit;
        } else {
            self.data[w] |= data << lbit;
            self.data[w + 1] |= lsb(data >> available, lbit);
        }
        self.idx += len as u64;
    }

    /// `appendInteger`.
    fn append_integer(&mut self, ts: u64) -> Cr {
        let cur = ts.wrapping_sub(self.prev_ts);
        let dd = (cur as i64).wrapping_sub(self.prev_delta);
        let steps: [(u64, u32, u32); 5] = [(0x01, 2, 5), (0x03, 3, 8), (0x07, 4, 11), (0x0f, 5, 15), (0x1f, 6, 32)];
        if dd == 0 {
            if !self.space(2) {
                return Cr::Err;
            }
            self.append(0, 1);
        } else if let Some(&(prefix, plen, l)) = steps.iter().find(|s| in_range(dd, s.2)) {
            if !self.space(plen + l + 1) {
                return Cr::Err;
            }
            self.append(prefix, plen);
            self.append(lsb(dd as u64, l), l);
        } else {
            if !self.space(6 + 64 + 1) {
                return Cr::Err;
            }
            self.append(0x3f, 6);
            self.append(dd as u64, 64);
        }
        self.prev_delta = cur as i64;
        self.prev_ts = ts;
        Cr::Ok
    }

    /// `appendFloat`.
    fn append_float(&mut self, value: f64) -> Cr {
        let x = value.to_bits() ^ self.prev_value;
        if x == 0 {
            self.append(0, 1);
            return Cr::Ok;
        }
        self.append(1, 1);
        let leading = x.leading_zeros().min(31);
        let trailing = x.trailing_zeros();
        let (pl, pt) = (self.prev_leading as u32, self.prev_trailing as u32);
        let block = (64 - leading - trailing) as u8 as u32;
        let expected = 5 + 6 + block;
        let prev_block = (64u32.wrapping_sub(pl).wrapping_sub(pt)) as u8 as u32;
        if leading >= pl && trailing >= pt && expected > prev_block {
            if !self.space(prev_block + 1) {
                return Cr::Err;
            }
            self.append(0, 1);
            self.append(x >> pt, prev_block);
        } else {
            if !self.space(expected + 1) {
                return Cr::Err;
            }
            self.append(1, 1);
            self.append(leading as u64, 5);
            self.append((block - 1) as u64, 6);
            self.append(x >> trailing, block);
            self.prev_leading = leading as u8;
            self.prev_trailing = trailing as u8;
        }
        self.prev_value = value.to_bits();
        Cr::Ok
    }

    /// `zero_bits`, with its rounding of the last word.
    fn zero_bits(&mut self, start: u64, end: u64) {
        if start == end {
            return;
        }
        let first = (start >> 6) as usize;
        let end_byte = (end >> 3).min(self.size - 1);
        let last = (end_byte >> 3) as usize;
        self.data[first] &= lsb(u64::MAX, (start % 64) as u32);
        for w in first + 1..=last {
            self.data[w] = 0;
        }
    }

    /// `Compressed_Append`.
    pub fn add(&mut self, ts: u64, value: f64) -> Cr {
        let value = if value.is_nan() { f64::from_bits(CANONICAL_NAN) } else { value };
        if self.count == 0 {
            self.base_value = value.to_bits();
            self.prev_value = value.to_bits();
            self.base_ts = ts;
            self.prev_ts = ts;
            self.prev_delta = 0;
        } else {
            let (idx, prev_ts, prev_delta) = (self.idx, self.prev_ts, self.prev_delta);
            if self.append_integer(ts) != Cr::Ok || self.append_float(value) != Cr::Ok {
                let end = self.idx;
                self.zero_bits(idx, end);
                self.idx = idx;
                self.prev_ts = prev_ts;
                self.prev_delta = prev_delta;
                return Cr::End;
            }
        }
        self.count += 1;
        Cr::Ok
    }

    /// `ensureAddSample`: grow by 32 bytes when full.
    fn ensure_add(&mut self, s: Sample) {
        if self.add(s.ts, s.v) != Cr::Ok {
            self.size += CHUNK_RESIZE_STEP;
            self.data.resize(self.size.div_ceil(8) as usize, 0);
            let r = self.add(s.ts, s.v);
            debug_assert_eq!(r, Cr::Ok);
        }
    }

    /// `trimChunk`: give back what's past the written bits, keeping whole words.
    fn trim(&mut self) {
        if self.size * 8 < self.idx {
            return;
        }
        let excess = (self.size * 8 - self.idx) / 8;
        if excess > 1 {
            let mut new_size = self.size - excess + 1;
            new_size += 8 - new_size % 8;
            self.size = new_size;
            self.data.resize(new_size.div_ceil(8) as usize, 0);
        }
    }

    pub fn iter(&self) -> CIter<'_> {
        CIter { c: self, idx: 0, count: 0, prev_ts: self.base_ts, prev_delta: 0, prev_value: self.base_value, leading: 32, trailing: 32, blocksize: 0 }
    }

    fn samples(&self) -> impl Iterator<Item = Sample> + '_ {
        let mut it = self.iter();
        std::iter::from_fn(move || it.next())
    }
}

/// `Compressed_Iterator`.
pub struct CIter<'a> {
    c: &'a Compressed,
    idx: u64,
    count: u64,
    prev_ts: u64,
    prev_delta: i64,
    prev_value: u64,
    leading: u8,
    trailing: u8,
    blocksize: u8,
}

impl CIter<'_> {
    fn read_integer(&mut self) -> i64 {
        let bins = &self.c.data[..];
        for l in [5u32, 8, 11, 15, 32] {
            let off = bit_off(bins, self.idx);
            self.idx += 1;
            if off {
                self.prev_delta = self.prev_delta.wrapping_add(bin2int(read_bits(bins, self.idx, l), l));
                self.idx += l as u64;
                return self.prev_delta;
            }
        }
        self.prev_delta = self.prev_delta.wrapping_add(read_bits(bins, self.idx, 64) as i64);
        self.idx += 64;
        self.prev_delta
    }

    fn read_float(&mut self) -> u64 {
        let bins = &self.c.data[..];
        let off = bit_off(bins, self.idx);
        self.idx += 1;
        let x = if off {
            let x = read_bits(bins, self.idx, self.blocksize as u32);
            self.idx += self.blocksize as u64;
            x.checked_shl(self.trailing as u32).unwrap_or(0)
        } else {
            self.leading = read_bits(bins, self.idx, 5) as u8;
            self.idx += 5;
            self.blocksize = read_bits(bins, self.idx, 6) as u8 + 1;
            self.idx += 6;
            self.trailing = 64u8.wrapping_sub(self.leading).wrapping_sub(self.blocksize);
            let x = read_bits(bins, self.idx, self.blocksize as u32).checked_shl(self.trailing as u32).unwrap_or(0);
            self.idx += self.blocksize as u64;
            x
        };
        self.prev_value ^= x;
        self.prev_value
    }
}

impl Iterator for CIter<'_> {
    type Item = Sample;

    /// `Compressed_ChunkIteratorGetNext`.
    fn next(&mut self) -> Option<Sample> {
        if self.count >= self.c.count {
            return None;
        }
        self.count += 1;
        if self.count == 1 {
            return Some(Sample { ts: self.c.base_ts, v: f64::from_bits(self.c.base_value) });
        }
        let off = bit_off(&self.c.data, self.idx);
        self.idx += 1;
        let delta = if off { self.prev_delta } else { self.read_integer() };
        self.prev_ts = self.prev_ts.wrapping_add(delta as u64);
        let off = bit_off(&self.c.data, self.idx);
        self.idx += 1;
        let v = if off { self.prev_value } else { self.read_float() };
        Some(Sample { ts: self.prev_ts, v: f64::from_bits(v) })
    }
}

// ---------------------------------------------------------------------------------------------
// Uncompressed
// ---------------------------------------------------------------------------------------------

/// A plain chunk (`Chunk`): `size` bytes of room for samples.
#[derive(Clone, Debug)]
pub struct Plain {
    pub base_ts: u64,
    pub samples: Vec<Sample>,
    pub size: u64,
}

impl Plain {
    pub fn new(size: u64) -> Self {
        Plain { base_ts: 0, samples: Vec::new(), size }
    }

    fn full(&self) -> bool {
        self.samples.len() as u64 == self.size / SAMPLE_SIZE
    }

    pub fn add(&mut self, s: Sample) -> Cr {
        if self.full() {
            return Cr::End;
        }
        if self.samples.is_empty() {
            self.base_ts = s.ts;
        }
        self.samples.push(s);
        Cr::Ok
    }
}

// ---------------------------------------------------------------------------------------------
// Either
// ---------------------------------------------------------------------------------------------

/// A duplicate sample the duplicate policy refuses (`UpsertSample` failing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused;

#[derive(Clone, Debug)]
pub enum Chunk {
    C(Compressed),
    U(Plain),
}

impl Chunk {
    pub fn new(compressed: bool, size: u64) -> Chunk {
        if compressed { Chunk::C(Compressed::new(size)) } else { Chunk::U(Plain::new(size)) }
    }

    pub fn num_samples(&self) -> u64 {
        match self {
            Chunk::C(c) => c.count,
            Chunk::U(u) => u.samples.len() as u64,
        }
    }

    /// `GetFirstTimestamp`: 0 for an empty chunk.
    pub fn first_ts(&self) -> u64 {
        match self {
            Chunk::C(c) => {
                if c.count == 0 {
                    0
                } else {
                    c.base_ts
                }
            }
            Chunk::U(u) => u.samples.first().map_or(0, |s| s.ts),
        }
    }

    pub fn last_ts(&self) -> u64 {
        match self {
            Chunk::C(c) => c.prev_ts,
            Chunk::U(u) => u.samples.last().map_or(0, |s| s.ts),
        }
    }

    pub fn last_value(&self) -> f64 {
        match self {
            Chunk::C(c) => f64::from_bits(c.prev_value),
            Chunk::U(u) => u.samples.last().map_or(0.0, |s| s.v),
        }
    }

    /// `GetChunkSize`: the data's size, or with the struct as allocated.
    pub fn size(&self, with_struct: bool) -> u64 {
        match self {
            Chunk::C(c) if with_struct => (jemalloc_size(COMPRESSED_STRUCT) + jemalloc_size(c.size as usize)) as u64,
            Chunk::U(u) if with_struct => (jemalloc_size(UNCOMPRESSED_STRUCT) + jemalloc_size(u.size as usize)) as u64,
            Chunk::C(c) => c.size,
            Chunk::U(u) => u.size,
        }
    }

    pub fn add(&mut self, s: Sample) -> Cr {
        match self {
            Chunk::C(c) => c.add(s.ts, s.v),
            Chunk::U(u) => u.add(s),
        }
    }

    /// Every sample, oldest first.
    pub fn samples(&self) -> Vec<Sample> {
        match self {
            Chunk::C(c) => c.samples().collect(),
            Chunk::U(u) => u.samples.clone(),
        }
    }

    /// `UpsertSample`: `Err` when the policy refuses; else how many samples were added (0 when
    /// one was replaced).
    pub fn upsert(&mut self, sample: &mut Sample, policy: Dp) -> Result<i32, Refused> {
        match self {
            Chunk::C(old) => {
                let ts = sample.ts;
                let mut new = Compressed::new(old.size);
                let mut it = old.iter();
                let n = old.count;
                let mut size = 0;
                let mut i = 0;
                let mut cur = Sample::default();
                let mut next_ok = true;
                while i < n {
                    match it.next() {
                        Some(s) => cur = s,
                        None => next_ok = false,
                    }
                    if cur.ts >= ts {
                        break;
                    }
                    new.ensure_add(cur);
                    i += 1;
                }
                if n > 0 && ts == cur.ts {
                    if handle_duplicate(policy, cur, sample) != Cr::Ok {
                        return Err(Refused);
                    }
                    match it.next() {
                        Some(s) => cur = s,
                        None => next_ok = false,
                    }
                    size = -1;
                }
                new.ensure_add(*sample);
                size += 1;
                if i < n {
                    while next_ok {
                        new.ensure_add(cur);
                        match it.next() {
                            Some(s) => cur = s,
                            None => next_ok = false,
                        }
                    }
                }
                *old = new;
                Ok(size)
            }
            Chunk::U(u) => {
                let ts = sample.ts;
                // The module counts with a `short` (a negative count compares as huge).
                let n = match u.samples.len() as i16 {
                    n if n < 0 => u.samples.len(),
                    n => n as usize,
                };
                let mut i = 0usize;
                let mut found = None;
                while i < n {
                    let s = u.samples[i];
                    found = Some(s);
                    if ts <= s.ts {
                        break;
                    }
                    i += 1;
                }
                if let Some(s) = found
                    && ts == s.ts
                {
                    if handle_duplicate(policy, s, sample) != Cr::Ok {
                        return Err(Refused);
                    }
                    u.samples[i].v = sample.v;
                    return Ok(0);
                }
                if i == 0 {
                    u.base_ts = ts;
                }
                if u.full() {
                    u.size += SAMPLE_SIZE;
                }
                u.samples.insert(i.min(u.samples.len()), *sample);
                Ok(1)
            }
        }
    }

    /// `DelRange`: drop the samples in [start, end]; how many went.
    pub fn del_range(&mut self, start: u64, end: u64) -> u64 {
        match self {
            Chunk::C(old) => {
                let mut new = Compressed::new(old.size);
                let mut deleted = 0;
                for s in old.samples() {
                    if s.ts >= start && s.ts <= end {
                        deleted += 1;
                        continue;
                    }
                    new.ensure_add(s);
                }
                *old = new;
                deleted
            }
            Chunk::U(u) => {
                let before = u.samples.len();
                u.samples.retain(|s| !(s.ts >= start && s.ts <= end));
                u.base_ts = u.samples.first().map_or(u.base_ts, |s| s.ts);
                (before - u.samples.len()) as u64
            }
        }
    }

    /// `SplitChunk`: keep the older half, return the newer.
    pub fn split(&mut self) -> Chunk {
        match self {
            Chunk::C(cur) => {
                let split = cur.count / 2;
                let keep = cur.count - split;
                let mut a = Compressed::new(cur.size);
                let mut b = Compressed::new(cur.size);
                for (i, s) in cur.samples().enumerate() {
                    if (i as u64) < keep {
                        a.ensure_add(s);
                    } else {
                        b.ensure_add(s);
                    }
                }
                a.trim();
                b.trim();
                *cur = a;
                Chunk::C(b)
            }
            Chunk::U(cur) => {
                let n = cur.samples.len();
                let split = n / 2;
                let keep = n - split;
                let mut new = Plain::new(split as u64 * SAMPLE_SIZE);
                for s in cur.samples.drain(keep..) {
                    new.add(s);
                }
                cur.size = keep as u64 * SAMPLE_SIZE;
                Chunk::U(new)
            }
        }
    }

    /// `ProcessChunk`: the samples in [start, end], newest first when `rev`.
    pub fn process(&self, start: u64, end: u64, rev: bool, out: &mut Vec<Sample>) {
        out.clear();
        if end < start || self.num_samples() == 0 {
            return;
        }
        match self {
            Chunk::C(c) => {
                if c.base_ts > end || c.prev_ts < start {
                    return;
                }
                out.extend(c.samples().skip_while(|s| s.ts < start).take_while(|s| s.ts <= end));
            }
            Chunk::U(u) => {
                if u.base_ts > end || u.samples.last().is_some_and(|s| s.ts < start) {
                    return;
                }
                out.extend(u.samples.iter().copied().skip_while(|s| s.ts < start).take_while(|s| s.ts <= end));
            }
        }
        if rev {
            out.reverse();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gorilla_round_trip() {
        let mut c = Compressed::new(128);
        let mut want = Vec::new();
        let mut ts = 1000u64;
        let mut v = 1.5f64;
        loop {
            if c.add(ts, v) != Cr::Ok {
                break;
            }
            want.push(Sample { ts, v });
            ts += 1 + (ts % 7) * 300 + if ts.is_multiple_of(11) { 1 << 33 } else { 0 };
            v = v * 1.37 - 0.25;
        }
        assert!(want.len() > 5);
        assert_eq!(Chunk::C(c.clone()).samples(), want);
        // A failed append leaves nothing behind.
        let before = c.clone();
        assert_eq!(c.add(ts, v), Cr::End);
        assert_eq!(c.data, before.data);
        assert_eq!(c.idx, before.idx);
    }

    #[test]
    fn upserts_and_splits() {
        for compressed in [true, false] {
            let mut ch = Chunk::new(compressed, 4096);
            for t in (10..100).step_by(10) {
                assert_eq!(ch.add(Sample { ts: t, v: t as f64 }), Cr::Ok);
            }
            let mut s = Sample { ts: 15, v: 1.0 };
            assert_eq!(ch.upsert(&mut s, Dp::Block), Ok(1));
            let mut s = Sample { ts: 20, v: 1.0 };
            assert_eq!(ch.upsert(&mut s, Dp::Block), Err(Refused));
            assert_eq!(ch.upsert(&mut s, Dp::Sum), Ok(0));
            let all = ch.samples();
            assert_eq!(all.iter().map(|s| s.ts).collect::<Vec<_>>(), [10, 15, 20, 30, 40, 50, 60, 70, 80, 90]);
            assert_eq!(all[2].v, 21.0);
            assert_eq!(ch.del_range(30, 50), 3);
            let tail = ch.split();
            assert_eq!(ch.samples().len() + tail.samples().len(), 7);
            assert_eq!(tail.first_ts(), 70);
            let mut out = Vec::new();
            ch.process(12, 100, true, &mut out);
            assert_eq!(out.iter().map(|s| s.ts).collect::<Vec<_>>(), [60, 20, 15]);
        }
    }
}
