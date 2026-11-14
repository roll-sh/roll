//! Float vector kernels of the vector score functions: Lucene's `VectorUtil.dotProduct` and
//! `squareDistance` as Elasticsearch runs them (the Panama implementation over 256-bit vectors).
//!
//! Short vectors (fewer than 32 elements) are summed sequentially with separate multiply and
//! add, as the JIT runs Lucene's scalar loop. Longer ones follow Lucene's vector body: eight
//! lanes, four accumulators fed by fused multiply-adds in strides of 32 elements, a vector tail
//! into the first accumulator, the accumulators added pairwise, the lanes reduced as a halving
//! tree, then a scalar tail. Every operation is an IEEE-754 single-precision one, so the AVX2
//! path and the portable path (which performs the same lane operations one by one) give the
//! same bits, and both agree with Elasticsearch on AVX2 hardware in the common cases (the JIT's
//! choices are not fully reproducible: an occasional last-ulp difference remains).

const LANES: usize = 8;
/// Below this length the scalar loop runs (`length > 2 * species length` vectorizes; observed
/// to be 32 for the 256-bit species).
const VECTOR_MIN: usize = 32;

/// `VectorUtil.dotProduct(float[], float[])` over the common prefix.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    if n < VECTOR_MIN {
        return scalar_tail(a, b, 0, 0.0, |x, y, r| x * y + r);
    }
    #[cfg(target_arch = "x86_64")]
    {
        if avx2_fma() {
            // SAFETY: the CPU supports AVX2 and FMA (checked above)
            return unsafe { x86::dot(a, b) };
        }
    }
    portable_body(a, b, |x, y, acc| x.mul_add(y, acc), |x, y, r| x * y + r)
}

/// `VectorUtil.squareDistance(float[], float[])` over the common prefix.
pub fn square_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let tail = |x: f32, y: f32, r: f32| {
        let d = x - y;
        d * d + r
    };
    if n < VECTOR_MIN {
        return scalar_tail(a, b, 0, 0.0, tail);
    }
    #[cfg(target_arch = "x86_64")]
    {
        if avx2_fma() {
            // SAFETY: the CPU supports AVX2 and FMA (checked above)
            return unsafe { x86::square_distance(a, b) };
        }
    }
    portable_body(
        a,
        b,
        |x, y, acc| {
            let d = x - y;
            d.mul_add(d, acc)
        },
        tail,
    )
}

#[cfg(target_arch = "x86_64")]
fn avx2_fma() -> bool {
    use std::sync::atomic::{AtomicU8, Ordering};
    static CACHE: AtomicU8 = AtomicU8::new(0);
    match CACHE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let yes = std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma");
            CACHE.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
            yes
        }
    }
}

fn scalar_tail(a: &[f32], b: &[f32], from: usize, mut r: f32, f: impl Fn(f32, f32, f32) -> f32) -> f32 {
    for i in from..a.len() {
        r = f(a[i], b[i], r);
    }
    r
}

/// Lanes reduced as a halving tree: `(l0+l4)+(l2+l6)` and `(l1+l5)+(l3+l7)`, then their sum.
fn reduce(v: [f32; LANES]) -> f32 {
    let h: [f32; 4] = [v[0] + v[4], v[1] + v[5], v[2] + v[6], v[3] + v[7]];
    let q: [f32; 2] = [h[0] + h[2], h[1] + h[3]];
    q[0] + q[1]
}

