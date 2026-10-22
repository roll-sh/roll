//! Random-access integer arrays: the value encodings behind doc values, ordinals and addresses.
//!
//! [`write_numeric`] picks the smallest of (like Lucene90's doc values):
//! - **const**: every value equal;
//! - **table**: at most 256 distinct values, stored once, with bit-packed indices;
//! - **gcd/delta**: `(v - min) / gcd` bit-packed at the width of the largest quotient (dates
//!   rounded to seconds, prices in cents, ...);
//! - **blocks**: per 16 384 values a separate minimum and width, when local ranges are much
//!   smaller than the global one (timestamps in index order);
//! - **linear**: non-decreasing values as an integer slope per block of 1024 plus bit-packed
//!   residuals (timestamps of time-ordered or time-sorted docs, ascending ids).
//!
//! [`write_monotonic`] encodes non-decreasing sequences (addresses, offsets) as a per-block
//! linear model plus small bit-packed residuals, like Lucene's `DirectMonotonicWriter`.
//!
//! Every reader is a header plus a zero-copy view of the packed bits; `get(i)` is a couple of
//! loads and shifts with no branches on the hot path besides the encoding kind.

use crate::bitpack::direct;
use crate::error::{Result, corrupt};
use crate::io::{In, Out};
use crate::slice::Slice;

const BLOCK_SHIFT: u32 = 14;
const BLOCK_SIZE: usize = 1 << BLOCK_SHIFT;
const MONO_SHIFT: u32 = 10;
const MONO_BLOCK: usize = 1 << MONO_SHIFT;

const CONST: u8 = 0;
const TABLE: u8 = 1;
const GCD: u8 = 2;
const BLOCKS: u8 = 3;
const LINEAR: u8 = 4;
const LINEAR_SHIFT: u32 = 10;
const LINEAR_BLOCK: usize = 1 << LINEAR_SHIFT;
/// base i64, slope i64, width u8, data offset u64.
const LINEAR_ENTRY: usize = 25;

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Writes `values` with the most compact encoding.
pub fn write_numeric<O: Out + ?Sized>(values: &[i64], out: &mut O) {
    write_numeric_with(values.len(), |i| values[i], out);
}

