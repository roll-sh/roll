//! Distance kernels, computed as the reference build computes them: the same float
//! operations in the same order, so results are pgvector's bit for bit.
//!
//! The reference is GCC 15 at `-O2 -ftree-vectorize -fassociative-math -fno-signed-zeros
//! -ffp-contract=fast` for baseline x86-64 (SSE2, no FMA). Its "Auto-vectorized" float loops
//! keep four lanes (`acc[j] += term`, i stepping by 4, zero to start) and then reduce them
//! in one of a few ways, each kernel here naming the one its disassembly shows; double sums
//! keep two lanes. halfvec's kernels are function pointers set at load time: AVX + F16C +
//! FMA variants (eight lanes, fused multiply-adds) where the CPU has them, scalar loops
//! otherwise; `half` mirrors that choice.

use super::common::{bytes, f32_at, half_to_f32, u16_at};
use super::sparsevec::Sparse;
use crate::types::Value;

/// Four floats from 16 little-endian bytes.
#[inline(always)]
fn quad(c: &[u8; 16]) -> [f32; 4] {
    let f = |i: usize| f32::from_le_bytes(c[4 * i..4 * i + 4].try_into().unwrap());
    [f(0), f(1), f(2), f(3)]
}

/// The four-lane sums of `term` over the first `n & !3` elements of `a` and `b` (zero
/// lanes when there are fewer than four).
#[inline(always)]
fn lanes(a: &[u8], b: &[u8], n: usize, term: impl Fn(f32, f32) -> f32) -> [f32; 4] {
    let main = 4 * (n & !3);
    let mut acc = [0.0f32; 4];
    for (ca, cb) in a[..main].as_chunks::<16>().0.iter().zip(b[..main].as_chunks::<16>().0) {
        let (x, y) = (quad(ca), quad(cb));
        for j in 0..4 {
            acc[j] += term(x[j], y[j]);
        }
    }
    acc
}

/// `lanes` of four rows `a` against one `b` (each `n` floats), their sums interleaved:
/// independent chains, so one row's additions overlap another's, each lane computed as
/// `lanes` computes it.
#[inline(always)]
fn lanes4(a: &[&[u8]; 4], b: &[u8], n: usize, term: impl Fn(f32, f32) -> f32) -> [[f32; 4]; 4] {
    let main = 4 * (n & !3);
    let mut acc = [[0.0f32; 4]; 4];
    let cb = b[..main].as_chunks::<16>().0;
    let ca = a.map(|a| a[..main].as_chunks::<16>().0);
    for (i, cb) in cb.iter().enumerate() {
        let y = quad(cb);
        for r in 0..4 {
            let x = quad(&ca[r][i]);
            for j in 0..4 {
                acc[r][j] += term(x[j], y[j]);
            }
        }
    }
    acc
}

/// Whether four rows can go through a kernel's four-row form against `b`: each as long.
fn same_len(a: &[&[u8]; 4], b: &[u8]) -> bool {
    a.iter().all(|a| a.len() == b.len())
}

/// The reduction GCC gives most float sums (`inner_product`, `l1_distance`, the sparse
/// norms): (l0 + l2) + (l1 + l3), then the remaining terms one at a time.
#[inline(always)]
fn seq_reduce(l: [f32; 4], a: &[u8], b: &[u8], n: usize, term: impl Fn(f32, f32) -> f32) -> f32 {
    let mut s = (l[0] + l[2]) + (l[1] + l[3]);
    for i in n & !3..n {
        s += term(f32_at(a, i), f32_at(b, i));
    }
    s
}

#[inline(always)]
fn sum_seq(a: &[u8], b: &[u8], n: usize, term: impl Fn(f32, f32) -> f32) -> f32 {
    seq_reduce(lanes(a, b, n, &term), a, b, n, term)
}

/// `VectorL2SquaredDistance`'s reduction of its lanes. Its remainder keeps two lanes: one
/// term is added to the full sum, two or three go to the half sums (l0 + l2) and (l1 + l3)
/// before they are added.
#[inline(always)]
fn l2_reduce(l: [f32; 4], a: &[u8], b: &[u8], n: usize) -> f32 {
    let (s0, s1) = (l[0] + l[2], l[1] + l[3]);
    let main = n & !3;
    let t = |i: usize| l2_term(f32_at(a, i), f32_at(b, i));
    match n - main {
        0 => s0 + s1,
        1 => (s0 + s1) + t(main),
        r => {
            let s = (s0 + t(main)) + (s1 + t(main + 1));
            if r == 3 { s + t(main + 2) } else { s }
        }
    }
}

#[inline(always)]
fn l2_term(x: f32, y: f32) -> f32 {
    (x - y) * (x - y)
}

/// `VectorL2SquaredDistance`.
pub(super) fn l2_squared(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len()) / 4;
    l2_reduce(lanes(a, b, n, l2_term), a, b, n)
}

