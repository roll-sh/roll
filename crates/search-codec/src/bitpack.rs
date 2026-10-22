//! Bit packing of 128-integer blocks, FOR and PFOR, with SIMD decoding.
//!
//! # Layout
//!
//! A block of 128 `u32`s packed at `b` bits takes exactly `16 * b` bytes. Values are interleaved
//! over four 32-bit lanes (the SIMD-BP128 layout): value `i` lives in lane `i % 4`, and each lane
//! packs its 32 values back to back into `b` little-endian words; word `k` of lane `l` is stored
//! at word index `4k + l`. One 128-bit load therefore yields one word of every lane, and a
//! shift-and-mask produces four consecutive values in natural order, so SSE2 (baseline on
//! x86-64) and NEON (baseline on aarch64) decode without shuffles and without runtime feature
//! detection. The scalar decoder reads the same layout and is tested against the SIMD one.
//!
//! Every width is a separate monomorphized function (`const B`), so shifts are constants and
//! LLVM unrolls the 32-step lane loops.
//!
//! # FOR / PFOR
//!
//! [`for_encode`] writes a width byte followed by the packed block. [`pfor_encode`] picks the
//! width minimizing the encoded size when the largest few values are stored as exceptions
//! (index byte + varint of the high bits), which keeps a single long document or a large position
//! gap from inflating the whole block. Exceptions are capped so patching stays cheap.

use crate::io::read_vu32;

/// Values per block.
pub const BLOCK: usize = 128;

/// Upper bound of exceptions per PFOR block.
const MAX_EXCEPTIONS: usize = 24;