/// The vector body lane by lane (the reference the SIMD path must equal).
fn portable_body(a: &[f32], b: &[f32], step: impl Fn(f32, f32, f32) -> f32, tail: impl Fn(f32, f32, f32) -> f32) -> f32 {
    let n = a.len();
    let limit = n / LANES * LANES;
    let mut acc = [[0f32; LANES]; 4];
    let mut i = 0;
    while i + 4 * LANES <= limit {
        for (k, lane_acc) in acc.iter_mut().enumerate() {
            let off = i + k * LANES;
            for l in 0..LANES {
                lane_acc[l] = step(a[off + l], b[off + l], lane_acc[l]);
            }
        }
        i += 4 * LANES;
    }
    while i < limit {
        for l in 0..LANES {
            acc[0][l] = step(a[i + l], b[i + l], acc[0][l]);
        }
        i += LANES;
    }
    let mut r = [0f32; LANES];
    for l in 0..LANES {
        r[l] = (acc[0][l] + acc[1][l]) + (acc[2][l] + acc[3][l]);
    }
    let res = 0.0 + reduce(r);
    scalar_tail(a, b, limit, res, tail)
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::{reduce, scalar_tail, LANES};

    #[target_feature(enable = "avx2,fma")]
    unsafe fn finish(acc: [__m256; 4]) -> f32 {
        let r = _mm256_add_ps(_mm256_add_ps(acc[0], acc[1]), _mm256_add_ps(acc[2], acc[3]));
        let mut lanes = [0f32; LANES];
        // SAFETY: `lanes` holds eight floats
        unsafe { _mm256_storeu_ps(lanes.as_mut_ptr(), r) };
        0.0 + reduce(lanes)
    }

    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let limit = n / LANES * LANES;
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = [_mm256_setzero_ps(); 4];
        let mut i = 0;
        // SAFETY: every load reads eight floats below `limit <= n` of both slices
        unsafe {
            while i + 4 * LANES <= limit {
                for (k, x) in acc.iter_mut().enumerate() {
                    let off = i + k * LANES;
                    *x = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(off)), _mm256_loadu_ps(pb.add(off)), *x);
                }
                i += 4 * LANES;
            }
            while i < limit {
                acc[0] = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc[0]);
                i += LANES;
            }
            let res = finish(acc);
            scalar_tail(a, b, limit, res, |x, y, r| x * y + r)
        }
    }

    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn square_distance(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let limit = n / LANES * LANES;
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = [_mm256_setzero_ps(); 4];
        let mut i = 0;
        // SAFETY: as in `dot`
        unsafe {
            while i + 4 * LANES <= limit {
                for (k, x) in acc.iter_mut().enumerate() {
                    let off = i + k * LANES;
                    let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(off)), _mm256_loadu_ps(pb.add(off)));
                    *x = _mm256_fmadd_ps(d, d, *x);
                }
                i += 4 * LANES;
            }
            while i < limit {
                let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
                acc[0] = _mm256_fmadd_ps(d, d, acc[0]);
                i += LANES;
            }
            let res = finish(acc);
            scalar_tail(a, b, limit, res, |x, y, r| {
                let d = x - y;
                d * d + r
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift: deterministic inputs without a dependency.
    fn floats(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn simd_and_portable_paths_agree_bit_for_bit() {
        for n in [0, 1, 7, 31, 32, 33, 63, 64, 100, 128, 129, 384, 768, 1000, 1536] {
            let (a, b) = (floats(n as u64 + 1, n), floats(n as u64 * 7 + 3, n));
            let pd = if n < VECTOR_MIN { scalar_tail(&a, &b, 0, 0.0, |x, y, r| x * y + r) } else { portable_body(&a, &b, |x, y, acc| x.mul_add(y, acc), |x, y, r| x * y + r) };
            assert_eq!(dot(&a, &b).to_bits(), pd.to_bits(), "dot n={n}");
            let ps = if n < VECTOR_MIN {
                scalar_tail(&a, &b, 0, 0.0, |x, y, r| (x - y) * (x - y) + r)
            } else {
                portable_body(&a, &b, |x, y, acc| (x - y).mul_add(x - y, acc), |x, y, r| (x - y) * (x - y) + r)
            };
            assert_eq!(square_distance(&a, &b).to_bits(), ps.to_bits(), "l2 n={n}");
        }
    }

    #[test]
    fn matches_elasticsearch_samples() {
        // captured from Elasticsearch 9.5.5 (`dotProduct(params.q, 'v')`) for vectors of the
        // generator above would need the same inputs; these are small-vector identities
        let a = [0.5f32, -1.25, 2.0];
        let b = [4.0f32, 0.5, -0.25];
        assert_eq!(dot(&a, &b), 0.5 * 4.0 + -1.25 * 0.5 + 2.0 * -0.25);
        assert_eq!(square_distance(&a, &a), 0.0);
    }
}