/// [`write_numeric`] over `n` values read through `get` (a permuted or converted view, so
/// callers need not materialize a copy). Reads each value a few times; writes append-only, so
/// `out` may stream into a file.
pub fn write_numeric_with<F: Fn(usize) -> i64, O: Out + ?Sized>(n: usize, get: F, out: &mut O) {
    out.put_vu64(n as u64);
    if n == 0 {
        out.put_u8(CONST);
        out.put_i64(0);
        return;
    }
    let (mut min, mut max) = (i64::MAX, i64::MIN);
    let mut sorted = true;
    let mut prev = i64::MIN;
    for i in 0..n {
        let v = get(i);
        min = min.min(v);
        max = max.max(v);
        sorted &= v >= prev;
        prev = v;
    }
    if min == max {
        out.put_u8(CONST);
        out.put_i64(min);
        return;
    }
    let mut g = 0u64;
    for i in 0..n {
        g = gcd(g, (get(i) as u64).wrapping_sub(min as u64));
        if g == 1 {
            break;
        }
    }
    let range = (max as u64).wrapping_sub(min as u64) / g;
    let delta_bits = direct::bits_for(range);
    // Distinct values, abandoning past 256.
    let mut uniq: Vec<i64> = Vec::new();
    for i in 0..n {
        let v = get(i);
        if let Err(at) = uniq.binary_search(&v) {
            if uniq.len() == 256 {
                uniq.clear();
                break;
            }
            uniq.insert(at, v);
        }
    }
    let nn = n as u64;
    let delta_size = direct::byte_len(nn, delta_bits) + 17;
    let table_size = if uniq.is_empty() {
        usize::MAX
    } else {
        direct::byte_len(nn, direct::bits_for(uniq.len() as u64 - 1)) + uniq.len() * 8 + 2
    };
    let nblocks = n.div_ceil(BLOCK_SIZE);
    // Per-block minima and widths; they only pay with enough values.
    let mut blocks: Vec<(i64, u32)> = Vec::new();
    let mut blocks_size = usize::MAX;
    if n > BLOCK_SIZE {
        let mut size = 8;
        blocks.reserve(nblocks);
        for b in 0..nblocks {
            let (lo, hi) = (b * BLOCK_SIZE, ((b + 1) * BLOCK_SIZE).min(n));
            let (mut bmin, mut bmax) = (i64::MAX, i64::MIN);
            for i in lo..hi {
                let v = get(i);
                bmin = bmin.min(v);
                bmax = bmax.max(v);
            }
            let bits = direct::bits_for((bmax as u64).wrapping_sub(bmin as u64) / g);
            size += 17 + direct::byte_len((hi - lo) as u64, bits);
            blocks.push((bmin, bits));
        }
        blocks_size = size;
    }
    // Values with a local linear trend (timestamps of time-ordered or time-sorted docs,
    // ascending ids): per block of 1024 an integer slope plus small residuals.
    let mut linear: Vec<(i64, i64, u32)> = Vec::new();
    let mut linear_size = usize::MAX;
    if sorted
        && n > 1
        && let Some(blocks) = linear_blocks(n, &get)
    {
        linear_size = 1 + blocks.len() * LINEAR_ENTRY
            + blocks
                .iter()
                .enumerate()
                .map(|(b, e)| direct::byte_len((n - b * LINEAR_BLOCK).min(LINEAR_BLOCK) as u64, e.2))
                .sum::<usize>();
        linear = blocks;
    }
    if linear_size < delta_size && linear_size < table_size && linear_size < blocks_size {
        out.put_u8(LINEAR);
        let mut offset = 0u64;
        for (b, &(base, slope, bits)) in linear.iter().enumerate() {
            out.put_i64(base);
            out.put_i64(slope);
            out.put_u8(bits as u8);
            out.put_u64(offset);
            offset += direct::byte_len((n - b * LINEAR_BLOCK).min(LINEAR_BLOCK) as u64, bits) as u64;
        }
        for (b, &(base, slope, bits)) in linear.iter().enumerate() {
            let lo = b * LINEAR_BLOCK;
            let len = (n - lo).min(LINEAR_BLOCK);
            direct::write(
                (0..len).map(|j| {
                    (get(lo + j) as u64)
                        .wrapping_sub((slope as u64).wrapping_mul(j as u64))
                        .wrapping_sub(base as u64)
                }),
                bits,
                out,
            );
        }
    } else if table_size <= delta_size && table_size <= blocks_size {
        out.put_u8(TABLE);
        out.put_vu32(uniq.len() as u32);
        for v in &uniq {
            out.put_i64(*v);
        }
        let bits = direct::bits_for(uniq.len() as u64 - 1);
        out.put_u8(bits as u8);
        direct::write((0..n).map(|i| uniq.binary_search(&get(i)).unwrap_or(0) as u64), bits, out);
    } else if blocks_size != usize::MAX && blocks_size * 10 < delta_size * 9 {
        out.put_u8(BLOCKS);
        out.put_u64(g);
        let mut offset = 0u64;
        for (b, &(bmin, bits)) in blocks.iter().enumerate() {
            let cnt = (n - b * BLOCK_SIZE).min(BLOCK_SIZE) as u64;
            out.put_i64(bmin);
            out.put_u8(bits as u8);
            out.put_u64(offset);
            offset += direct::byte_len(cnt, bits) as u64;
        }
        for (b, &(bmin, bits)) in blocks.iter().enumerate() {
            let (lo, hi) = (b * BLOCK_SIZE, ((b + 1) * BLOCK_SIZE).min(n));
            direct::write((lo..hi).map(|i| (get(i) as u64).wrapping_sub(bmin as u64) / g), bits, out);
        }
    } else {
        out.put_u8(GCD);
        out.put_i64(min);
        out.put_u64(g);
        out.put_u8(delta_bits as u8);
        direct::write((0..n).map(|i| (get(i) as u64).wrapping_sub(min as u64) / g), delta_bits, out);
    }
}