/// Bits needed to represent `v` (0 for 0).
#[inline]
pub fn bits_required(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// Bits needed for the largest value of the block.
#[inline]
pub fn max_bits(values: &[u32; BLOCK]) -> u32 {
    bits_required(values.iter().fold(0, |a, &v| a | v))
}

/// Packed size of a block at `bits`.
#[inline]
pub const fn packed_len(bits: u32) -> usize {
    16 * bits as usize
}

/// Packs `values` (each must fit in `bits`) and appends `16 * bits` bytes to `out`.
pub fn pack(values: &[u32; BLOCK], bits: u32, out: &mut Vec<u8>) {
    debug_assert!(bits <= 32);
    let start = out.len();
    out.resize(start + packed_len(bits), 0);
    if bits == 0 {
        return;
    }
    let dst = &mut out[start..];
    let mask = mask64(bits);
    for lane in 0..4 {
        let mut acc = 0u64;
        let mut filled = 0u32;
        let mut k = 0usize;
        for j in 0..32 {
            acc |= (values[4 * j + lane] as u64 & mask) << filled;
            filled += bits;
            if filled >= 32 {
                let at = (4 * k + lane) * 4;
                dst[at..at + 4].copy_from_slice(&(acc as u32).to_le_bytes());
                k += 1;
                acc >>= 32;
                filled -= 32;
            }
        }
    }
}

#[inline(always)]
const fn mask64(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

macro_rules! dispatch_bits {
    ($bits:expr, $f:ident, $($arg:expr),*) => {
        match $bits {
            0 => $f::<0>($($arg),*), 1 => $f::<1>($($arg),*), 2 => $f::<2>($($arg),*),
            3 => $f::<3>($($arg),*), 4 => $f::<4>($($arg),*), 5 => $f::<5>($($arg),*),
            6 => $f::<6>($($arg),*), 7 => $f::<7>($($arg),*), 8 => $f::<8>($($arg),*),
            9 => $f::<9>($($arg),*), 10 => $f::<10>($($arg),*), 11 => $f::<11>($($arg),*),
            12 => $f::<12>($($arg),*), 13 => $f::<13>($($arg),*), 14 => $f::<14>($($arg),*),
            15 => $f::<15>($($arg),*), 16 => $f::<16>($($arg),*), 17 => $f::<17>($($arg),*),
            18 => $f::<18>($($arg),*), 19 => $f::<19>($($arg),*), 20 => $f::<20>($($arg),*),
            21 => $f::<21>($($arg),*), 22 => $f::<22>($($arg),*), 23 => $f::<23>($($arg),*),
            24 => $f::<24>($($arg),*), 25 => $f::<25>($($arg),*), 26 => $f::<26>($($arg),*),
            27 => $f::<27>($($arg),*), 28 => $f::<28>($($arg),*), 29 => $f::<29>($($arg),*),
            30 => $f::<30>($($arg),*), 31 => $f::<31>($($arg),*), _ => $f::<32>($($arg),*),
        }
    };
}

/// Unpacks one block of `bits`-wide values from `input` with the fastest available decoder.
///
/// `input` should hold at least `16 * bits` bytes; a short (corrupt) input decodes as if
/// zero-padded instead of reading out of bounds.
#[inline]
pub fn unpack(input: &[u8], bits: u32, out: &mut [u32; BLOCK]) {
    let bits = bits.min(32);
    let need = packed_len(bits);
    if input.len() < need {
        let mut padded = [0u8; 16 * 32];
        padded[..input.len()].copy_from_slice(input);
        return unpack(&padded[..need], bits, out);
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: SSE2 is part of the x86-64 baseline; `input` holds `need` bytes (checked
        // above) and `out` holds 128 values.
        unsafe { dispatch_bits!(bits, unpack_sse2, input.as_ptr(), out.as_mut_ptr()) }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of the aarch64 baseline; bounds as above.
        unsafe { dispatch_bits!(bits, unpack_neon, input.as_ptr(), out.as_mut_ptr()) }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        unpack_scalar(input, bits, out)
    }
}

/// Portable decoder; the reference the SIMD paths are tested against.
pub fn unpack_scalar(input: &[u8], bits: u32, out: &mut [u32; BLOCK]) {
    let bits = bits.min(32);
    let need = packed_len(bits);
    if input.len() < need {
        let mut padded = [0u8; 16 * 32];
        padded[..input.len()].copy_from_slice(input);
        return unpack_scalar(&padded[..need], bits, out);
    }
    dispatch_bits!(bits, unpack_scalar_b, input, out)
}

#[inline(always)]
fn unpack_scalar_b<const B: u32>(input: &[u8], out: &mut [u32; BLOCK]) {
    let mask = mask64(B);
    let word = |i: usize| u32::from_le_bytes(input[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    for lane in 0..4 {
        let mut acc = 0u64;
        let mut avail = 0u32;
        let mut k = 0usize;
        for j in 0..32 {
            if avail < B {
                acc |= word(4 * k + lane) << avail;
                k += 1;
                avail += 32;
            }
            out[4 * j + lane] = (acc & mask) as u32;
            acc >>= B;
            avail -= B;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn unpack_sse2<const B: u32>(input: *const u8, out: *mut u32) {
    use std::arch::x86_64::*;
    // SAFETY (whole body): the caller guarantees `16 * B` readable bytes at `input` and 128
    // writable values at `out`; every load/store below stays within those bounds.
    unsafe {
        if B == 0 {
            std::ptr::write_bytes(out, 0, BLOCK);
            return;
        }
        let src = input as *const __m128i;
        let mask = _mm_set1_epi32(mask64(B) as u32 as i32);
        let mut w = _mm_loadu_si128(src);
        let mut next = 1usize;
        let mut off = 0u32;
        for j in 0..32 {
            let mut v = _mm_srl_epi32(w, _mm_cvtsi32_si128(off as i32));
            let end = off + B;
            if end >= 32 {
                if next < B as usize {
                    w = _mm_loadu_si128(src.add(next));
                    next += 1;
                    if end > 32 {
                        v = _mm_or_si128(v, _mm_sll_epi32(w, _mm_cvtsi32_si128((32 - off) as i32)));
                    }
                }
                off = end - 32;
            } else {
                off = end;
            }
            _mm_storeu_si128(out.add(4 * j) as *mut __m128i, _mm_and_si128(v, mask));
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn unpack_neon<const B: u32>(input: *const u8, out: *mut u32) {
    use std::arch::aarch64::*;
    // SAFETY: as for `unpack_sse2`.
    unsafe {
        if B == 0 {
            std::ptr::write_bytes(out, 0, BLOCK);
            return;
        }
        let src = input as *const u32;
        let mask = vdupq_n_u32(mask64(B) as u32);
        let mut w = vld1q_u32(src);
        let mut next = 1usize;
        let mut off = 0u32;
        for j in 0..32 {
            let mut v = vshlq_u32(w, vdupq_n_s32(-(off as i32)));
            let end = off + B;
            if end >= 32 {
                if next < B as usize {
                    w = vld1q_u32(src.add(4 * next));
                    next += 1;
                    if end > 32 {
                        v = vorrq_u32(v, vshlq_u32(w, vdupq_n_s32((32 - off) as i32)));
                    }
                }
                off = end - 32;
            } else {
                off = end;
            }
            vst1q_u32(out.add(4 * j), vandq_u32(v, mask));
        }
    }
}

/// In-place prefix sum: `out[i] = base + out[0] + ... + out[i]` (wrapping).
#[inline]
pub fn prefix_sum(values: &mut [u32; BLOCK], base: u32) {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: SSE2 is baseline; 32 vectors of 4 values cover the block exactly.
        unsafe {
            use std::arch::x86_64::*;
            let p = values.as_mut_ptr() as *mut __m128i;
            let mut carry = _mm_set1_epi32(base as i32);
            for i in 0..32 {
                let mut v = _mm_loadu_si128(p.add(i));
                v = _mm_add_epi32(v, _mm_slli_si128::<4>(v));
                v = _mm_add_epi32(v, _mm_slli_si128::<8>(v));
                v = _mm_add_epi32(v, carry);
                _mm_storeu_si128(p.add(i), v);
                carry = _mm_shuffle_epi32::<0xFF>(v);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is baseline; 32 vectors of 4 values cover the block exactly.
        unsafe {
            use std::arch::aarch64::*;
            let p = values.as_mut_ptr();
            let zero = vdupq_n_u32(0);
            let mut carry = vdupq_n_u32(base);
            for i in 0..32 {
                let mut v = vld1q_u32(p.add(4 * i));
                v = vaddq_u32(v, vextq_u32::<3>(zero, v));
                v = vaddq_u32(v, vextq_u32::<2>(zero, v));
                v = vaddq_u32(v, carry);
                vst1q_u32(p.add(4 * i), v);
                carry = vdupq_laneq_u32::<3>(v);
            }
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    prefix_sum_scalar(values, base)
}

/// Portable prefix sum.
pub fn prefix_sum_scalar(values: &mut [u32; BLOCK], base: u32) {
    let mut acc = base;
    for v in values.iter_mut() {
        acc = acc.wrapping_add(*v);
        *v = acc;
    }
}

/// FOR: width byte + packed block.
pub fn for_encode(values: &[u32; BLOCK], out: &mut Vec<u8>) {
    let b = max_bits(values);
    out.push(b as u8);
    pack(values, b, out);
}

/// Decodes a [`for_encode`]d block at `*pos`, advancing it.
#[inline]
pub fn for_decode(buf: &[u8], pos: &mut usize, out: &mut [u32; BLOCK]) {
    let b = buf.get(*pos).copied().unwrap_or(0) as u32 & 63;
    *pos += 1;
    let start = (*pos).min(buf.len());
    unpack(&buf[start..], b, out);
    *pos += packed_len(b.min(32));
}

/// Skips a [`for_encode`]d block.
#[inline]
pub fn for_skip(buf: &[u8], pos: &mut usize) {
    let b = buf.get(*pos).copied().unwrap_or(0) as u32 & 63;
    *pos += 1 + packed_len(b.min(32));
}

/// PFOR: `[width][exception count]` + packed low bits + `(index, high bits varint)` per exception.
pub fn pfor_encode(values: &[u32; BLOCK], out: &mut Vec<u8>) {
    // Exact cost per candidate width from a histogram of value widths: an exception of width
    // `w` at base width `b` costs an index byte plus a varint of its `w - b` high bits.
    let mut hist = [0usize; 33];
    for &v in values.iter() {
        hist[bits_required(v) as usize] += 1;
    }
    let maxb = (0..=32).rev().find(|&w| hist[w] > 0).unwrap_or(0) as u32;
    let mut best_b = maxb;
    let mut best_cost = packed_len(maxb);
    for b in (0..maxb).rev() {
        let mut cost = packed_len(b);
        let mut exceptions = 0usize;
        for w in (b + 1)..=maxb {
            let n = hist[w as usize];
            exceptions += n;
            cost += n * (1 + (w - b).div_ceil(7) as usize);
        }
        if exceptions > MAX_EXCEPTIONS {
            break;
        }
        if cost < best_cost {
            best_cost = cost;
            best_b = b;
        }
    }
    let mask = mask64(best_b) as u32;
    let mut low = [0u32; BLOCK];
    let mut exceptions = 0u8;
    for (i, &v) in values.iter().enumerate() {
        low[i] = v & mask;
        if bits_required(v) > best_b {
            exceptions += 1;
        }
    }
    out.push(best_b as u8);
    out.push(exceptions);
    pack(&low, best_b, out);
    if exceptions > 0 {
        for (i, &v) in values.iter().enumerate() {
            if bits_required(v) > best_b {
                out.push(i as u8);
                crate::io::Out::put_vu32(out, v >> best_b);
            }
        }
    }
}

/// Decodes a [`pfor_encode`]d block at `*pos`, advancing it.
#[inline]
pub fn pfor_decode(buf: &[u8], pos: &mut usize, out: &mut [u32; BLOCK]) {
    let b = buf.get(*pos).copied().unwrap_or(0) as u32 & 63;
    let nex = buf.get(*pos + 1).copied().unwrap_or(0) as usize;
    *pos += 2;
    let start = (*pos).min(buf.len());
    let b = b.min(32);
    unpack(&buf[start..], b, out);
    *pos += packed_len(b);
    for _ in 0..nex {
        let idx = buf.get(*pos).copied().unwrap_or(0) as usize & (BLOCK - 1);
        *pos += 1;
        let high = read_vu32(buf, pos);
        out[idx] |= high.checked_shl(b).unwrap_or(0);
    }
}

/// Skips a [`pfor_encode`]d block.
#[inline]
pub fn pfor_skip(buf: &[u8], pos: &mut usize) {
    let b = (buf.get(*pos).copied().unwrap_or(0) as u32 & 63).min(32);
    let nex = buf.get(*pos + 1).copied().unwrap_or(0) as usize;
    *pos += 2 + packed_len(b);
    for _ in 0..nex {
        *pos += 1;
        read_vu32(buf, pos);
    }
}

/// Fixed-width bit-packed array with random access, for doc values, offsets and ordinals.
///
/// Values are packed little-endian in a contiguous bit stream; writers append 8 zero bytes so
/// that a single unaligned 64-bit load reads any value of up to 57 bits.
pub mod direct {
    use super::mask64;
    use crate::io::Out;

    /// Bytes needed for `count` values at `bits`, including the read padding.
    pub fn byte_len(count: u64, bits: u32) -> usize {
        ((count * bits as u64).div_ceil(8) + 8) as usize
    }

    /// Appends `values` packed at `bits` (0..=64) plus padding.
    pub fn write<O: Out + ?Sized>(values: impl IntoIterator<Item = u64>, bits: u32, out: &mut O) {
        let mut acc = 0u128;
        let mut filled = 0u32;
        if bits > 0 {
            // Whole words collect in a small stack buffer: one sink call per 32 words.
            let mut words = [0u8; 256];
            let mut w = 0usize;
            for v in values {
                acc |= ((v & mask64(bits)) as u128) << filled;
                filled += bits;
                while filled >= 64 {
                    words[w..w + 8].copy_from_slice(&(acc as u64).to_le_bytes());
                    w += 8;
                    if w == words.len() {
                        out.put_bytes(&words);
                        w = 0;
                    }
                    acc >>= 64;
                    filled -= 64;
                }
            }
            out.put_bytes(&words[..w]);
        }
        let tail = filled.div_ceil(8) as usize;
        out.put_bytes(&(acc as u64).to_le_bytes()[..tail]);
        out.put_bytes(&[0u8; 8]);
    }

    /// Reads value `index` from a stream written by [`write`](fn@write). Out-of-range reads return 0.
    #[inline(always)]
    pub fn get(data: &[u8], bits: u32, index: u64) -> u64 {
        if bits == 0 {
            return 0;
        }
        let bits = bits.min(64);
        // Saturating: a corrupt index reads past the end (zeros) instead of overflowing.
        let bit = index.saturating_mul(bits as u64);
        let byte = (bit >> 3).min(usize::MAX as u64 / 2) as usize;
        let shift = (bit & 7) as u32;
        let lo = read_u64(data, byte) >> shift;
        if bits + shift <= 64 {
            lo & mask64(bits)
        } else {
            let hi = data.get(byte + 8).copied().unwrap_or(0) as u64;
            (lo | (hi << (64 - shift))) & mask64(bits)
        }
    }

    /// Calls `f` with the `n` consecutive values from `start`, in order. Faster than `n` calls
    /// to [`get`]: whole-byte widths are plain loads, the others walk the bit stream without
    /// recomputing offsets or checking bounds per value (the range is checked once).
    #[inline]
    pub fn for_each_range(data: &[u8], bits: u32, start: u64, n: usize, mut f: impl FnMut(u64)) {
        let bits = bits.min(64);
        let end_bit = start.saturating_add(n as u64).saturating_mul(bits as u64);
        // The stream's padding makes an 8-byte load at the last value's byte safe; data that
        // is shorter (corrupt or truncated) falls back to the checked path.
        if bits == 0 || end_bit.div_ceil(8) + 8 > data.len() as u64 {
            for i in 0..n as u64 {
                f(get(data, bits, start.saturating_add(i)));
            }
            return;
        }
        let s = start as usize;
        match bits {
            8 => data[s..s + n].iter().for_each(|&b| f(b as u64)),
            16 => data[s * 2..(s + n) * 2].chunks_exact(2).for_each(|c| f(u16::from_le_bytes([c[0], c[1]]) as u64)),
            32 => data[s * 4..(s + n) * 4]
                .chunks_exact(4)
                .for_each(|c| f(u32::from_le_bytes(c.try_into().unwrap()) as u64)),
            64 => data[s * 8..(s + n) * 8]
                .chunks_exact(8)
                .for_each(|c| f(u64::from_le_bytes(c.try_into().unwrap()))),
            _ if bits <= 57 => {
                let mask = mask64(bits);
                let mut bit = start * bits as u64;
                for _ in 0..n {
                    let byte = (bit >> 3) as usize;
                    let w = u64::from_le_bytes(data[byte..byte + 8].try_into().unwrap());
                    f((w >> (bit & 7)) & mask);
                    bit += bits as u64;
                }
            }
            _ => {
                for i in 0..n as u64 {
                    f(get(data, bits, start + i));
                }
            }
        }
    }

    #[inline(always)]
    fn read_u64(data: &[u8], at: usize) -> u64 {
        match data.get(at..at + 8) {
            Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
            None => {
                let mut tmp = [0u8; 8];
                if at < data.len() {
                    let n = data.len() - at;
                    tmp[..n].copy_from_slice(&data[at..]);
                }
                u64::from_le_bytes(tmp)
            }
        }
    }

    /// Bits needed for an unsigned value.
    pub fn bits_for(max: u64) -> u32 {
        64 - max.leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn random_block(rng: &mut StdRng, bits: u32) -> [u32; BLOCK] {
        let mut v = [0u32; BLOCK];
        for x in v.iter_mut() {
            *x = if bits == 0 {
                0
            } else {
                rng.r#gen::<u32>() & (mask64(bits) as u32)
            };
        }
        v
    }

    #[test]
    fn pack_unpack_every_width_simd_and_scalar() {
        let mut rng = StdRng::seed_from_u64(7);
        for bits in 0..=32 {
            for _ in 0..20 {
                let v = random_block(&mut rng, bits);
                let mut buf = Vec::new();
                pack(&v, bits, &mut buf);
                assert_eq!(buf.len(), packed_len(bits));
                let mut a = [0u32; BLOCK];
                let mut b = [1u32; BLOCK];
                unpack(&buf, bits, &mut a);
                unpack_scalar(&buf, bits, &mut b);
                assert_eq!(a, v, "simd bits={bits}");
                assert_eq!(b, v, "scalar bits={bits}");
            }
            // Extreme values: all ones at this width.
            let v = [mask64(bits) as u32; BLOCK];
            let mut buf = Vec::new();
            pack(&v, bits, &mut buf);
            let mut a = [0u32; BLOCK];
            unpack(&buf, bits, &mut a);
            assert_eq!(a, v);
        }
    }

    #[test]
    fn short_input_does_not_panic() {
        let mut out = [0u32; BLOCK];
        unpack(&[0xff; 5], 7, &mut out);
        unpack_scalar(&[0xff; 5], 7, &mut out);
        let mut pos = 0;
        pfor_decode(&[30, 200, 1], &mut pos, &mut out);
        for_decode(&[40], &mut 0, &mut out);
    }

    #[test]
    fn prefix_sum_matches_scalar() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..50 {
            let bits = rng.gen_range(0..=32);
            let mut v = random_block(&mut rng, bits);
            let mut w = v;
            let base = rng.r#gen::<u32>();
            prefix_sum(&mut v, base);
            prefix_sum_scalar(&mut w, base);
            assert_eq!(v, w);
        }
    }

    #[test]
    fn pfor_round_trips_with_outliers() {
        let mut rng = StdRng::seed_from_u64(11);
        for case in 0..200 {
            let bits = rng.gen_range(0..=6);
            let mut v = random_block(&mut rng, bits);
            let outliers = case % 30;
            for _ in 0..outliers {
                let i = rng.gen_range(0..BLOCK);
                v[i] = rng.r#gen::<u32>() >> rng.gen_range(0..32);
            }
            let mut buf = vec![0xAA];
            pfor_encode(&v, &mut buf);
            buf.push(0x55);
            let mut pos = 1;
            let mut out = [0u32; BLOCK];
            pfor_decode(&buf, &mut pos, &mut out);
            assert_eq!(out, v);
            assert_eq!(buf[pos], 0x55);
            let mut spos = 1;
            pfor_skip(&buf, &mut spos);
            assert_eq!(spos, pos);
        }
    }

    #[test]
    fn pfor_beats_for_with_one_outlier() {
        let mut v = [1u32; BLOCK];
        v[17] = 1 << 30;
        let mut p = Vec::new();
        pfor_encode(&v, &mut p);
        let mut f = Vec::new();
        for_encode(&v, &mut f);
        assert!(p.len() < f.len() / 4, "{} vs {}", p.len(), f.len());
    }

    #[test]
    fn direct_round_trips_all_widths() {
        let mut rng = StdRng::seed_from_u64(5);
        for bits in 0..=64u32 {
            for &n in &[0usize, 1, 7, 64, 129] {
                let vals: Vec<u64> = (0..n).map(|_| rng.r#gen::<u64>() & mask64(bits)).collect();
                let mut buf = Vec::new();
                direct::write(vals.iter().copied(), bits, &mut buf);
                assert_eq!(buf.len(), direct::byte_len(n as u64, bits));
                for (i, &v) in vals.iter().enumerate() {
                    assert_eq!(direct::get(&buf, bits, i as u64), v, "bits={bits} i={i}");
                }
            }
        }
    }

    #[test]
    #[ignore]
    fn bench_unpack_throughput() {
        let mut rng = StdRng::seed_from_u64(1);
        for bits in [1u32, 4, 8, 12, 16, 24] {
            let blocks: Vec<Vec<u8>> = (0..1024)
                .map(|_| {
                    let v = random_block(&mut rng, bits);
                    let mut b = Vec::new();
                    pack(&v, bits, &mut b);
                    b
                })
                .collect();
            let mut out = [0u32; BLOCK];
            let mut sink = 0u32;
            let rounds = 2000;
            let t = std::time::Instant::now();
            for _ in 0..rounds {
                for b in &blocks {
                    unpack(b, bits, &mut out);
                    prefix_sum(&mut out, sink);
                    sink = out[127];
                }
            }
            let simd = t.elapsed();
            let t = std::time::Instant::now();
            for _ in 0..rounds {
                for b in &blocks {
                    unpack_scalar(b, bits, &mut out);
                    prefix_sum_scalar(&mut out, sink);
                    sink = out[127];
                }
            }
            let scalar = t.elapsed();
            let ints = (rounds * blocks.len() * BLOCK) as f64;
            println!(
                "bits={bits:2} simd {:.0} M ints/s, scalar {:.0} M ints/s ({sink})",
                ints / simd.as_secs_f64() / 1e6,
                ints / scalar.as_secs_f64() / 1e6
            );
        }
    }
}
