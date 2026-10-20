//! The vector arithmetic of `hnsw.c` and `vset.c`, operation for operation, so distances,
//! norms and projections round exactly as in Redis' build (GCC, `-std=gnu11 -O3`, whose default
//! `-ffp-contract=fast` fuses a multiplication into the addition using it wherever the target has
//! the instruction: always on arm64, on x86-64 only inside the AVX2/AVX-512 functions, which
//! `hnsw.c` picks at run time by the CPU's features).

use super::hnsw::{Data, Vect};

/// Whether products feeding an addition are fused in the plain C (built for the baseline CPU).
const FMA: bool = cfg!(target_arch = "aarch64");

/// `a*b + c`, as the compiled C computes it.
#[inline]
pub fn mul_add(a: f32, b: f32, c: f32) -> f32 {
    if FMA { a.mul_add(b, c) } else { a * b + c }
}

/// `a*b - c`, as the compiled C computes it.
#[inline]
pub fn mul_sub(a: f32, b: f32, c: f32) -> f32 {
    if FMA { a.mul_add(b, -c) } else { a * b - c }
}

/// `x0*y0 + x1*y1 + x2*y2 + x3*y3` (left to right; GCC fuses the first product into the first
/// addition, then each next product into the running sum).
#[inline]
fn sum4(x: &[f32], y: &[f32]) -> f32 {
    if FMA {
        mul_add(x[3], y[3], mul_add(x[2], y[2], mul_add(x[0], y[0], x[1] * y[1])))
    } else {
        x[0] * y[0] + x[1] * y[1] + x[2] * y[2] + x[3] * y[3]
    }
}

/// `vectors_distance_float`: `1 - dot` of normalized vectors, with two accumulators over
/// blocks of eight.
pub fn dot_distance(x: &[f32], y: &[f32]) -> f32 {
    let dim = x.len().min(y.len());
    #[cfg(target_arch = "x86_64")]
    {
        if dim >= 16 && simd::avx512() {
            return simd::dot_avx512(&x[..dim], &y[..dim]);
        }
        if dim >= 16 && simd::avx2() {
            return simd::dot_avx2(&x[..dim], &y[..dim]);
        }
    }
    let (mut d0, mut d1) = (0f32, 0f32);
    let mut i = 0;
    while i + 7 < dim {
        d0 += sum4(&x[i..i + 4], &y[i..i + 4]);
        d1 += sum4(&x[i + 4..i + 8], &y[i + 4..i + 8]);
        i += 8;
    }
    while i < dim {
        d0 = mul_add(x[i], y[i], d0);
        i += 1;
    }
    1.0 - (d0 + d1)
}

/// `vectors_distance_q8`: the integer dot product scaled back by both ranges, clamped to [0, 2].
pub fn q8_distance(x: &[i8], y: &[i8], ra: f32, rb: f32) -> f32 {
    if ra == 0.0 || rb == 0.0 {
        return 1.0;
    }
    let scale = (ra / 127.0) * (rb / 127.0);
    let dim = x.len().min(y.len());
    // The AVX2/AVX-512 versions only regroup the integer sum (exact, wrapping), and fuse the
    // scaling into `1 - dot*scale`.
    #[cfg(target_arch = "x86_64")]
    let fused = (dim >= 64 && simd::avx512()) || (dim >= 32 && simd::avx2());
    #[cfg(not(target_arch = "x86_64"))]
    let fused = FMA;
    let (mut d0, mut d1) = (0i32, 0i32);
    let mut i = 0;
    let p = |k: usize| x[k] as i32 * y[k] as i32;
    while i + 7 < dim {
        d0 = d0.wrapping_add(p(i) + p(i + 1) + p(i + 2) + p(i + 3));
        d1 = d1.wrapping_add(p(i + 4) + p(i + 5) + p(i + 6) + p(i + 7));
        i += 8;
    }
    while i < dim {
        d0 = d0.wrapping_add(p(i));
        i += 1;
    }
    let dot = d0.wrapping_add(d1) as f32;
    let d = if fused { (-dot).mul_add(scale, 1.0) } else { 1.0 - dot * scale };
    d.clamp(0.0, 2.0)
}

/// `vectors_distance_bin`: the share of differing signs, times two.
pub fn bin_distance(x: &[u64], y: &[u64], dim: u32) -> f32 {
    let opposite: u32 = x.iter().zip(y).map(|(a, b)| (a ^ b).count_ones()).sum();
    (opposite as f32 * 2.0) / dim as f32
}

/// `hnsw_distance` for two vectors of one set.
#[inline]
pub fn distance(a: &Vect, b: &Vect, dim: u32) -> f32 {
    match (&a.data, &b.data) {
        (Data::F32(x), Data::F32(y)) => dot_distance(x, y),
        (Data::Q8(x), Data::Q8(y)) => q8_distance(x, y, a.range, b.range),
        (Data::Bin(x), Data::Bin(y)) => bin_distance(x, y, dim),
        _ => unreachable!("one quantization per set"),
    }
}

