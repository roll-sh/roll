//! Vector distance kernels over little-endian byte storage.
//!
//! Stored vectors are byte slices (memory-mapped, unaligned), so every kernel takes `&[u8]`:
//! `f32` kernels read 4-byte little-endian lanes, `i8` kernels signed bytes, `u8` kernels the
//! unsigned codes of scalar quantization, and `hamming` packed bits.
//!
//! On x86-64 the AVX2+FMA versions are chosen at runtime (`is_x86_feature_detected!` caches the
//! CPUID result); elsewhere, and on older CPUs, the portable versions run. The portable float
//! kernels keep 8 independent accumulators so LLVM vectorizes them without reassociating a single
//! sum (which IEEE semantics forbid); integer kernels vectorize as written. Float results of the
//! two paths differ only by summation order and are tested against each other with a relative
//! tolerance; integer results are tested for equality.

/// Hints the CPU to load `bytes` (up to 1 KiB) into cache; a no-op where unsupported.
#[inline(always)]
pub fn prefetch(bytes: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        let n = bytes.len().min(1024);
        let mut off = 0;
        while off < n {
            // SAFETY: prefetching never faults, and the address lies within `bytes`.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(bytes.as_ptr().add(off) as *const i8) };
            off += 64;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = bytes;
}

/// Dot product of two little-endian `f32` vectors.
#[inline]
pub fn dot_f32(a: &[u8], b: &[u8]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 and FMA were detected; lengths are equal and reads stay in bounds.
            return unsafe { x86::dot_f32(a, b) };
        }
    }
    dot_f32_portable(a, b)
}

/// Squared Euclidean distance of two little-endian `f32` vectors.
#[inline]
pub fn l2sq_f32(a: &[u8], b: &[u8]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: as for `dot_f32`.
            return unsafe { x86::l2sq_f32(a, b) };
        }
    }
    l2sq_f32_portable(a, b)
}

/// Dot product of two signed-byte vectors.
#[inline]
pub fn dot_i8(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 detected; equal lengths.
            return unsafe { x86::dot_i8(a, b) };
        }
    }
    dot_i8_portable(a, b)
}

/// Squared Euclidean distance of two signed-byte vectors.
#[inline]
pub fn l2sq_i8(a: &[u8], b: &[u8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as i8 as i32 - y as i8 as i32;
            d * d
        })
        .sum()
}

/// Dot product of two unsigned-byte vectors (quantized codes, at most 127 each).
#[inline]
pub fn dot_u8(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 detected; equal lengths.
            return unsafe { x86::dot_u8(a, b) };
        }
    }
    dot_u8_portable(a, b)
}

/// Squared distance of two unsigned-byte vectors (quantized codes, at most 127 each).
#[inline]
pub fn l2sq_u8(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 detected; equal lengths.
            return unsafe { x86::l2sq_u8(a, b) };
        }
    }
    l2sq_u8_portable(a, b)
}

/// Portable [`l2sq_u8`].
pub fn l2sq_u8_portable(a: &[u8], b: &[u8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as i32 - y as i32;
            d * d
        })
        .sum()
}

/// Dot product of two packed 4-bit code vectors (two codes per byte, same layout on both
/// sides).
#[inline]
pub fn dot_u4(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 detected; equal lengths.
            return unsafe { x86::dot_u4(a, b) };
        }
    }
    dot_u4_portable(a, b)
}

/// Portable [`dot_u4`]: two independent byte-wise products, which LLVM vectorizes.
pub fn dot_u4_portable(a: &[u8], b: &[u8]) -> i32 {
    let mut lo = 0i32;
    let mut hi = 0i32;
    for (&x, &y) in a.iter().zip(b) {
        lo += (x & 15) as i32 * (y & 15) as i32;
        hi += (x >> 4) as i32 * (y >> 4) as i32;
    }
    lo + hi
}

