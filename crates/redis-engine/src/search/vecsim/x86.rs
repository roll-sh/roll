//! VecSim's x86-64 distance functions, lane for lane: the kernel `spaces::GetDistFunc` picks for
//! this CPU and dimension (AVX-512, AVX/AVX2, F16C or SSE), with its accumulators and their
//! residual handling, multiply-adds fused only where the kernel asks for them (its AVX-512 and
//! F16C+FMA builds; the AVX, AVX2 and SSE builds can't), and GCC's horizontal sums. Below each
//! type's minimum dimension, or on a CPU without the features, VecSim runs its scalar loops,
//! which baseline x86-64 compiles without contraction.

use super::super::spec::{Metric, VecType};
use super::{F16_ONE, bf16_to_f32, f16_fma, f16_sub, f32s, f64s, fp16_to_fp32, ftz, u16s};
use std::array::from_fn;
use std::ops::{Add, Mul, Sub};
use std::sync::OnceLock;

/// The CPU features VecSim's dispatch looks at.
struct Cpu {
    sse3: bool,
    avx: bool,
    avx2: bool,
    fma: bool,
    f16c: bool,
    avx512f: bool,
    avx512bw: bool,
    avx512vl: bool,
    avx512vbmi2: bool,
    avx512vnni: bool,
    avx512bf16: bool,
    avx512fp16: bool,
}

fn cpu() -> &'static Cpu {
    static CPU: OnceLock<Cpu> = OnceLock::new();
    CPU.get_or_init(|| Cpu {
        sse3: is_x86_feature_detected!("sse3"),
        avx: is_x86_feature_detected!("avx"),
        avx2: is_x86_feature_detected!("avx2"),
        fma: is_x86_feature_detected!("fma"),
        f16c: is_x86_feature_detected!("f16c"),
        avx512f: is_x86_feature_detected!("avx512f"),
        avx512bw: is_x86_feature_detected!("avx512bw"),
        avx512vl: is_x86_feature_detected!("avx512vl"),
        avx512vbmi2: is_x86_feature_detected!("avx512vbmi2"),
        avx512vnni: is_x86_feature_detected!("avx512vnni"),
        avx512bf16: is_x86_feature_detected!("avx512bf16"),
        avx512fp16: is_x86_feature_detected!("avx512fp16"),
    })
}

/// A field's distance on this CPU (see [`super::distance`]); floating point types only.
pub(super) fn distance(ty: VecType, metric: Metric, dim: usize, a: &[u8], b: &[u8]) -> f64 {
    let l2 = metric == Metric::L2;
    let c = cpu();
    match ty {
        VecType::Float32 => {
            let (x, y) = (f32s(a, dim), f32s(b, dim));
            let s = if dim < 8 {
                scalar(&x, &y, l2)
            } else if c.avx512f {
                reduce512(&lanes::<f32, 16, 2>(&x, &y, l2, true))
            } else if c.avx {
                seq(&lanes::<f32, 8, 4>(&x, &y, l2, false))
            } else {
                fp32_sse(&x, &y, l2)
            };
            (if l2 { s } else { 1.0f32 - s }) as f64
        }
        VecType::Float64 => {
            let (x, y) = (f64s(a, dim), f64s(b, dim));
            let s = if dim < 4 {
                scalar(&x, &y, l2)
            } else if c.avx512f {
                let s = lanes::<f64, 8, 2>(&x, &y, l2, true);
                let t3: [f64; 4] = from_fn(|k| s[k + 4] + s[k]);
                (t3[2] + t3[0]) + (t3[3] + t3[1])
            } else if c.avx {
                seq(&lanes::<f64, 4, 2>(&x, &y, l2, false))
            } else {
                seq(&lanes::<f64, 2, 2>(&x, &y, l2, false))
            };
            if l2 { s } else { 1.0 - s }
        }
        VecType::Float16 => {
            let (x, y) = (u16s(a, dim), u16s(b, dim));
            if dim >= 32 && c.avx512fp16 && c.avx512vl {
                return fp16_avx512fp16(&x, &y, l2) as f64;
            }
            let (x, y): (Vec<f32>, Vec<f32>) = (x.into_iter().map(fp16_to_fp32).collect(), y.into_iter().map(fp16_to_fp32).collect());
            let s = if dim >= 16 && c.avx512f {
                reduce512(&lanes::<f32, 16, 2>(&x, &y, l2, true))
            } else if dim >= 8 && c.f16c && c.fma && c.avx {
                seq(&lanes::<f32, 8, 4>(&x, &y, l2, true))
            } else {
                scalar(&x, &y, l2)
            };
            (if l2 { s } else { 1.0f32 - s }) as f64
        }
        VecType::BFloat16 => {
            let (x, y) = (u16s(a, dim), u16s(b, dim));
            let s = if dim < 32 {
                let (x, y): (Vec<f32>, Vec<f32>) = (x.into_iter().map(bf16_to_f32).collect(), y.into_iter().map(bf16_to_f32).collect());
                scalar(&x, &y, l2)
            } else if !l2 && c.avx512bf16 && c.avx512vl {
                bf16_avx512bf16(&x, &y)
            } else if c.avx512bw && c.avx512vbmi2 {
                bf16_avx512bw(&x, &y, l2)
            } else if c.avx2 {
                bf16_avx2(&x, &y, l2)
            } else if c.sse3 {
                bf16_sse3(&x, &y, l2)
            } else {
                let (x, y): (Vec<f32>, Vec<f32>) = (x.into_iter().map(bf16_to_f32).collect(), y.into_iter().map(bf16_to_f32).collect());
                scalar(&x, &y, l2)
            };
            (if l2 { s } else { 1.0f32 - s }) as f64
        }
        VecType::Int8 | VecType::Uint8 => unreachable!("integer distances are exact"),
    }
}