/// `l2_squared` of four rows against `b`.
fn l2_squared4(a: &[&[u8]; 4], b: &[u8]) -> [f32; 4] {
    if !same_len(a, b) {
        return a.map(|a| l2_squared(a, b));
    }
    let n = b.len() / 4;
    let l = lanes4(a, b, n, l2_term);
    [0, 1, 2, 3].map(|r| l2_reduce(l[r], a[r], b, n))
}

/// `VectorInnerProduct`.
pub(super) fn inner_product(a: &[u8], b: &[u8]) -> f32 {
    sum_seq(a, b, a.len().min(b.len()) / 4, |x, y| x * y)
}

/// `inner_product` of four rows against `b`.
fn inner_product4(a: &[&[u8]; 4], b: &[u8]) -> [f32; 4] {
    if !same_len(a, b) {
        return a.map(|a| inner_product(a, b));
    }
    let n = b.len() / 4;
    let l = lanes4(a, b, n, |x, y| x * y);
    [0, 1, 2, 3].map(|r| seq_reduce(l[r], a[r], b, n, |x, y| x * y))
}

/// `VectorL1Distance`.
pub(super) fn l1(a: &[u8], b: &[u8]) -> f32 {
    sum_seq(a, b, a.len().min(b.len()) / 4, |x, y| (x - y).abs())
}

/// `VectorCosineSimilarity`'s three sums (a·b, a·a, b·b) in their lanes.
#[inline(always)]
fn cos_lanes(x: [f32; 4], y: [f32; 4], acc: &mut [[f32; 4]; 3]) {
    for j in 0..4 {
        acc[0][j] += x[j] * y[j];
        acc[1][j] += x[j] * x[j];
        acc[2][j] += y[j] * y[j];
    }
}

/// `VectorCosineSimilarity`'s reduction of its lanes: as `l2_squared`'s, except that one or
/// two remaining terms go to the full sums.
#[inline(always)]
fn cos_reduce(acc: [[f32; 4]; 3], a: &[u8], b: &[u8], n: usize) -> f64 {
    let main = n & !3;
    let [ab, aa, bb] = acc;
    let half = |l: [f32; 4]| [l[0] + l[2], l[1] + l[3]];
    let (hab, haa, hbb) = (half(ab), half(aa), half(bb));
    let (mut sab, mut saa, mut sbb) = (hab[0] + hab[1], haa[0] + haa[1], hbb[0] + hbb[1]);
    let mut i = main;
    if n - main == 3 {
        let (x0, y0, x1, y1) = (f32_at(a, i), f32_at(b, i), f32_at(a, i + 1), f32_at(b, i + 1));
        sab = (hab[0] + x0 * y0) + (hab[1] + x1 * y1);
        saa = (haa[0] + x0 * x0) + (haa[1] + x1 * x1);
        sbb = (hbb[0] + y0 * y0) + (hbb[1] + y1 * y1);
        i += 2;
    }
    for i in i..n {
        let (x, y) = (f32_at(a, i), f32_at(b, i));
        sab += x * y;
        saa += x * x;
        sbb += y * y;
    }
    // Use sqrt(a * b) over sqrt(a) * sqrt(b)
    sab as f64 / (saa as f64 * sbb as f64).sqrt()
}

/// `VectorCosineSimilarity`.
pub(super) fn cosine_similarity(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len()) / 4;
    let main = 4 * (n & !3);
    let mut acc = [[0.0f32; 4]; 3];
    for (ca, cb) in a[..main].as_chunks::<16>().0.iter().zip(b[..main].as_chunks::<16>().0) {
        cos_lanes(quad(ca), quad(cb), &mut acc);
    }
    cos_reduce(acc, a, b, n)
}

/// `cosine_similarity` of four rows against `b`.
fn cosine_similarity4(a: &[&[u8]; 4], b: &[u8]) -> [f64; 4] {
    if !same_len(a, b) {
        return a.map(|a| cosine_similarity(a, b));
    }
    let n = b.len() / 4;
    let main = 4 * (n & !3);
    let mut acc = [[[0.0f32; 4]; 3]; 4];
    let cb = b[..main].as_chunks::<16>().0;
    let ca = a.map(|a| a[..main].as_chunks::<16>().0);
    for (i, cb) in cb.iter().enumerate() {
        let y = quad(cb);
        for r in 0..4 {
            cos_lanes(quad(&ca[r][i]), y, &mut acc[r]);
        }
    }
    [0, 1, 2, 3].map(|r| cos_reduce(acc[r], a[r], b, n))
}