/// Squared distance of two packed 4-bit code vectors.
#[inline]
pub fn l2sq_u4(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2_fma() {
            // SAFETY: AVX2 detected; equal lengths.
            return unsafe { x86::l2sq_u4(a, b) };
        }
    }
    l2sq_u4_portable(a, b)
}

/// Portable [`l2sq_u4`].
pub fn l2sq_u4_portable(a: &[u8], b: &[u8]) -> i32 {
    let mut s = 0i32;
    for (&x, &y) in a.iter().zip(b) {
        let dl = (x & 15) as i32 - (y & 15) as i32;
        let dh = (x >> 4) as i32 - (y >> 4) as i32;
        s += dl * dl + dh * dh;
    }
    s
}

/// Number of differing bits.
#[inline]
pub fn hamming(a: &[u8], b: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("popcnt") {
            // SAFETY: POPCNT was detected.
            return unsafe { hamming_popcnt(a, b) };
        }
    }
    hamming_portable(a, b)
}

/// [`hamming_portable`] compiled with the `popcnt` instruction.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
unsafe fn hamming_popcnt(a: &[u8], b: &[u8]) -> u32 {
    hamming_portable(a, b)
}

/// Portable [`hamming`].
#[inline(always)]
pub fn hamming_portable(a: &[u8], b: &[u8]) -> u32 {
    let mut n = 0u32;
    let mut ca = a.chunks_exact(8);
    let mut cb = b.chunks_exact(8);
    for (x, y) in (&mut ca).zip(&mut cb) {
        let x = u64::from_le_bytes(x.try_into().unwrap());
        let y = u64::from_le_bytes(y.try_into().unwrap());
        n += (x ^ y).count_ones();
    }
    for (x, y) in ca.remainder().iter().zip(cb.remainder()) {
        n += (x ^ y).count_ones();
    }
    n
}

#[inline]
fn f(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

/// Portable dot product (8 accumulators, auto-vectorized).
pub fn dot_f32_portable(a: &[u8], b: &[u8]) -> f32 {
    portable_f32(a, b, |x, y| x * y)
}

/// Portable squared distance.
pub fn l2sq_f32_portable(a: &[u8], b: &[u8]) -> f32 {
    portable_f32(a, b, |x, y| (x - y) * (x - y))
}

/// Sums `op(a[i], b[i])` with 8 independent lanes over 32-byte chunks: fixed-size chunks let
/// LLVM drop bounds checks and turn the lanes into one vector register.
#[inline(always)]
fn portable_f32(a: &[u8], b: &[u8], op: impl Fn(f32, f32) -> f32) -> f32 {
    let mut acc = [0f32; 8];
    let ca = a.chunks_exact(32);
    let cb = b.chunks_exact(32);
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        let x: &[u8; 32] = x.try_into().unwrap();
        let y: &[u8; 32] = y.try_into().unwrap();
        for (l, s) in acc.iter_mut().enumerate() {
            let xv = f32::from_le_bytes([x[4 * l], x[4 * l + 1], x[4 * l + 2], x[4 * l + 3]]);
            let yv = f32::from_le_bytes([y[4 * l], y[4 * l + 1], y[4 * l + 2], y[4 * l + 3]]);
            *s += op(xv, yv);
        }
    }
    let mut s = ((acc[0] + acc[4]) + (acc[1] + acc[5])) + ((acc[2] + acc[6]) + (acc[3] + acc[7]));
    for i in 0..ra.len().min(rb.len()) / 4 {
        s += op(f(ra, i), f(rb, i));
    }
    s
}

pub fn dot_i8_portable(a: &[u8], b: &[u8]) -> i32 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| x as i8 as i32 * y as i8 as i32)
        .sum()
}