#[derive(Clone, Debug)]
enum Enc {
    Const(i64),
    Table { table: Vec<i64>, bits: u32 },
    Gcd { min: i64, gcd: u64, bits: u32 },
    Blocks { gcd: u64, table: Slice },
    /// Per block of 1024: `base + slope * j` plus a bit-packed residual.
    Linear { table: Slice },
}

/// Random access to values written by [`write_numeric`].
#[derive(Clone, Debug)]
pub struct NumericValues {
    len: u64,
    enc: Enc,
    data: Slice,
}

impl NumericValues {
    /// Parses at `*pos`, advancing past the encoded values.
    pub fn read(buf: &Slice, pos: &mut usize) -> Result<NumericValues> {
        let mut r = In::at(buf, *pos);
        let len = r.vu64()?;
        let kind = r.u8()?;
        let (enc, data) = match kind {
            CONST => (Enc::Const(r.i64()?), Slice::empty()),
            TABLE => {
                let n = r.vlen(256)?;
                if n == 0 {
                    return Err(corrupt("empty value table"));
                }
                let mut table = Vec::with_capacity(n);
                for _ in 0..n {
                    table.push(r.i64()?);
                }
                let bits = r.u8()? as u32;
                if bits > 8 {
                    return Err(corrupt("bad table width"));
                }
                let n = direct::byte_len(len, bits);
                let data = take(buf, &mut r, n)?;
                (Enc::Table { table, bits }, data)
            }
            GCD => {
                let min = r.i64()?;
                let gcd = r.u64()?;
                let bits = r.u8()? as u32;
                if bits > 64 || gcd == 0 {
                    return Err(corrupt("bad gcd encoding"));
                }
                let n = direct::byte_len(len, bits);
                let data = take(buf, &mut r, n)?;
                (Enc::Gcd { min, gcd, bits }, data)
            }
            BLOCKS => {
                let gcd = r.u64()?;
                let nblocks = len.div_ceil(BLOCK_SIZE as u64) as usize;
                let table = take(
                    buf,
                    &mut r,
                    nblocks.checked_mul(17).ok_or_else(|| corrupt("overflow"))?,
                )?;
                let mut total = 0usize;
                for b in 0..nblocks {
                    let e = &table[b * 17..b * 17 + 17];
                    let bits = e[8] as u32;
                    let offset = u64::from_le_bytes(e[9..17].try_into().unwrap()) as usize;
                    if bits > 64 || offset != total {
                        return Err(corrupt("bad numeric block table"));
                    }
                    let cnt = (len as usize - b * BLOCK_SIZE).min(BLOCK_SIZE) as u64;
                    total += direct::byte_len(cnt, bits);
                }
                let data = take(buf, &mut r, total)?;
                (Enc::Blocks { gcd, table }, data)
            }
            LINEAR => {
                let nblocks = len.div_ceil(LINEAR_BLOCK as u64) as usize;
                let table = take(
                    buf,
                    &mut r,
                    nblocks.checked_mul(LINEAR_ENTRY).ok_or_else(|| corrupt("overflow"))?,
                )?;
                let mut total = 0usize;
                for b in 0..nblocks {
                    let e = &table[b * LINEAR_ENTRY..(b + 1) * LINEAR_ENTRY];
                    let bits = e[16] as u32;
                    let offset = u64::from_le_bytes(e[17..25].try_into().unwrap()) as usize;
                    if bits > 64 || offset != total {
                        return Err(corrupt("bad linear block table"));
                    }
                    let cnt = (len as usize - b * LINEAR_BLOCK).min(LINEAR_BLOCK) as u64;
                    total += direct::byte_len(cnt, bits);
                }
                let data = take(buf, &mut r, total)?;
                (Enc::Linear { table }, data)
            }
            k => return Err(corrupt(format!("bad numeric encoding {k}"))),
        };
        *pos = r.pos();
        Ok(NumericValues { len, enc, data })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Value at `index` (`index < len`).
    #[inline]
    pub fn get(&self, index: u64) -> i64 {
        match &self.enc {
            Enc::Const(v) => *v,
            Enc::Table { table, bits } => {
                let i = direct::get(&self.data, *bits, index) as usize;
                table.get(i).copied().unwrap_or(0)
            }
            Enc::Gcd { min, gcd, bits } => {
                let q = direct::get(&self.data, *bits, index);
                (*min as u64).wrapping_add(q.wrapping_mul(*gcd)) as i64
            }
            Enc::Blocks { gcd, table } => {
                let b = (index >> BLOCK_SHIFT) as usize;
                let Some(e) = table.get(b * 17..b * 17 + 17) else {
                    return 0;
                };
                let bmin = i64::from_le_bytes(e[0..8].try_into().unwrap());
                let bits = e[8] as u32;
                let off = u64::from_le_bytes(e[9..17].try_into().unwrap()) as usize;
                let q = direct::get(
                    &self.data[off.min(self.data.len())..],
                    bits,
                    index & (BLOCK_SIZE as u64 - 1),
                );
                (bmin as u64).wrapping_add(q.wrapping_mul(*gcd)) as i64
            }
            Enc::Linear { table } => {
                let (base, slope, bits, data) = self.linear_block(table, (index >> LINEAR_SHIFT) as usize);
                let j = index & (LINEAR_BLOCK as u64 - 1);
                base.wrapping_add(slope.wrapping_mul(j)).wrapping_add(direct::get(data, bits, j)) as i64
            }
        }
    }

    /// `(base, slope, width, residuals)` of linear block `b` (zeros past the end).
    #[inline]
    fn linear_block<'a>(&'a self, table: &[u8], b: usize) -> (u64, u64, u32, &'a [u8]) {
        let Some(e) = table.get(b * LINEAR_ENTRY..(b + 1) * LINEAR_ENTRY) else {
            return (0, 0, 0, &[]);
        };
        let base = u64::from_le_bytes(e[0..8].try_into().unwrap());
        let slope = u64::from_le_bytes(e[8..16].try_into().unwrap());
        let off = (u64::from_le_bytes(e[17..25].try_into().unwrap()) as usize).min(self.data.len());
        (base, slope, e[16] as u32, &self.data[off..])
    }