/// The sum of the squares of floats in double, as `vector_norm` and `l2_normalize` (and
/// the sparse ones) compute it: two lanes, each adding x[j]² + x[j + 2]² per four floats,
/// then (l0 + l1) and the rest one at a time.
pub(super) fn norm_squared(a: &[u8]) -> f64 {
    let n = a.len() / 4;
    let main = n & !3;
    let sq = |x: f32| x as f64 * x as f64;
    let mut acc = [0.0f64; 2];
    for c in a[..4 * main].as_chunks::<16>().0 {
        let x = quad(c);
        acc[0] += sq(x[0]) + sq(x[2]);
        acc[1] += sq(x[1]) + sq(x[3]);
    }
    let mut s = acc[0] + acc[1];
    for i in main..n {
        s += sq(f32_at(a, i));
    }
    s
}

// ── halfvec (halfutils.c) ──

/// Whether halfvec's kernels are the F16C variants: `HalfvecInit` takes them when CPUID
/// shows AVX, F16C and FMA and the OS saves the AVX state.
fn f16c() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx")
            && std::arch::is_x86_feature_detected!("f16c")
            && std::arch::is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn h(b: &[u8], i: usize) -> f32 {
    half_to_f32(u16_at(b, i))
}

/// `HalfvecL2SquaredDistance`.
pub(super) fn half_l2_squared(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len()) / 2;
    #[cfg(target_arch = "x86_64")]
    if f16c() {
        // SAFETY: the CPU has the features.
        return unsafe { x86::l2_squared(a, b, n) };
    }
    // HalfvecL2SquaredDistanceDefault: one at a time.
    (0..n).fold(0.0, |s, i| {
        let d = h(a, i) - h(b, i);
        s + d * d
    })
}

/// `HalfvecInnerProduct`.
pub(super) fn half_inner_product(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len()) / 2;
    #[cfg(target_arch = "x86_64")]
    if f16c() {
        // SAFETY: the CPU has the features.
        return unsafe { x86::inner_product(a, b, n) };
    }
    (0..n).fold(0.0, |s, i| s + h(a, i) * h(b, i))
}

/// `HalfvecL1Distance`.
pub(super) fn half_l1(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len()) / 2;
    #[cfg(target_arch = "x86_64")]
    if f16c() {
        // SAFETY: the CPU has the features.
        return unsafe { x86::l1(a, b, n) };
    }
    (0..n).fold(0.0, |s, i| s + (h(a, i) - h(b, i)).abs())
}

/// `HalfvecCosineSimilarity`.
pub(super) fn half_cosine_similarity(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len()) / 2;
    #[cfg(target_arch = "x86_64")]
    if f16c() {
        // SAFETY: the CPU has the features.
        let (ab, aa, bb) = unsafe { x86::cosine_sums(a, b, n) };
        return ab as f64 / (aa as f64 * bb as f64).sqrt();
    }
    let (mut ab, mut aa, mut bb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        let (x, y) = (h(a, i), h(b, i));
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    ab as f64 / (aa as f64 * bb as f64).sqrt()
}

/// The sum of the squares of halves in double (`halfvec_l2_norm`,
/// `halfvec_l2_normalize`): one at a time.
pub(super) fn half_norm_squared(a: &[u8]) -> f64 {
    (0..a.len() / 2).fold(0.0, |s, i| {
        let x = h(a, i) as f64;
        s + x * x
    })
}