/// The storage alignment the dispatch reports with the kernel it picks (`*alignment`): a whole
/// vector register's bytes when the dimension leaves no residual, otherwise none.
pub(super) fn alignment(ty: VecType, metric: Metric, dim: usize) -> usize {
    let c = cpu();
    let fits = |n: usize, bytes: usize| if dim.is_multiple_of(n) { bytes } else { 0 };
    match ty {
        VecType::Float32 if dim >= 8 => {
            if c.avx512f {
                fits(16, 64)
            } else if c.avx {
                fits(8, 32)
            } else {
                fits(4, 16)
            }
        }
        VecType::Float64 if dim >= 4 => {
            if c.avx512f {
                fits(8, 64)
            } else if c.avx {
                fits(4, 32)
            } else {
                fits(2, 16)
            }
        }
        VecType::Float16 => {
            if (dim >= 32 && c.avx512fp16 && c.avx512vl) || (dim >= 16 && c.avx512f) {
                fits(32, 64)
            } else if dim >= 8 && c.f16c && c.fma && c.avx {
                fits(16, 32)
            } else {
                0
            }
        }
        VecType::BFloat16 if dim >= 32 => {
            if (metric != Metric::L2 && c.avx512bf16 && c.avx512vl) || (c.avx512bw && c.avx512vbmi2) {
                fits(32, 64)
            } else if c.avx2 {
                fits(16, 32)
            } else if c.sse3 {
                fits(8, 16)
            } else {
                0
            }
        }
        // the VNNI kernels; cosine's stored norm breaks the alignment, so it asks for none
        VecType::Int8 | VecType::Uint8 if dim >= 32 && metric != Metric::Cosine && c.avx512f && c.avx512bw && c.avx512vl && c.avx512vnni => fits(32, 32),
        _ => 0,
    }
}

/// f32 and f64 lanes.
trait Lane: Copy + Default + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> {
    fn fma(self, b: Self, c: Self) -> Self;
}

impl Lane for f32 {
    #[inline]
    fn fma(self, b: f32, c: f32) -> f32 {
        self.mul_add(b, c)
    }
}

impl Lane for f64 {
    #[inline]
    fn fma(self, b: f64, c: f64) -> f64 {
        self.mul_add(b, c)
    }
}

/// The two factors a lane multiplies: the difference twice for L2, the elements for IP.
#[inline]
fn factors<T: Lane>(l2: bool, x: T, y: T) -> (T, T) {
    if l2 {
        let d = x - y;
        (d, d)
    } else {
        (x, y)
    }
}

/// `acc += m * n` over `W` lanes from `p`, fused or a multiply then an add.
#[inline]
fn step<T: Lane, const W: usize>(acc: &mut [T; W], a: &[T], b: &[T], p: usize, l2: bool, fused: bool) {
    for l in 0..W {
        let (m, n) = factors(l2, a[p + l], b[p + l]);
        acc[l] = if fused { m.fma(n, acc[l]) } else { acc[l] + m * n };
    }
}

/// The scalar loop.
fn scalar<T: Lane>(a: &[T], b: &[T], l2: bool) -> T {
    let mut res = T::default();
    for (&x, &y) in a.iter().zip(b) {
        let (m, n) = factors(l2, x, y);
        res = res + m * n;
    }
    res
}