pub fn dot_u8_portable(a: &[u8], b: &[u8]) -> i32 {
    a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum()
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn has_avx2_fma() -> bool {
    std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn hsum(v: __m256) -> f32 {
        let lo = _mm256_castps256_ps128(v);
        let hi = _mm256_extractf128_ps::<1>(v);
        let s = _mm_add_ps(lo, hi);
        let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
        let s = _mm_add_ss(s, _mm_shuffle_ps::<0x55>(s, s));
        _mm_cvtss_f32(s)
    }

    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn hsum_i32(v: __m256i) -> i32 {
        let lo = _mm256_castsi256_si128(v);
        let hi = _mm256_extracti128_si256::<1>(v);
        let s = _mm_add_epi32(lo, hi);
        let s = _mm_add_epi32(s, _mm_shuffle_epi32::<0x4E>(s));
        let s = _mm_add_epi32(s, _mm_shuffle_epi32::<0xB1>(s));
        _mm_cvtsi128_si32(s)
    }

    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot_f32(a: &[u8], b: &[u8]) -> f32 {
        let n = a.len().min(b.len()) / 4;
        let (pa, pb) = (a.as_ptr() as *const f32, b.as_ptr() as *const f32);
        // SAFETY: all offsets are < n floats, within both slices; loads are unaligned.
        unsafe {
            let mut acc = [_mm256_setzero_ps(); 4];
            let mut i = 0;
            while i + 32 <= n {
                for (k, a) in acc.iter_mut().enumerate() {
                    let x = _mm256_loadu_ps(pa.add(i + 8 * k));
                    let y = _mm256_loadu_ps(pb.add(i + 8 * k));
                    *a = _mm256_fmadd_ps(x, y, *a);
                }
                i += 32;
            }
            while i + 8 <= n {
                acc[0] = _mm256_fmadd_ps(
                    _mm256_loadu_ps(pa.add(i)),
                    _mm256_loadu_ps(pb.add(i)),
                    acc[0],
                );
                i += 8;
            }
            let v = _mm256_add_ps(_mm256_add_ps(acc[0], acc[1]), _mm256_add_ps(acc[2], acc[3]));
            let mut s = hsum(v);
            while i < n {
                s += pa.add(i).read_unaligned() * pb.add(i).read_unaligned();
                i += 1;
            }
            s
        }
    }

    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn l2sq_f32(a: &[u8], b: &[u8]) -> f32 {
        let n = a.len().min(b.len()) / 4;
        let (pa, pb) = (a.as_ptr() as *const f32, b.as_ptr() as *const f32);
        // SAFETY: as in `dot_f32`.
        unsafe {
            let mut acc = [_mm256_setzero_ps(); 4];
            let mut i = 0;
            while i + 32 <= n {
                for (k, a) in acc.iter_mut().enumerate() {
                    let d = _mm256_sub_ps(
                        _mm256_loadu_ps(pa.add(i + 8 * k)),
                        _mm256_loadu_ps(pb.add(i + 8 * k)),
                    );
                    *a = _mm256_fmadd_ps(d, d, *a);
                }
                i += 32;
            }
            while i + 8 <= n {
                let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
                acc[0] = _mm256_fmadd_ps(d, d, acc[0]);
                i += 8;
            }
            let v = _mm256_add_ps(_mm256_add_ps(acc[0], acc[1]), _mm256_add_ps(acc[2], acc[3]));
            let mut s = hsum(v);
            while i < n {
                let d = pa.add(i).read_unaligned() - pb.add(i).read_unaligned();
                s += d * d;
                i += 1;
            }
            s
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_i8(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len().min(b.len());
        // SAFETY: 16-byte loads at offsets i + 16 <= n.
        unsafe {
            let mut acc = _mm256_setzero_si256();
            let mut i = 0;
            while i + 16 <= n {
                let x = _mm256_cvtepi8_epi16(_mm_loadu_si128(a.as_ptr().add(i) as *const __m128i));
                let y = _mm256_cvtepi8_epi16(_mm_loadu_si128(b.as_ptr().add(i) as *const __m128i));
                acc = _mm256_add_epi32(acc, _mm256_madd_epi16(x, y));
                i += 16;
            }
            let mut s = hsum_i32(acc);
            while i < n {
                s += a[i] as i8 as i32 * b[i] as i8 as i32;
                i += 1;
            }
            s
        }
    }

    /// Sums the 16-bit lanes of `v` into 32-bit lanes added to `acc`.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn widen_add(acc: __m256i, v: __m256i) -> __m256i {
        _mm256_add_epi32(acc, _mm256_madd_epi16(v, _mm256_set1_epi16(1)))
    }

    /// Codes are at most 127, so `pmaddubsw` (unsigned × signed bytes, pairs summed into
    /// 16 bits) cannot saturate: 2 · 127² < 2¹⁵.
    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_u8(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len().min(b.len());
        // SAFETY: 32-byte loads at offsets i + 32 <= n.
        unsafe {
            let mut acc = _mm256_setzero_si256();
            let mut i = 0;
            while i + 32 <= n {
                let x = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
                let y = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
                acc = widen_add(acc, _mm256_maddubs_epi16(x, y));
                i += 32;
            }
            let mut s = hsum_i32(acc);
            while i < n {
                s += a[i] as i32 * b[i] as i32;
                i += 1;
            }
            s
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn l2sq_u8(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len().min(b.len());
        // SAFETY: 32-byte loads at offsets i + 32 <= n.
        unsafe {
            let mut acc = _mm256_setzero_si256();
            let mut i = 0;
            while i + 32 <= n {
                let x = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
                let y = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
                let d = _mm256_abs_epi8(_mm256_sub_epi8(x, y));
                acc = widen_add(acc, _mm256_maddubs_epi16(d, d));
                i += 32;
            }
            let mut s = hsum_i32(acc);
            while i < n {
                let d = a[i] as i32 - b[i] as i32;
                s += d * d;
                i += 1;
            }
            s
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_u4(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len().min(b.len());
        // SAFETY: 32-byte loads at offsets i + 32 <= n.
        unsafe {
            let mask = _mm256_set1_epi8(0x0F);
            let mut acc = _mm256_setzero_si256();
            let mut i = 0;
            while i + 32 <= n {
                let x = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
                let y = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
                let (xl, yl) = (_mm256_and_si256(x, mask), _mm256_and_si256(y, mask));
                let xh = _mm256_and_si256(_mm256_srli_epi16::<4>(x), mask);
                let yh = _mm256_and_si256(_mm256_srli_epi16::<4>(y), mask);
                let p = _mm256_add_epi16(_mm256_maddubs_epi16(xl, yl), _mm256_maddubs_epi16(xh, yh));
                acc = widen_add(acc, p);
                i += 32;
            }
            let mut s = hsum_i32(acc);
            while i < n {
                let (x, y) = (a[i], b[i]);
                s += (x & 15) as i32 * (y & 15) as i32 + (x >> 4) as i32 * (y >> 4) as i32;
                i += 1;
            }
            s
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn l2sq_u4(a: &[u8], b: &[u8]) -> i32 {
        let n = a.len().min(b.len());
        // SAFETY: 32-byte loads at offsets i + 32 <= n.
        unsafe {
            let mask = _mm256_set1_epi8(0x0F);
            let mut acc = _mm256_setzero_si256();
            let mut i = 0;
            while i + 32 <= n {
                let x = _mm256_loadu_si256(a.as_ptr().add(i) as *const __m256i);
                let y = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
                let dl = _mm256_abs_epi8(_mm256_sub_epi8(_mm256_and_si256(x, mask), _mm256_and_si256(y, mask)));
                let dh = _mm256_abs_epi8(_mm256_sub_epi8(
                    _mm256_and_si256(_mm256_srli_epi16::<4>(x), mask),
                    _mm256_and_si256(_mm256_srli_epi16::<4>(y), mask),
                ));
                let p = _mm256_add_epi16(_mm256_maddubs_epi16(dl, dl), _mm256_maddubs_epi16(dh, dh));
                acc = widen_add(acc, p);
                i += 32;
            }
            let mut s = hsum_i32(acc);
            while i < n {
                let (x, y) = (a[i], b[i]);
                let dl = (x & 15) as i32 - (y & 15) as i32;
                let dh = (x >> 4) as i32 - (y >> 4) as i32;
                s += dl * dl + dh * dh;
                i += 1;
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn simd_matches_portable() {
        let mut rng = StdRng::seed_from_u64(1);
        for dims in [0usize, 1, 7, 8, 9, 31, 32, 33, 100, 384, 1024] {
            let a: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let b: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let (ab, bb) = (bytes(&a), bytes(&b));
            let exact_dot: f64 = a.iter().zip(&b).map(|(x, y)| *x as f64 * *y as f64).sum();
            let exact_l2: f64 = a
                .iter()
                .zip(&b)
                .map(|(x, y)| (*x as f64 - *y as f64).powi(2))
                .sum();
            for (got, want) in [
                (dot_f32(&ab, &bb), exact_dot),
                (dot_f32_portable(&ab, &bb), exact_dot),
                (l2sq_f32(&ab, &bb), exact_l2),
                (l2sq_f32_portable(&ab, &bb), exact_l2),
            ] {
                assert!(
                    (got as f64 - want).abs() <= 1e-4 * (1.0 + want.abs()),
                    "dims {dims}: {got} vs {want}"
                );
            }
            let ia: Vec<u8> = (0..dims).map(|_| rng.r#gen()).collect();
            let ib: Vec<u8> = (0..dims).map(|_| rng.r#gen()).collect();
            assert_eq!(dot_i8(&ia, &ib), dot_i8_portable(&ia, &ib));
            let ua: Vec<u8> = ia.iter().map(|x| x & 127).collect();
            let ub: Vec<u8> = ib.iter().map(|x| x & 127).collect();
            assert_eq!(dot_u8(&ua, &ub), dot_u8_portable(&ua, &ub));
            assert_eq!(l2sq_u8(&ua, &ub), l2sq_u8_portable(&ua, &ub));
            assert_eq!(dot_u4(&ia, &ib), dot_u4_portable(&ia, &ib));
            assert_eq!(l2sq_u4(&ia, &ib), l2sq_u4_portable(&ia, &ib));
            let max = vec![127u8; dims];
            assert_eq!(dot_u8(&max, &max), 127 * 127 * dims as i32);
            assert_eq!(l2sq_u8(&max, &vec![0u8; dims]), 127 * 127 * dims as i32);
            let ff = vec![0xFFu8; dims];
            assert_eq!(dot_u4(&ff, &ff), 2 * 225 * dims as i32);
            assert_eq!(l2sq_u4(&ff, &vec![0u8; dims]), 2 * 225 * dims as i32);
            let h: u32 = ia.iter().zip(&ib).map(|(x, y)| (x ^ y).count_ones()).sum();
            assert_eq!(hamming(&ia, &ib), h);
            assert_eq!(hamming_portable(&ia, &ib), h);
        }
    }

    #[test]
    #[ignore]
    fn bench_distances() {
        let mut rng = StdRng::seed_from_u64(1);
        let dims = 768;
        let n = 10_000;
        let data: Vec<u8> = (0..n * dims)
            .flat_map(|_| rng.gen_range(-1.0f32..1.0).to_le_bytes())
            .collect();
        let q: Vec<u8> = data[..dims * 4].to_vec();
        let t = std::time::Instant::now();
        let mut s = 0f32;
        for _ in 0..20 {
            for v in data.chunks_exact(dims * 4) {
                s += dot_f32(&q, v);
            }
        }
        let e = t.elapsed();
        println!(
            "dot_f32 768d: {:.1} ns/vector ({s})",
            e.as_nanos() as f64 / (20 * n) as f64
        );
        let t = std::time::Instant::now();
        for _ in 0..20 {
            for v in data.chunks_exact(dims * 4) {
                s += dot_f32_portable(&q, v);
            }
        }
        let e = t.elapsed();
        println!(
            "dot_f32 portable 768d: {:.1} ns/vector ({s})",
            e.as_nanos() as f64 / (20 * n) as f64
        );
    }
}