/// The F16C variants: eight lanes converted by `vcvtph2ps`, accumulated with fused
/// multiply-adds (L1: adds), reduced as (v0 + v2) + (v1 + v3) over v[j] = l[j] + l[j + 4];
/// then the remaining halves one at a time, fused in the cosine's sums only.
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::h;
    use std::arch::x86_64::*;

    #[target_feature(enable = "avx,f16c,fma")]
    fn reduce(l: __m256) -> f32 {
        let mut s = [0.0f32; 8];
        // SAFETY: s has room for eight floats.
        unsafe { _mm256_storeu_ps(s.as_mut_ptr(), l) };
        let v = [s[0] + s[4], s[1] + s[5], s[2] + s[6], s[3] + s[7]];
        (v[0] + v[2]) + (v[1] + v[3])
    }

    /// Eight halves at `i` as floats.
    #[target_feature(enable = "avx,f16c,fma")]
    fn load(b: &[u8], i: usize) -> __m256 {
        let c = &b[2 * i..2 * i + 16];
        // SAFETY: c holds 16 bytes.
        _mm256_cvtph_ps(unsafe { _mm_loadu_si128(c.as_ptr() as *const __m128i) })
    }

    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn l2_squared(a: &[u8], b: &[u8], n: usize) -> f32 {
        let count = n & !7;
        let mut l = _mm256_setzero_ps();
        for i in (0..count).step_by(8) {
            let d = _mm256_sub_ps(load(a, i), load(b, i));
            l = _mm256_fmadd_ps(d, d, l);
        }
        let mut s = reduce(l);
        for i in count..n {
            let d = h(a, i) - h(b, i);
            s += d * d;
        }
        s
    }

    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn inner_product(a: &[u8], b: &[u8], n: usize) -> f32 {
        let count = n & !7;
        let mut l = _mm256_setzero_ps();
        for i in (0..count).step_by(8) {
            l = _mm256_fmadd_ps(load(a, i), load(b, i), l);
        }
        let mut s = reduce(l);
        for i in count..n {
            s += h(a, i) * h(b, i);
        }
        s
    }

    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn l1(a: &[u8], b: &[u8], n: usize) -> f32 {
        let count = n & !7;
        let sign = _mm256_set1_ps(-0.0);
        let mut l = _mm256_setzero_ps();
        for i in (0..count).step_by(8) {
            l = _mm256_add_ps(l, _mm256_andnot_ps(sign, _mm256_sub_ps(load(a, i), load(b, i))));
        }
        let mut s = reduce(l);
        for i in count..n {
            s += (h(a, i) - h(b, i)).abs();
        }
        s
    }

    /// The cosine's three sums (a·b, a·a, b·b).
    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn cosine_sums(a: &[u8], b: &[u8], n: usize) -> (f32, f32, f32) {
        let count = n & !7;
        let (mut ab, mut aa, mut bb) = (_mm256_setzero_ps(), _mm256_setzero_ps(), _mm256_setzero_ps());
        for i in (0..count).step_by(8) {
            let (x, y) = (load(a, i), load(b, i));
            ab = _mm256_fmadd_ps(x, y, ab);
            aa = _mm256_fmadd_ps(x, x, aa);
            bb = _mm256_fmadd_ps(y, y, bb);
        }
        let (mut ab, mut aa, mut bb) = (reduce(ab), reduce(aa), reduce(bb));
        for i in count..n {
            let (x, y) = (h(a, i), h(b, i));
            ab = x.mul_add(y, ab);
            aa = x.mul_add(x, aa);
            bb = y.mul_add(y, bb);
        }
        (ab, aa, bb)
    }

    /// `l2_squared` of four rows against `b`, their sums interleaved.
    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn l2_squared4(a: &[&[u8]; 4], b: &[u8], n: usize) -> [f32; 4] {
        let count = n & !7;
        let mut l = [_mm256_setzero_ps(); 4];
        for i in (0..count).step_by(8) {
            let y = load(b, i);
            for r in 0..4 {
                let d = _mm256_sub_ps(load(a[r], i), y);
                l[r] = _mm256_fmadd_ps(d, d, l[r]);
            }
        }
        let mut out = [0.0f32; 4];
        for r in 0..4 {
            let mut s = reduce(l[r]);
            for i in count..n {
                let d = h(a[r], i) - h(b, i);
                s += d * d;
            }
            out[r] = s;
        }
        out
    }

    /// `inner_product` of four rows against `b`, their sums interleaved.
    #[target_feature(enable = "avx,f16c,fma")]
    pub(super) fn inner_product4(a: &[&[u8]; 4], b: &[u8], n: usize) -> [f32; 4] {
        let count = n & !7;
        let mut l = [_mm256_setzero_ps(); 4];
        for i in (0..count).step_by(8) {
            let y = load(b, i);
            for r in 0..4 {
                l[r] = _mm256_fmadd_ps(load(a[r], i), y, l[r]);
            }
        }
        let mut out = [0.0f32; 4];
        for r in 0..4 {
            let mut s = reduce(l[r]);
            for i in count..n {
                s += h(a[r], i) * h(b, i);
            }
            out[r] = s;
        }
        out
    }
}

/// `half_l2_squared` of four rows against `b` (each as long as `b`).
fn half_l2_squared4(a: &[&[u8]; 4], b: &[u8]) -> [f32; 4] {
    #[cfg(target_arch = "x86_64")]
    if f16c() && same_len(a, b) {
        // SAFETY: the CPU has the features.
        return unsafe { x86::l2_squared4(a, b, b.len() / 2) };
    }
    a.map(|a| half_l2_squared(a, b))
}

/// `half_inner_product` of four rows against `b`.
fn half_inner_product4(a: &[&[u8]; 4], b: &[u8]) -> [f32; 4] {
    #[cfg(target_arch = "x86_64")]
    if f16c() && same_len(a, b) {
        // SAFETY: the CPU has the features.
        return unsafe { x86::inner_product4(a, b, b.len() / 2) };
    }
    a.map(|a| half_inner_product(a, b))
}

// ── sparsevec (sparsevec.c) ──

/// `distance` plus `term` over the values `v[from..]` of a sparse vector, as the loop that
/// ends `SparsevecL2SquaredDistance` and `sparsevec_l1_distance` adds them: four lanes over
/// whole groups of four (when there are more than three), reduced as `sum_seq`'s and added
/// to `distance`, then the rest one at a time.
fn rest(mut distance: f32, v: &Sparse, from: usize, term: impl Fn(f32) -> f32) -> f32 {
    let count = v.nnz - from;
    let mut i = from;
    if count > 3 {
        let vals = &v.vals[4 * from..4 * v.nnz];
        let l = lanes(vals, vals, count, |x, _| term(x));
        distance += (l[0] + l[2]) + (l[1] + l[3]);
        i += count & !3;
    }
    for i in i..v.nnz {
        distance += term(v.val(i));
    }
    distance
}