/// The shape of the FP32 AVX-512 and AVX, FP64 AVX-512, AVX and SSE, and FP16 AVX-512F and F16C
/// kernels: `K` accumulators of `W` lanes over chunks of `K * W`; the chunk's partial vector
/// first (a masked load and a multiply into the first accumulator), then its whole vectors into
/// the second, third, ... accumulator, then whole chunks. The accumulators' sum (pairwise for 4).
fn lanes<T: Lane, const W: usize, const K: usize>(a: &[T], b: &[T], l2: bool, fused: bool) -> [T; W] {
    let dim = a.len();
    let residual = dim % (K * W);
    let mut acc = [[T::default(); W]; K];
    let r = residual % W;
    acc[0] = from_fn(|l| {
        let (x, y) = if l < r { (a[l], b[l]) } else { (T::default(), T::default()) };
        let (m, n) = factors(l2, x, y);
        m * n
    });
    let mut p = r;
    for (i, s) in acc.iter_mut().enumerate().skip(1) {
        if residual >= i * W {
            step(s, a, b, p, l2, fused);
            p += W;
        }
    }
    while p < dim {
        for s in acc.iter_mut() {
            step(s, a, b, p, l2, fused);
            p += W;
        }
    }
    if K == 4 { from_fn(|l| (acc[0][l] + acc[1][l]) + (acc[2][l] + acc[3][l])) } else { from_fn(|l| acc[0][l] + acc[1][l]) }
}

/// A stored vector's lanes summed in order (`TmpRes[0] + TmpRes[1] + ...`).
fn seq<T: Lane>(s: &[T]) -> T {
    s[1..].iter().fold(s[0], |t, &x| t + x)
}

/// GCC's `_mm512_reduce_add_ps`: halves, quarters, then the two pairs.
fn reduce512(s: &[f32; 16]) -> f32 {
    let t3: [f32; 8] = from_fn(|k| s[k + 8] + s[k]);
    let t6: [f32; 4] = from_fn(|k| t3[k + 4] + t3[k]);
    (t6[0] + t6[2]) + (t6[1] + t6[3])
}

/// `FP32_L2SqrSIMD16_SSE` / `FP32_InnerProductSIMD16_SSE` (the sum): chunks of 16 in four 4-lane
/// accumulators, the partial vector loaded into particular lanes and the whole ones into the
/// second to fourth accumulator from the back.
fn fp32_sse(a: &[f32], b: &[f32], l2: bool) -> f32 {
    let dim = a.len();
    let residual = dim % 16;
    let mut acc = [[0f32; 4]; 4];
    let r = residual % 4;
    if r > 0 {
        // the lane each of the first `r` elements lands in
        let at: &[usize] = match (r, l2) {
            (3, true) => &[1, 2, 3],
            (3, false) => &[0, 2, 3],
            (2, _) => &[2, 3],
            _ => &[0],
        };
        let (mut x, mut y) = ([0f32; 4], [0f32; 4]);
        for (i, &l) in at.iter().enumerate() {
            x[l] = a[i];
            y[l] = b[i];
        }
        acc[0] = from_fn(|l| {
            let (m, n) = factors(l2, x[l], y[l]);
            m * n
        });
    }
    let mut p = r;
    for (i, min) in [(1, 12), (2, 8), (3, 4)] {
        if residual >= min {
            step(&mut acc[i], a, b, p, l2, false);
            p += 4;
        }
    }
    while p < dim {
        for s in acc.iter_mut() {
            step(s, a, b, p, l2, false);
            p += 4;
        }
    }
    let c: [f32; 4] = from_fn(|l| (acc[0][l] + acc[1][l]) + (acc[2][l] + acc[3][l]));
    seq(&c)
}

fn f16_mul(a: u16, b: u16) -> u16 {
    f16_fma(a, b, 0x8000)
}

/// `FP16_L2SqrSIMD32_AVX512FP16_VL` / `FP16_InnerProductSIMD32_AVX512FP16_VL` (the distance):
/// one 32-lane half precision accumulator, GCC's half precision `_mm512_reduce_add_ph`, and IP's
/// `1 - sum` in half precision too.
fn fp16_avx512fp16(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let dim = a.len();
    let residual = dim % 32;
    let mut sum = [0u16; 32];
    if residual > 0 {
        for (l, s) in sum.iter_mut().enumerate().take(residual) {
            *s = if l2 {
                let d = f16_sub(a[l], b[l]);
                f16_mul(d, d)
            } else {
                f16_mul(a[l], b[l])
            };
        }
    }
    let mut p = residual;
    loop {
        for (l, s) in sum.iter_mut().enumerate() {
            *s = if l2 {
                let d = f16_sub(a[p + l], b[p + l]);
                f16_fma(d, d, *s)
            } else {
                f16_fma(a[p + l], b[p + l], *s)
            };
        }
        p += 32;
        if p >= dim {
            break;
        }
    }
    let add = super::f16_add;
    let t3: [u16; 16] = from_fn(|k| add(sum[k], sum[k + 16]));
    let t6: [u16; 8] = from_fn(|k| add(t3[k], t3[k + 8]));
    let t8: [u16; 4] = from_fn(|k| add(t6[k], t6[k + 4]));
    let res = add(add(t8[0], t8[2]), add(t8[1], t8[3]));
    fp16_to_fp32(if l2 { res } else { f16_sub(F16_ONE, res) })
}