/// `hnsw_normalize_vector`: divide by the L2 norm and return it; an all-zero vector is left
/// as it is (`None`).
pub fn normalize(x: &mut [f32]) -> Option<f32> {
    let dim = x.len();
    let mut l2 = 0f32;
    let mut i = 0;
    while i + 3 < dim {
        l2 += sum4(&x[i..i + 4], &x[i..i + 4]);
        i += 4;
    }
    while i < dim {
        l2 = mul_add(x[i], x[i], l2);
        i += 1;
    }
    if l2 == 0.0 {
        return None;
    }
    let l2 = l2.sqrt();
    for v in x.iter_mut() {
        *v /= l2;
    }
    Some(l2)
}

/// `quantize_to_q8`: to [-127, 127] by the max abs value, which is returned as the range.
pub fn quantize_q8(src: &[f32]) -> (Vec<i8>, f32) {
    let mut max_abs = 0f32;
    for &v in src {
        if v > max_abs {
            max_abs = v;
        }
        if -v > max_abs {
            max_abs = -v;
        }
    }
    if max_abs == 0.0 {
        return (vec![0; src.len()], 0.0);
    }
    let scale = 127.0f32 / max_abs;
    (src.iter().map(|&v| (v * scale).round() as i8).collect(), max_abs)
}

/// `quantize_to_bin`: a set bit per positive component.
pub fn quantize_bin(src: &[f32]) -> Vec<u64> {
    let mut out = vec![0u64; src.len().div_ceil(64)];
    for (j, &v) in src.iter().enumerate() {
        if v > 0.0 {
            out[j / 64] |= 1u64 << (j & 63);
        }
    }
    out
}

/// `createProjectionMatrix`: `output × input` Hadamard signs scaled by `1/sqrt(input)`.
pub fn projection_matrix(input: u32, output: u32) -> Vec<f32> {
    let scale = (1.0 / (input as f64).sqrt()) as f32;
    let mut m = Vec::with_capacity(input as usize * output as usize);
    for i in 0..output {
        for j in 0..input {
            let v: f32 = if (i & j).count_ones() % 2 == 0 { 1.0 } else { -1.0 };
            m.push(v * scale);
        }
    }
    m
}

/// `applyProjection`.
pub fn project(input: &[f32], m: &[f32], output: u32) -> Vec<f32> {
    let n = input.len();
    (0..output as usize)
        .map(|i| {
            let row = &m[i * n..(i + 1) * n];
            let mut sum = 0f32;
            for j in 0..n {
                sum = mul_add(row[j], input[j], sum);
            }
            sum
        })
        .collect()
}

/// `hnsw.c`'s x86-64 kernels: what `__builtin_cpu_supports` picks, and the float dot products
/// lane for lane (each lane a fused multiply-add, then the intrinsics' reduction order).
#[cfg(target_arch = "x86_64")]
mod simd {
    /// `VSET_USE_AVX512`.
    pub fn avx512() -> bool {
        std::arch::is_x86_feature_detected!("avx512f") && std::arch::is_x86_feature_detected!("avx512bw")
    }

    /// `VSET_USE_AVX2`.
    pub fn avx2() -> bool {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }

    /// `vectors_distance_float_avx512`: one 16-lane accumulator, `_mm512_reduce_add_ps` (halves,
    /// then quarters, then pairs), the rest fused into the sum.
    pub fn dot_avx512(x: &[f32], y: &[f32]) -> f32 {
        let dim = x.len();
        let mut s = [0f32; 16];
        let mut i = 0;
        while i + 15 < dim {
            for k in 0..16 {
                s[k] = x[i + k].mul_add(y[i + k], s[k]);
            }
            i += 16;
        }
        let t3: [f32; 8] = std::array::from_fn(|k| s[k + 8] + s[k]);
        let t6: [f32; 4] = std::array::from_fn(|k| t3[k + 4] + t3[k]);
        let mut dot = (t6[0] + t6[2]) + (t6[1] + t6[3]);
        while i < dim {
            dot = x[i].mul_add(y[i], dot);
            i += 1;
        }
        1.0 - dot
    }

    /// `vectors_distance_float_avx2`: two 8-lane accumulators added, the high half added to the
    /// low, two horizontal adds, the rest fused into the sum.
    pub fn dot_avx2(x: &[f32], y: &[f32]) -> f32 {
        let dim = x.len();
        let (mut s1, mut s2) = ([0f32; 8], [0f32; 8]);
        let mut i = 0;
        while i + 15 < dim {
            for k in 0..8 {
                s1[k] = x[i + k].mul_add(y[i + k], s1[k]);
                s2[k] = x[i + 8 + k].mul_add(y[i + 8 + k], s2[k]);
            }
            i += 16;
        }
        let c: [f32; 8] = std::array::from_fn(|k| s1[k] + s2[k]);
        let q: [f32; 4] = std::array::from_fn(|k| c[k + 4] + c[k]);
        let mut dot = (q[0] + q[1]) + (q[2] + q[3]);
        while i < dim {
            dot = x[i].mul_add(y[i], dot);
            i += 1;
        }
        1.0 - dot
    }
}