/// `SparsevecL2SquaredDistance` (and with `term` |x| for `sparsevec_l1_distance`): the
/// two index lists merged, `diff(a, b)` where both have an element, `term(x)` where one has.
fn sparse_merge(a: &Sparse, b: &Sparse, diff: impl Fn(f32, f32) -> f32, term: impl Fn(f32) -> f32) -> f32 {
    let mut distance = 0.0f32;
    let mut bpos = 0;
    for i in 0..a.nnz {
        let ai = a.idx(i);
        let mut bi = -1;
        let from = bpos;
        for j in from..b.nnz {
            bi = b.idx(j);
            if ai == bi {
                distance += diff(a.val(i), b.val(j));
            } else if ai > bi {
                distance += term(b.val(j));
            }
            // Update start for next iteration
            if ai >= bi {
                bpos = j + 1;
            }
            // Found or passed it
            if bi >= ai {
                break;
            }
        }
        if ai != bi {
            distance += term(a.val(i));
        }
    }
    rest(distance, b, bpos, term)
}

/// `SparsevecL2SquaredDistance`.
pub(super) fn sparse_l2_squared(a: &Sparse, b: &Sparse) -> f32 {
    sparse_merge(a, b, |x, y| (x - y) * (x - y), |x| x * x)
}

/// The sum in `sparsevec_l1_distance`.
pub(super) fn sparse_l1(a: &Sparse, b: &Sparse) -> f32 {
    sparse_merge(a, b, |x, y| (x - y).abs(), |x| x.abs())
}

/// `SparsevecInnerProduct`: one product at a time.
pub(super) fn sparse_inner_product(a: &Sparse, b: &Sparse) -> f32 {
    let mut distance = 0.0f32;
    let mut bpos = 0;
    for i in 0..a.nnz {
        let ai = a.idx(i);
        let from = bpos;
        for j in from..b.nnz {
            let bi = b.idx(j);
            if ai == bi {
                distance += a.val(i) * b.val(j);
            }
            if ai >= bi {
                bpos = j + 1;
            }
            if bi >= ai {
                break;
            }
        }
    }
    distance
}

/// The float sum of the squares of a sparse vector's values (`sparsevec_cosine_distance`).
fn sparse_norm_f32(a: &Sparse) -> f32 {
    sum_seq(a.vals, a.vals, a.nnz, |x, _| x * x)
}

// ── bit (bitutils.c) ──

/// A bit string's bytes (as many as its length takes) and the mask of its last byte's bits.
fn bit_view(v: &Value) -> (&[u8], u8) {
    let Value::Bits(n, b) = v else { return (&[], 0xff) };
    let mask = if n % 8 != 0 { 0xffu8 << (8 - n % 8) } else { 0xff };
    (&b[..(*n as usize).div_ceil(8).min(b.len())], mask)
}

/// Two bit strings' bytes, their pad bits cleared and zipped to the shorter, without
/// copying: the bytes before the last pair, and the last pair.
fn bit_pair<'a>(a: &'a Value, b: &'a Value) -> (&'a [u8], &'a [u8], (u8, u8)) {
    let ((a, ma), (b, mb)) = (bit_view(a), bit_view(b));
    let m = a.len().min(b.len());
    if m == 0 {
        return (&[], &[], (0, 0));
    }
    let last = |v: &[u8], mask: u8| v[m - 1] & if v.len() == m { mask } else { 0xff };
    (&a[..m - 1], &b[..m - 1], (last(a, ma), last(b, mb)))
}

/// Σ popcount(f(x, y)) over the 8-byte words of two byte strings (zipped to the shorter),
/// then their remaining bytes.
#[inline(always)]
fn popcount2(a: &[u8], b: &[u8], f: impl Fn(u64, u64) -> u64) -> u64 {
    let m = a.len().min(b.len());
    let ((ca, ra), (cb, rb)) = (a[..m].as_chunks::<8>(), b[..m].as_chunks::<8>());
    let mut n = 0u64;
    for (x, y) in ca.iter().zip(cb) {
        n += f(u64::from_ne_bytes(*x), u64::from_ne_bytes(*y)).count_ones() as u64;
    }
    for (x, y) in ra.iter().zip(rb) {
        n += f(*x as u64, *y as u64).count_ones() as u64;
    }
    n
}

#[inline(always)]
fn xor_count(a: &[u8], b: &[u8]) -> u64 {
    popcount2(a, b, |x, y| x ^ y)
}