/// The product terms of bfloat16 elements `i` of `a` and `b` (as single precision).
#[inline]
fn bf_factors(a: &[u16], b: &[u16], i: usize, l2: bool) -> (f32, f32) {
    factors(l2, bf16_to_f32(a[i]), bf16_to_f32(b[i]))
}

/// `BF16_L2SqrSIMD32_AVX2` / `BF16_InnerProductSIMD32_AVX2` (the sum): one 8-lane accumulator; a
/// 16-element load widens elements 0-3 and 8-11 into the low half's lanes, 4-7 and 12-15 into
/// the high half's, each half multiplied then added.
fn bf16_avx2(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let dim = a.len();
    let residual = dim % 32;
    let lo = |l: usize| if l < 4 { l } else { l + 4 };
    let mut sum = [0f32; 8];
    let half = |sum: &mut [f32; 8], p: usize, offset: usize, kept: usize| {
        for (l, s) in sum.iter_mut().enumerate() {
            let (m, n) = if l < kept { bf_factors(a, b, p + lo(l) + offset, l2) } else { (0.0, 0.0) };
            *s += m * n;
        }
    };
    let r = residual % 16;
    if r > 0 {
        // lanes of the low and high halves blended in
        let (low, high) = match r {
            1..=4 => (r, 0),
            5..=8 => (4, r - 4),
            9..=11 => (r - 4, 4),
            _ => (8, r - 8),
        };
        if r > 4 {
            half(&mut sum, 0, 4, high);
        }
        half(&mut sum, 0, 0, low);
    }
    let mut p = r;
    let full = |sum: &mut [f32; 8], p: &mut usize| {
        half(sum, *p, 0, 8);
        half(sum, *p, 4, 8);
        *p += 16;
    };
    if residual >= 16 {
        full(&mut sum, &mut p);
    }
    loop {
        full(&mut sum, &mut p);
        full(&mut sum, &mut p);
        if p >= dim {
            break;
        }
    }
    seq(&sum)
}

/// `BF16_L2SqrSIMD32_SSE3` / `BF16_InnerProductSIMD32_SSE3` (the sum): one 4-lane accumulator,
/// 8-element steps in two halves, the partial vector of `dim % 4` then a lone half first.
fn bf16_sse3(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let dim = a.len();
    let residual = dim % 32;
    let mut sum = [0f32; 4];
    let quad = |sum: &mut [f32; 4], p: usize, kept: usize| {
        for (l, s) in sum.iter_mut().enumerate() {
            let (m, n) = if l < kept { bf_factors(a, b, p + l, l2) } else { (0.0, 0.0) };
            *s += m * n;
        }
    };
    let r = residual % 4;
    if r > 0 {
        quad(&mut sum, 0, r);
    }
    let mut p = r;
    if residual % 8 >= 4 {
        quad(&mut sum, p, 4);
        p += 4;
    }
    let step = |sum: &mut [f32; 4], p: &mut usize| {
        quad(sum, *p, 4);
        quad(sum, *p + 4, 4);
        *p += 8;
    };
    for min in [24, 16, 8] {
        if residual >= min {
            step(&mut sum, &mut p);
        }
    }
    loop {
        for _ in 0..4 {
            step(&mut sum, &mut p);
        }
        if p >= dim {
            break;
        }
    }
    seq(&sum)
}