    /// Replaces each slot, read as an index with `index`, by the value at that index written
    /// with `value`. One dispatch on the encoding for the whole batch (collectors gather the
    /// values of a block of matching docs at once).
    #[inline]
    pub fn gather<T: Copy>(&self, slots: &mut [T], index: impl Fn(T) -> u64, value: impl Fn(i64) -> T) {
        match &self.enc {
            Enc::Const(v) => slots.iter_mut().for_each(|s| *s = value(*v)),
            Enc::Table { table, bits } => {
                for s in slots.iter_mut() {
                    let i = direct::get(&self.data, *bits, index(*s)) as usize;
                    *s = value(table.get(i).copied().unwrap_or(0));
                }
            }
            Enc::Gcd { min, gcd, bits } => {
                let (min, gcd, bits) = (*min as u64, *gcd, *bits);
                if gcd == 1 {
                    for s in slots.iter_mut() {
                        *s = value(min.wrapping_add(direct::get(&self.data, bits, index(*s))) as i64);
                    }
                } else {
                    for s in slots.iter_mut() {
                        let q = direct::get(&self.data, bits, index(*s));
                        *s = value(min.wrapping_add(q.wrapping_mul(gcd)) as i64);
                    }
                }
            }
            Enc::Blocks { gcd, table } => {
                // Batches are mostly ascending: decode a block's header only when it changes.
                let mut cur = usize::MAX;
                let (mut bmin, mut bits, mut data): (u64, u32, &[u8]) = (0, 0, &[]);
                for s in slots.iter_mut() {
                    let i = index(*s);
                    let b = (i >> BLOCK_SHIFT) as usize;
                    if b != cur {
                        cur = b;
                        match table.get(b * 17..b * 17 + 17) {
                            Some(e) => {
                                bmin = i64::from_le_bytes(e[0..8].try_into().unwrap()) as u64;
                                bits = e[8] as u32;
                                let off = u64::from_le_bytes(e[9..17].try_into().unwrap()) as usize;
                                data = &self.data[off.min(self.data.len())..];
                            }
                            None => (bmin, bits, data) = (0, 0, &[]),
                        }
                    }
                    let q = direct::get(data, bits, i & (BLOCK_SIZE as u64 - 1));
                    *s = value(bmin.wrapping_add(q.wrapping_mul(*gcd)) as i64);
                }
            }
            Enc::Linear { table } => {
                let mut cur = usize::MAX;
                let (mut base, mut slope, mut bits, mut data) = (0u64, 0u64, 0u32, &[][..]);
                for s in slots.iter_mut() {
                    let i = index(*s);
                    let b = (i >> LINEAR_SHIFT) as usize;
                    if b != cur {
                        cur = b;
                        (base, slope, bits, data) = self.linear_block(table, b);
                    }
                    let j = i & (LINEAR_BLOCK as u64 - 1);
                    *s = value(base.wrapping_add(slope.wrapping_mul(j)).wrapping_add(direct::get(data, bits, j)) as i64);
                }
            }
        }
    }