#[inline(always)]
fn jaccard_counts(a: &[u8], b: &[u8]) -> [u64; 3] {
    let m = a.len().min(b.len());
    let (a, b) = (&a[..m], &b[..m]);
    [popcount2(a, b, |x, y| x & y), popcount2(a, a, |x, _| x), popcount2(b, b, |y, _| y)]
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
fn xor_count_popcnt(a: &[u8], b: &[u8]) -> u64 {
    xor_count(a, b)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
fn jaccard_counts_popcnt(a: &[u8], b: &[u8]) -> [u64; 3] {
    jaccard_counts(a, b)
}

/// Whether the CPU counts bits in one instruction (pgvector's bit kernels take it when it
/// does, as these do; the counts are the same either way).
fn popcnt() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("popcnt")
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// The bits that differ between two byte strings (zipped to the shorter).
pub(crate) fn bits_differing(a: &[u8], b: &[u8]) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if popcnt() {
        // SAFETY: the CPU has POPCNT.
        return unsafe { xor_count_popcnt(a, b) };
    }
    xor_count(a, b)
}

/// The bits set in both of two byte strings (zipped to the shorter), in the first, in the
/// second.
pub(crate) fn bits_shared(a: &[u8], b: &[u8]) -> [u64; 3] {
    #[cfg(target_arch = "x86_64")]
    if popcnt() {
        // SAFETY: the CPU has POPCNT.
        return unsafe { jaccard_counts_popcnt(a, b) };
    }
    jaccard_counts(a, b)
}

/// `BitHammingDistance`.
pub(super) fn hamming(a: &Value, b: &Value) -> f64 {
    let (a, b, (x, y)) = bit_pair(a, b);
    (bits_differing(a, b) + (x ^ y).count_ones() as u64) as f64
}

/// `BitJaccardDistance`.
pub(super) fn jaccard(a: &Value, b: &Value) -> f64 {
    let (a, b, (x, y)) = bit_pair(a, b);
    let [ab, aa, bb] = bits_shared(a, b);
    let (ab, aa, bb) = (ab + (x & y).count_ones() as u64, aa + x.count_ones() as u64, bb + y.count_ones() as u64);
    if ab == 0 { 1.0 } else { 1.0 - (ab as f64 / (aa + bb - ab) as f64) }
}

// ── what the SQL functions return ──

/// A similarity kept in [-1, 1] (NaN as it is).
fn clamp(s: f64) -> f64 {
    s.clamp(-1.0, 1.0)
}

/// `acos(distance) / M_PI` of an inner product kept in range (the spherical distances).
fn spherical(ip: f32) -> f64 {
    acos(clamp(ip as f64)) / std::f64::consts::PI
}

pub(super) fn vector_l2_squared(a: &[u8], b: &[u8]) -> f64 {
    l2_squared(a, b) as f64
}
pub(super) fn vector_l2(a: &[u8], b: &[u8]) -> f64 {
    (l2_squared(a, b) as f64).sqrt()
}
pub(super) fn vector_ip(a: &[u8], b: &[u8]) -> f64 {
    inner_product(a, b) as f64
}
pub(super) fn vector_negative_ip(a: &[u8], b: &[u8]) -> f64 {
    -inner_product(a, b) as f64
}
pub(super) fn vector_cosine(a: &[u8], b: &[u8]) -> f64 {
    1.0 - clamp(cosine_similarity(a, b))
}
pub(super) fn vector_spherical(a: &[u8], b: &[u8]) -> f64 {
    spherical(inner_product(a, b))
}
pub(super) fn vector_l1(a: &[u8], b: &[u8]) -> f64 {
    l1(a, b) as f64
}
pub(super) fn vector_norm(a: &[u8]) -> f64 {
    norm_squared(a).sqrt()
}

pub(super) fn halfvec_l2_squared(a: &[u8], b: &[u8]) -> f64 {
    half_l2_squared(a, b) as f64
}
pub(super) fn halfvec_l2(a: &[u8], b: &[u8]) -> f64 {
    (half_l2_squared(a, b) as f64).sqrt()
}
pub(super) fn halfvec_ip(a: &[u8], b: &[u8]) -> f64 {
    half_inner_product(a, b) as f64
}
pub(super) fn halfvec_negative_ip(a: &[u8], b: &[u8]) -> f64 {
    -half_inner_product(a, b) as f64
}
pub(super) fn halfvec_cosine(a: &[u8], b: &[u8]) -> f64 {
    1.0 - clamp(half_cosine_similarity(a, b))
}
pub(super) fn halfvec_spherical(a: &[u8], b: &[u8]) -> f64 {
    spherical(half_inner_product(a, b))
}
pub(super) fn halfvec_l1(a: &[u8], b: &[u8]) -> f64 {
    half_l1(a, b) as f64
}
pub(super) fn halfvec_norm(a: &[u8]) -> f64 {
    half_norm_squared(a).sqrt()
}

pub(super) fn sparsevec_l2_squared(a: &[u8], b: &[u8]) -> f64 {
    sparse_l2_squared(&Sparse::new(a), &Sparse::new(b)) as f64
}
pub(super) fn sparsevec_l2(a: &[u8], b: &[u8]) -> f64 {
    (sparse_l2_squared(&Sparse::new(a), &Sparse::new(b)) as f64).sqrt()
}
pub(super) fn sparsevec_ip(a: &[u8], b: &[u8]) -> f64 {
    sparse_inner_product(&Sparse::new(a), &Sparse::new(b)) as f64
}
pub(super) fn sparsevec_negative_ip(a: &[u8], b: &[u8]) -> f64 {
    -sparse_inner_product(&Sparse::new(a), &Sparse::new(b)) as f64
}
/// `sparsevec_cosine_distance`.
pub(super) fn sparsevec_cosine(a: &[u8], b: &[u8]) -> f64 {
    let (a, b) = (Sparse::new(a), Sparse::new(b));
    let similarity = sparse_inner_product(&a, &b) as f64;
    let (na, nb) = (sparse_norm_f32(&a), sparse_norm_f32(&b));
    1.0 - clamp(similarity / (na as f64 * nb as f64).sqrt())
}
pub(super) fn sparsevec_l1(a: &[u8], b: &[u8]) -> f64 {
    sparse_l1(&Sparse::new(a), &Sparse::new(b)) as f64
}
pub(super) fn sparsevec_norm(a: &[u8]) -> f64 {
    norm_squared(Sparse::new(a).vals).sqrt()
}

// ── for the index access methods ──

/// The byte-level kernels of the SQL distance functions over vector, halfvec and sparsevec,
/// by C name, handed to macro `$m`.
macro_rules! byte_kernels {
    ($m:ident) => {
        $m! {
            "vector_l2_squared_distance" => vector_l2_squared,
            "l2_distance" => vector_l2,
            "vector_negative_inner_product" => vector_negative_ip,
            "inner_product" => vector_ip,
            "cosine_distance" => vector_cosine,
            "vector_spherical_distance" => vector_spherical,
            "l1_distance" => vector_l1,
            "halfvec_l2_squared_distance" => halfvec_l2_squared,
            "halfvec_l2_distance" => halfvec_l2,
            "halfvec_negative_inner_product" => halfvec_negative_ip,
            "halfvec_inner_product" => halfvec_ip,
            "halfvec_cosine_distance" => halfvec_cosine,
            "halfvec_spherical_distance" => halfvec_spherical,
            "halfvec_l1_distance" => halfvec_l1,
            "sparsevec_l2_squared_distance" => sparsevec_l2_squared,
            "sparsevec_l2_distance" => sparsevec_l2,
            "sparsevec_negative_inner_product" => sparsevec_negative_ip,
            "sparsevec_inner_product" => sparsevec_ip,
            "sparsevec_cosine_distance" => sparsevec_cosine,
            "sparsevec_l1_distance" => sparsevec_l1,
        }
    };
}

/// The float8 the SQL function `symbol` (its C name) returns for two values of its type,
/// bit for bit (the dimensions are the caller's to check).
pub(crate) fn distance(symbol: &str) -> Option<fn(&Value, &Value) -> f64> {
    macro_rules! by_symbol {
        ($($name:literal => $f:path),* $(,)?) => {
            match symbol {
                $($name => {
                    fn w(a: &Value, b: &Value) -> f64 {
                        $f(bytes(a), bytes(b))
                    }
                    w
                })*
                "hamming_distance" => hamming,
                "jaccard_distance" => jaccard,
                _ => return None,
            }
        };
    }
    Some(byte_kernels!(by_symbol))
}

/// `distance` of a vector, halfvec or sparsevec function over the values' bytes (the
/// dimensions are the caller's to check).
pub(crate) fn distance_bytes(symbol: &str) -> Option<fn(&[u8], &[u8]) -> f64> {
    macro_rules! by_symbol {
        ($($name:literal => $f:path),* $(,)?) => {
            match symbol {
                $($name => $f as fn(&[u8], &[u8]) -> f64,)*
                _ => return None,
            }
        };
    }
    Some(byte_kernels!(by_symbol))
}

/// A distance of four rows to one value at once: `[d(rows[0], v), ...]` bit for bit
/// (`distance4`).
pub(crate) type Dist4 = fn(&[&[u8]; 4], &[u8]) -> [f64; 4];

/// The four-row form of the vector distance function `symbol` (its C name), for the
/// kernels whose sums are most of the work: four rows' sums interleaved, each row's
/// computed as the one-row kernel computes it. (The distances are symmetric bit for bit,
/// so the value may be either argument of the SQL function.)
pub(crate) fn distance4(symbol: &str) -> Option<Dist4> {
    Some(match symbol {
        "vector_l2_squared_distance" => |a, b| l2_squared4(a, b).map(|d| d as f64),
        "l2_distance" => |a, b| l2_squared4(a, b).map(|d| (d as f64).sqrt()),
        "inner_product" => |a, b| inner_product4(a, b).map(|d| d as f64),
        "vector_negative_inner_product" => |a, b| inner_product4(a, b).map(|d| -d as f64),
        "cosine_distance" => |a, b| cosine_similarity4(a, b).map(|s| 1.0 - clamp(s)),
        "halfvec_l2_squared_distance" => |a, b| half_l2_squared4(a, b).map(|d| d as f64),
        "halfvec_l2_distance" => |a, b| half_l2_squared4(a, b).map(|d| (d as f64).sqrt()),
        "halfvec_inner_product" => |a, b| half_inner_product4(a, b).map(|d| d as f64),
        "halfvec_negative_inner_product" => |a, b| half_inner_product4(a, b).map(|d| -d as f64),
        _ => return None,
    })
}

/// Ask for the first cache lines of `b` ahead of their use: a hint, which the hardware's
/// own prefetching follows through the rest (nothing on other targets).
#[inline(always)]
pub(crate) fn prefetch(b: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    for off in (0..b.len().min(256)).step_by(64) {
        // SAFETY: the address is inside `b`; a prefetch reads nothing the program sees.
        unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(b.as_ptr().add(off).cast()) }
    }
}