/// `BF16_L2SqrSIMD32_AVX512BW_VBMI2` / `BF16_InnerProductSIMD32_AVX512BW_VBMI2` (the sum): one
/// 16-lane accumulator of fused multiply-adds; the residual's 16 and then `dim % 16` elements
/// expand-loaded into consecutive lanes, then 32-element steps whose low half widens elements
/// 0-3, 8-11, 16-19, 24-27 and high half the four after each.
fn bf16_avx512bw(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let dim = a.len();
    let residual = dim % 32;
    let mut sum = [0f32; 16];
    let fma_lanes = |sum: &mut [f32; 16], at: &dyn Fn(usize) -> Option<usize>| {
        for (l, s) in sum.iter_mut().enumerate() {
            let (m, n) = at(l).map_or((0.0, 0.0), |i| bf_factors(a, b, i, l2));
            *s = m.mul_add(n, *s);
        }
    };
    let mut p = 0;
    if residual > 0 {
        if residual >= 16 {
            fma_lanes(&mut sum, &|l| Some(l));
            p += 16;
        }
        if residual != 16 {
            let (q, r) = (p, residual % 16);
            fma_lanes(&mut sum, &|l| (l < r).then_some(q + l));
            p += r;
        }
    }
    loop {
        let q = p;
        fma_lanes(&mut sum, &|l| Some(q + (l / 4) * 8 + l % 4));
        fma_lanes(&mut sum, &|l| Some(q + (l / 4) * 8 + l % 4 + 4));
        p += 32;
        if p >= dim {
            break;
        }
    }
    reduce512(&sum)
}

/// `VDPBF16PS`'s fused multiply-add: denormal inputs read as zero, a denormal result flushed.
fn dp_fma(x: f32, y: f32, acc: f32) -> f32 {
    ftz(ftz(x).mul_add(ftz(y), ftz(acc)))
}

/// `BF16_InnerProductSIMD32_AVX512BF16_VL` (the sum): `VDPBF16PS` into one 16-lane accumulator,
/// each lane adding its odd element's product and then its even one's.
fn bf16_avx512bf16(a: &[u16], b: &[u16]) -> f32 {
    let dim = a.len();
    let residual = dim % 32;
    let mut sum = [0f32; 16];
    let dp = |sum: &mut [f32; 16], p: usize, kept: usize| {
        let e = |v: &[u16], w: usize| if w < kept { bf16_to_f32(v[p + w]) } else { 0.0 };
        for (j, s) in sum.iter_mut().enumerate() {
            *s = dp_fma(e(a, 2 * j + 1), e(b, 2 * j + 1), *s);
            *s = dp_fma(e(a, 2 * j), e(b, 2 * j), *s);
        }
    };
    if residual > 0 {
        dp(&mut sum, 0, residual);
    }
    let mut p = residual;
    loop {
        dp(&mut sum, p, 32);
        p += 32;
        if p >= dim {
            break;
        }
    }
    reduce512(&sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(n: usize, seed: u32) -> Vec<f32> {
        (0..n).map(|i| (((i as u32).wrapping_mul(2654435761) ^ seed) % 1000) as f32 / 37.0 - 13.0).collect()
    }

    /// Every kernel visits each element exactly once: with small integers (exact sums) each
    /// equals the plain sum, whatever its residual.
    #[test]
    fn kernels_cover_every_element() {
        for dim in 32..100 {
            let (a, b) = (vals(dim, 1), vals(dim, 7));
            let (a, b): (Vec<f32>, Vec<f32>) = (a.iter().map(|v| v.round()).collect(), b.iter().map(|v| v.round()).collect());
            for l2 in [true, false] {
                let want = scalar(&a, &b, l2);
                assert_eq!(reduce512(&lanes::<f32, 16, 2>(&a, &b, l2, true)), want, "avx512 {dim}");
                assert_eq!(seq(&lanes::<f32, 8, 4>(&a, &b, l2, false)), want, "avx {dim}");
                assert_eq!(fp32_sse(&a, &b, l2), want, "sse {dim}");
                let (x, y): (Vec<f64>, Vec<f64>) = (a.iter().map(|&v| v as f64).collect(), b.iter().map(|&v| v as f64).collect());
                assert_eq!(seq(&lanes::<f64, 2, 2>(&x, &y, l2, false)), want as f64, "fp64 sse {dim}");
                let (h, k): (Vec<u16>, Vec<u16>) = (a.iter().map(|&v| super::super::f32_to_bf16(v)).collect(), b.iter().map(|&v| super::super::f32_to_bf16(v)).collect());
                assert_eq!(bf16_avx2(&h, &k, l2), want, "bf16 avx2 {dim}");
                assert_eq!(bf16_sse3(&h, &k, l2), want, "bf16 sse3 {dim}");
                assert_eq!(bf16_avx512bw(&h, &k, l2), want, "bf16 avx512bw {dim}");
                if !l2 {
                    assert_eq!(bf16_avx512bf16(&h, &k), want, "bf16 avx512bf16 {dim}");
                }
            }
        }
    }
}