    /// Decodes the `out.len()` values from `start` (`start + out.len() <= len`; values past
    /// the end read as 0). Sequential decoding for scans over every doc.
    pub fn get_range(&self, start: u64, out: &mut [i64]) {
        let n = (self.len.saturating_sub(start) as usize).min(out.len());
        out[n..].fill(0);
        let out = &mut out[..n];
        match &self.enc {
            Enc::Const(v) => out.fill(*v),
            Enc::Table { table, bits } => {
                let mut o = out.iter_mut();
                direct::for_each_range(&self.data, *bits, start, n, |i| {
                    if let Some(slot) = o.next() {
                        *slot = table.get(i as usize).copied().unwrap_or(0);
                    }
                });
            }
            Enc::Gcd { min, gcd, bits } => {
                let (min, gcd) = (*min as u64, *gcd);
                let mut o = out.iter_mut();
                direct::for_each_range(&self.data, *bits, start, n, |q| {
                    if let Some(slot) = o.next() {
                        *slot = min.wrapping_add(q.wrapping_mul(gcd)) as i64;
                    }
                });
            }
            Enc::Blocks { gcd, table } => {
                let mut i = 0usize;
                while i < n {
                    let index = start + i as u64;
                    let b = (index >> BLOCK_SHIFT) as usize;
                    let in_block = (index & (BLOCK_SIZE as u64 - 1)) as usize;
                    let take = (BLOCK_SIZE - in_block).min(n - i);
                    let Some(e) = table.get(b * 17..b * 17 + 17) else {
                        out[i..].fill(0);
                        return;
                    };
                    let bmin = i64::from_le_bytes(e[0..8].try_into().unwrap()) as u64;
                    let bits = e[8] as u32;
                    let off = (u64::from_le_bytes(e[9..17].try_into().unwrap()) as usize).min(self.data.len());
                    let mut o = out[i..i + take].iter_mut();
                    direct::for_each_range(&self.data[off..], bits, in_block as u64, take, |q| {
                        if let Some(slot) = o.next() {
                            *slot = bmin.wrapping_add(q.wrapping_mul(*gcd)) as i64;
                        }
                    });
                    i += take;
                }
            }
            Enc::Linear { table } => {
                let mut i = 0usize;
                while i < n {
                    let index = start + i as u64;
                    let in_block = (index & (LINEAR_BLOCK as u64 - 1)) as usize;
                    let take = (LINEAR_BLOCK - in_block).min(n - i);
                    let (base, slope, bits, data) = self.linear_block(table, (index >> LINEAR_SHIFT) as usize);
                    // Incremental trend: no multiply per value.
                    let mut trend = base.wrapping_add(slope.wrapping_mul(in_block as u64));
                    let mut o = out[i..i + take].iter_mut();
                    direct::for_each_range(data, bits, in_block as u64, take, |r| {
                        if let Some(slot) = o.next() {
                            *slot = trend.wrapping_add(r) as i64;
                            trend = trend.wrapping_add(slope);
                        }
                    });
                    i += take;
                }
            }
        }
    }