/// The float8 the norm function `symbol` returns (`vector_norm`, `halfvec_l2_norm`,
/// `sparsevec_l2_norm`).
pub(crate) fn norm(symbol: &str) -> Option<fn(&Value) -> f64> {
    fn w<const K: u8>(v: &Value) -> f64 {
        match K {
            0 => vector_norm(bytes(v)),
            1 => halfvec_norm(bytes(v)),
            _ => sparsevec_norm(bytes(v)),
        }
    }
    Some(match symbol {
        "vector_norm" => w::<0>,
        "halfvec_l2_norm" => w::<1>,
        "sparsevec_l2_norm" => w::<2>,
        _ => return None,
    })
}

/// A vector, halfvec or sparsevec normalized as the SQL `l2_normalize` does it (another
/// value as it is).
pub(crate) fn l2_normalize(v: &Value) -> Value {
    use crate::ext::types::{HALFVEC, SPARSEVEC, VECTOR};
    match v {
        // (A quotient by the norm can't overflow: the norm is at least each element.)
        Value::Ext(VECTOR, b) => super::vector::normalize(b).unwrap_or_else(|_| v.clone()),
        Value::Ext(HALFVEC, b) => super::halfvec::normalize(b).unwrap_or_else(|_| v.clone()),
        Value::Ext(SPARSEVEC, b) => super::sparsevec::normalize(b).unwrap_or_else(|_| v.clone()),
        _ => v.clone(),
    }
}