    /// Smallest and largest value if cheaply known (const and table encodings).
    pub fn known_range(&self) -> Option<(i64, i64)> {
        match &self.enc {
            Enc::Const(v) => Some((*v, *v)),
            Enc::Table { table, .. } => Some((table[0], *table.last().unwrap())),
            _ => None,
        }
    }
}

fn take(buf: &Slice, r: &mut In<'_>, n: usize) -> Result<Slice> {
    let s = buf
        .try_slice(r.pos() as u64, n as u64)
        .ok_or_else(|| corrupt("packed data truncated"))?;
    r.skip(n)?;
    Ok(s)
}

/// Writes a non-decreasing sequence (append-only: the block table is computed first).
pub fn write_monotonic<O: Out + ?Sized>(values: &[u64], out: &mut O) {
    write_monotonic_iter(values.len(), || values.iter().copied(), out);
}

/// Per block of [`LINEAR_BLOCK`] values: `(base, slope, width)` such that value `j` of the
/// block is `base + slope * j + residual_j` with `residual_j < 2^width`; `None` when a block's
/// residuals do not fit 64 bits.
fn linear_blocks(n: usize, get: &impl Fn(usize) -> i64) -> Option<Vec<(i64, i64, u32)>> {
    let mut out = Vec::with_capacity(n.div_ceil(LINEAR_BLOCK));
    for lo in (0..n).step_by(LINEAR_BLOCK) {
        let len = (n - lo).min(LINEAR_BLOCK);
        let first = get(lo) as i128;
        let slope = if len > 1 { (get(lo + len - 1) as i128 - first) / (len as i128 - 1) } else { 0 };
        let slope = i64::try_from(slope).ok()?;
        let (mut rmin, mut rmax) = (i128::MAX, i128::MIN);
        for j in 0..len {
            let r = get(lo + j) as i128 - slope as i128 * j as i128;
            rmin = rmin.min(r);
            rmax = rmax.max(r);
        }
        let spread = u64::try_from(rmax - rmin).ok()?;
        out.push((i64::try_from(rmin).ok()?, slope, direct::bits_for(spread)));
    }
    Some(out)
}

/// [`write_monotonic`] of the `n` values produced by `values()`, which is called twice (block
/// parameters, then residuals), so sequences derived on the fly (offsets from lengths) need no
/// materialized array.
pub fn write_monotonic_iter<I: Iterator<Item = u64>, O: Out + ?Sized>(n: usize, values: impl Fn() -> I, out: &mut O) {
    let blocks = monotonic_blocks(n, &values);
    write_monotonic_blocks(n, values, &blocks, out);
}

#[inline]
fn residual(chunk: &[u64], avg: f32, i: usize) -> i64 {
    (chunk[i] as i64).wrapping_sub((avg * i as f32) as i64)
}

/// Per block of [`MONO_BLOCK`] values: the residual minimum, the slope and the residual width.
fn monotonic_blocks<I: Iterator<Item = u64>>(n: usize, values: impl Fn() -> I) -> Vec<(i64, f32, u32)> {
    let mut chunk = [0u64; MONO_BLOCK];
    let mut blocks = Vec::with_capacity(n.div_ceil(MONO_BLOCK));
    let mut it = values();
    for b in 0..n.div_ceil(MONO_BLOCK) {
        let len = (n - b * MONO_BLOCK).min(MONO_BLOCK);
        for c in chunk[..len].iter_mut() {
            *c = it.next().unwrap_or(0);
        }
        let chunk = &chunk[..len];
        let avg = if len > 1 {
            (chunk[len - 1].wrapping_sub(chunk[0])) as f64 as f32 / (len - 1) as f32
        } else {
            0.0
        };
        let mut min = i64::MAX;
        for i in 0..len {
            min = min.min(residual(chunk, avg, i));
        }
        let mut maxd = 0u64;
        for i in 0..len {
            maxd = maxd.max(residual(chunk, avg, i).wrapping_sub(min) as u64);
        }
        blocks.push((min, avg, direct::bits_for(maxd)));
    }
    blocks
}

fn write_monotonic_blocks<I: Iterator<Item = u64>, O: Out + ?Sized>(
    n: usize,
    values: impl Fn() -> I,
    blocks: &[(i64, f32, u32)],
    out: &mut O,
) {
    out.put_vu64(n as u64);
    let mut offset = 0u64;
    for (b, &(min, avg, bits)) in blocks.iter().enumerate() {
        out.put_i64(min);
        out.put_f32(avg);
        out.put_u8(bits as u8);
        out.put_u64(offset);
        offset += direct::byte_len((n - b * MONO_BLOCK).min(MONO_BLOCK) as u64, bits) as u64;
    }
    let mut chunk = [0u64; MONO_BLOCK];
    let mut it = values();
    for (b, &(min, avg, bits)) in blocks.iter().enumerate() {
        let len = (n - b * MONO_BLOCK).min(MONO_BLOCK);
        for c in chunk[..len].iter_mut() {
            *c = it.next().unwrap_or(0);
        }
        let chunk = &chunk[..len];
        direct::write((0..len).map(|i| residual(chunk, avg, i).wrapping_sub(min) as u64), bits, out);
    }
}

/// Random access to a sequence written by [`write_monotonic`].
#[derive(Clone, Debug)]
pub struct MonotonicValues {
    len: u64,
    table: Slice,
    data: Slice,
}