// ── musl's acos ──

/// musl's `acos` (fdlibm's), the reference's libm, so that the spherical distances are
/// its bits (glibc's differs in the last place).
pub(super) fn acos(x: f64) -> f64 {
    let c = f64::from_bits;
    let pio2_hi = c(0x3FF921FB54442D18);
    let pio2_lo = c(0x3C91A62633145C07);
    // 0x1p-120f
    let tiny = c(903 << 52);
    let r = |z: f64| {
        let p = z
            * (c(0x3FC5555555555555)
                + z * (c(0xBFD4D61203EB6F7D)
                    + z * (c(0x3FC9C1550E884455)
                        + z * (c(0xBFA48228B5688F3B) + z * (c(0x3F49EFE07501B288) + z * c(0x3F023DE10DFDF709))))));
        let q = 1.0
            + z * (c(0xC0033A271C8A2D4B)
                + z * (c(0x40002AE59C598AC8) + z * (c(0xBFE6066C1B8D0159) + z * c(0x3FB3B8C5B12E9282))));
        p / q
    };
    let hx = (x.to_bits() >> 32) as u32;
    let ix = hx & 0x7fff_ffff;
    // |x| >= 1 or nan
    if ix >= 0x3ff0_0000 {
        let lx = x.to_bits() as u32;
        if (ix - 0x3ff0_0000) | lx == 0 {
            // acos(1)=0, acos(-1)=pi
            return if hx >> 31 != 0 { 2.0 * pio2_hi + tiny } else { 0.0 };
        }
        return f64::NAN;
    }
    // |x| < 0.5
    if ix < 0x3fe0_0000 {
        if ix <= 0x3c60_0000 {
            return pio2_hi + tiny;
        }
        return pio2_hi - (x - (pio2_lo - x * r(x * x)));
    }
    // x < -0.5
    if hx >> 31 != 0 {
        let z = (1.0 + x) * 0.5;
        let s = z.sqrt();
        let w = r(z) * s - pio2_lo;
        return 2.0 * (pio2_hi - (s + w));
    }
    // x > 0.5
    let z = (1.0 - x) * 0.5;
    let s = z.sqrt();
    let df = c(s.to_bits() & 0xffff_ffff_0000_0000);
    let cc = (z - df * df) / (s + df);
    let w = r(z) * s + cc;
    2.0 * (df + w)
}