impl MonotonicValues {
    pub fn read(buf: &Slice, pos: &mut usize) -> Result<MonotonicValues> {
        let mut r = In::at(buf, *pos);
        let len = r.vu64()?;
        let nblocks = len.div_ceil(MONO_BLOCK as u64) as usize;
        if nblocks > r.remaining() / 21 {
            return Err(corrupt("monotonic table truncated"));
        }
        let table = take(buf, &mut r, nblocks * 21)?;
        let mut total = 0usize;
        for b in 0..nblocks {
            let e = &table[b * 21..b * 21 + 21];
            let bits = e[12] as u32;
            let offset = u64::from_le_bytes(e[13..21].try_into().unwrap()) as usize;
            if bits > 64 || offset != total {
                return Err(corrupt("bad monotonic block table"));
            }
            let cnt = (len as usize - b * MONO_BLOCK).min(MONO_BLOCK) as u64;
            total += direct::byte_len(cnt, bits);
        }
        let data = take(buf, &mut r, total)?;
        *pos = r.pos();
        Ok(MonotonicValues { len, table, data })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, index: u64) -> u64 {
        let b = (index >> MONO_SHIFT) as usize;
        let Some(e) = self.table.get(b * 21..b * 21 + 21) else {
            return 0;
        };
        let min = i64::from_le_bytes(e[0..8].try_into().unwrap());
        let avg = f32::from_le_bytes(e[8..12].try_into().unwrap());
        let bits = e[12] as u32;
        let off = u64::from_le_bytes(e[13..21].try_into().unwrap()) as usize;
        let i = index & (MONO_BLOCK as u64 - 1);
        let d = direct::get(&self.data[off.min(self.data.len())..], bits, i);
        min.wrapping_add((avg * i as f32) as i64)
            .wrapping_add(d as i64) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn round_trip(values: &[i64]) -> usize {
        let mut out = vec![0xAB];
        write_numeric(values, &mut out);
        out.push(0xCD);
        let s = Slice::from(out);
        let mut pos = 1;
        let r = NumericValues::read(&s, &mut pos).unwrap();
        assert_eq!(s[pos], 0xCD);
        assert_eq!(r.len(), values.len() as u64);
        for (i, &v) in values.iter().enumerate() {
            assert_eq!(r.get(i as u64), v, "index {i}");
        }
        s.len()
    }

    #[test]
    fn numeric_encodings() {
        let mut rng = StdRng::seed_from_u64(1);
        round_trip(&[]);
        round_trip(&[5]);
        round_trip(&[i64::MIN, i64::MAX, 0, -1]);
        round_trip(&vec![7; 1000]);
        // table
        let t: Vec<i64> = (0..5000)
            .map(|_| [3i64, -1_000_000_000_000, 77][rng.gen_range(0..3)])
            .collect();
        let size = round_trip(&t);
        assert!(size < 2000, "table encoding expected, got {size}");
        // gcd
        let g: Vec<i64> = (0..5000)
            .map(|_| 1_600_000_000_000 + rng.gen_range(0..100_000) * 1000)
            .collect();
        let size = round_trip(&g);
        assert!(size < 5000 * 3, "gcd encoding expected, got {size}");
        // blocks: increasing timestamps
        let b: Vec<i64> = (0..100_000)
            .map(|i| 1_600_000_000_000 + i * 977 + rng.gen_range(0..50))
            .collect();
        let size = round_trip(&b);
        assert!(size < 100_000 * 3, "block encoding expected, got {size}");
        // random full range
        let r: Vec<i64> = (0..3000).map(|_| rng.r#gen()).collect();
        round_trip(&r);
        for n in [127usize, 128, 129, 16383, 16384, 16385] {
            let v: Vec<i64> = (0..n as i64).map(|i| i * i).collect();
            round_trip(&v);
        }
    }

    #[test]
    fn sorted_values_use_the_linear_encoding() {
        let mut rng = StdRng::seed_from_u64(5);
        // Millisecond timestamps of time-ordered docs: ~2 bytes per value at most.
        let mut t = 1_700_000_000_000i64;
        let ts: Vec<i64> = (0..100_000)
            .map(|_| {
                t += rng.gen_range(0..40);
                t
            })
            .collect();
        let size = round_trip(&ts);
        assert!(size < ts.len() * 2, "{size} bytes");
        let mut out = Vec::new();
        write_numeric(&ts, &mut out);
        let r = NumericValues::read(&Slice::from(out), &mut 0).unwrap();
        assert!(matches!(r.enc, Enc::Linear { .. }));
        let mut buf = vec![0i64; 3000];
        r.get_range(50_000, &mut buf);
        assert_eq!(&buf[..], &ts[50_000..53_000]);
        let mut slots: Vec<u64> = vec![3, 99_999, 7, 1024, 1023];
        let mut vals = vec![0i64; slots.len()];
        r.gather(&mut vals, |_| 0, |v| v);
        for (s, v) in slots.iter_mut().zip(vals.iter_mut()) {
            *v = r.get(*s);
            assert_eq!(*v, ts[*s as usize]);
        }
        // Edge sizes and extremes.
        for v in [vec![i64::MIN, i64::MAX], vec![-5, -5, -4, 10], (0..1025).collect::<Vec<i64>>()] {
            round_trip(&v);
        }
    }

    #[test]
    fn monotonic_round_trips() {
        let mut rng = StdRng::seed_from_u64(2);
        for n in [0usize, 1, 2, 1023, 1024, 1025, 5000] {
            let mut acc = 0u64;
            let v: Vec<u64> = (0..n)
                .map(|_| {
                    acc += rng.gen_range(0..100);
                    acc
                })
                .collect();
            let mut out = Vec::new();
            write_monotonic(&v, &mut out);
            let s = Slice::from(out);
            let mut pos = 0;
            let m = MonotonicValues::read(&s, &mut pos).unwrap();
            assert_eq!(pos, s.len());
            for (i, &x) in v.iter().enumerate() {
                assert_eq!(m.get(i as u64), x);
            }
        }
        // Huge jumps.
        let v = vec![0u64, 1 << 40, (1 << 40) + 1, u64::MAX / 4];
        let mut out = Vec::new();
        write_monotonic(&v, &mut out);
        let m = MonotonicValues::read(&Slice::from(out), &mut 0).unwrap();
        for (i, &x) in v.iter().enumerate() {
            assert_eq!(m.get(i as u64), x);
        }
    }

    #[test]
    fn corrupt_headers_are_errors() {
        let mut out = Vec::new();
        write_numeric(&(0..1000).collect::<Vec<i64>>(), &mut out);
        for n in 0..out.len().min(40) {
            assert!(NumericValues::read(&Slice::from(out[..n].to_vec()), &mut 0).is_err());
        }
    }
}
