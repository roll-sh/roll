//! VecSim, the vector similarity library RediSearch 8.10.1 bundles, reimplemented: a field's
//! vector index (vectors stored by internal id with the labels — doc ids — they belong to,
//! deletes swapping the last vector into the hole), the distance functions bit for bit as the
//! reference's aarch64 build computes them (its NEON, NEON_HP, NEON_BF16 and NEON_DOTPROD
//! kernels with their accumulator layouts, fused multiply-adds and half precision or
//! round-to-odd BFDOT arithmetic, the scalar fallbacks for short vectors), cosine
//! normalization, top-k, range and batch queries, and the hybrid policy heuristics.
//!
//! FLAT is brute force by definition. HNSW fields are tiered HNSW graphs built and searched as
//! VecSim does (see [`super::hnsw`]), approximately. SVS-VAMANA indexes are searched exactly
//! (none of the reference's tests give them vectors: their shared thread pool's memory would show
//! in every later FT.INFO).

use super::spec::{Algo, Metric, VecType, VectorParams};
use std::collections::{BTreeSet, HashMap};

// ---------------------------------------------------------------------------------------------
// Soft half precision (FPCR.FZ16 clear: subnormals kept, round to nearest even)

/// `mant * 2^exp` (exact, non-zero) rounded to binary16's magnitude bits.
fn round_f16(mant: u128, exp: i32) -> u16 {
    let n = 128 - mant.leading_zeros() as i32;
    let e = n - 1 + exp;
    let mut q = (e - 10).max(-24);
    let shift = q - exp;
    let mut m = if shift <= 0 {
        mant << (-shift) as u32
    } else if shift > 127 {
        0
    } else {
        let rem = mant & ((1u128 << shift) - 1);
        let half = 1u128 << (shift - 1);
        let mut m = mant >> shift;
        if rem > half || (rem == half && m & 1 == 1) {
            m += 1;
        }
        m
    };
    if m >= 1 << 11 {
        m >>= 1;
        q += 1;
    }
    if m >= 1 << 10 && q + 10 > 15 {
        return 0x7c00;
    }
    if m < 1 << 10 {
        return m as u16;
    }
    (((q + 10 + 15) as u16) << 10) | (m as u16 & 0x3ff)
}

/// A finite binary16's `(negative, mantissa, exponent)`: the value is `mantissa * 2^exponent`.
fn f16_parts(h: u16) -> (bool, u64, i32) {
    let e = ((h >> 10) & 0x1f) as i32;
    let f = (h & 0x3ff) as u64;
    let neg = h & 0x8000 != 0;
    if e == 0 { (neg, f, -24) } else { (neg, f | 0x400, e - 25) }
}

fn f16_finite(h: u16) -> bool {
    h & 0x7c00 != 0x7c00
}

/// An exact binary16 value as f64.
fn f16_to_f64(h: u16) -> f64 {
    if !f16_finite(h) {
        let s = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
        return if h & 0x3ff != 0 { f64::NAN } else { s * f64::INFINITY };
    }
    let (neg, m, e) = f16_parts(h);
    let v = m as f64 * (e as f64).exp2();
    if neg { -v } else { v }
}

/// A non-finite result (the inputs had an infinity or NaN): computed in f64, which is exact
/// enough to tell infinities from NaNs.
fn f16_from_special(v: f64) -> u16 {
    if v.is_nan() {
        0x7e00
    } else if v.is_infinite() {
        if v < 0.0 { 0xfc00 } else { 0x7c00 }
    } else {
        // finite (an infinity times zero is NaN, handled above): not reachable with a
        // non-finite input, but round anyway.
        f64_to_f16(v)
    }
}

/// An f64 rounded to binary16 (nearest even).
fn f64_to_f16(v: f64) -> u16 {
    if v.is_nan() {
        return 0x7e00;
    }
    let sign = if v.is_sign_negative() { 0x8000 } else { 0 };
    if v.is_infinite() {
        return sign | 0x7c00;
    }
    if v == 0.0 {
        return sign;
    }
    let bits = v.to_bits();
    let e = ((bits >> 52) & 0x7ff) as i32;
    let f = bits & ((1u64 << 52) - 1);
    let (m, x) = if e == 0 { (f, -1074) } else { (f | (1u64 << 52), e - 1075) };
    sign | round_f16(m as u128, x)
}

/// `vfmaq_f16`'s lane: `a * b + c` rounded once.
fn f16_fma(a: u16, b: u16, c: u16) -> u16 {
    if !f16_finite(a) || !f16_finite(b) || !f16_finite(c) {
        return f16_from_special(f16_to_f64(a).mul_add(f16_to_f64(b), f16_to_f64(c)));
    }
    let (na, ma, ea) = f16_parts(a);
    let (nb, mb, eb) = f16_parts(b);
    let (nc, mc, ec) = f16_parts(c);
    // everything as a multiple of 2^-48
    let p = (ma as i128 * mb as i128) << (ea + eb + 48) as u32;
    let p = if na != nb { -p } else { p };
    let z = (mc as i128) << (ec + 48) as u32;
    let z = if nc { -z } else { z };
    let s = p + z;
    if s == 0 {
        let pneg = na != nb;
        return if p == 0 && z == 0 && pneg && nc { 0x8000 } else { 0 };
    }
    let sign = if s < 0 { 0x8000 } else { 0 };
    sign | round_f16(s.unsigned_abs(), -48)
}

const F16_ONE: u16 = 0x3c00;

fn f16_add(a: u16, b: u16) -> u16 {
    f16_fma(a, F16_ONE, b)
}

fn f16_sub(a: u16, b: u16) -> u16 {
    f16_fma(a, F16_ONE, b ^ 0x8000)
}

/// `FP16_to_FP32`: exact.
pub fn fp16_to_fp32(h: u16) -> f32 {
    let shifted_exp: u32 = 0x7c00 << 13;
    let mut o: u32 = ((h & 0x7fff) as u32) << 13;
    let exp = shifted_exp & o;
    o = o.wrapping_add((127 - 15) << 23);
    let infnan = o.wrapping_add((128 - 16) << 23);
    let zerodenorm = (f32::from_bits(o.wrapping_add(1 << 23)) - f32::from_bits(113 << 23)).to_bits();
    let reg = if exp == 0 { zerodenorm } else { o };
    let sign = ((h & 0x8000) as u32) << 16;
    f32::from_bits((if exp == shifted_exp { infnan } else { reg }) | sign)
}

/// VecSim's `FP32_to_FP16` (ryg's conversion, NaNs kept).
pub fn fp32_to_fp16(input: f32) -> u16 {
    let mut fint = input.to_bits();
    let sign = fint & 0x8000_0000;
    fint ^= sign;
    let f32infty: u32 = 255 << 23;
    let mut o: u32 = if fint > f32infty { 0x7e00 } else { 0x7c00 };
    let round_mask: u32 = !0xfff;
    let magic = f32::from_bits(15 << 23);
    let mut fscale = f32::from_bits(fint & round_mask) * magic;
    fscale = fscale.min(f32::from_bits((31 << 23) - 0x1000));
    let fint2 = fscale.to_bits().wrapping_sub(round_mask);
    if fint < f32infty {
        o = fint2 >> 13;
    }
    (o | (sign >> 16)) as u16
}

fn bf16_to_f32(h: u16) -> f32 {
    f32::from_bits((h as u32) << 16)
}

/// `float_to_bf16`: round to nearest even by adding into the low half.
pub fn f32_to_bf16(f: f32) -> u16 {
    let b = f.to_bits();
    let lsb = (b >> 16) & 1;
    (b.wrapping_add(lsb + 0x7fff) >> 16) as u16
}

// ---------------------------------------------------------------------------------------------
// BFDOT (FEAT_BF16 with FPCR.EBF clear): products and sums rounded to odd, flushing subnormals

/// `BFRound`: an exact `s + err` (a TwoSum pair) rounded to odd at f32's precision, flushing
/// results below the normal range to zero and overflowing to infinity.
fn bf_round(s: f64, err: f64) -> f32 {
    if s == 0.0 {
        return 0.0;
    }
    // round to odd at f64's precision first (innocuous before rounding to odd at 24 bits)
    let mut v = s;
    if err != 0.0 && s.to_bits() & 1 == 0 {
        v = if (err > 0.0) == (s > 0.0) {
            f64::from_bits(s.to_bits() + 1)
        } else {
            f64::from_bits(s.to_bits() - 1)
        };
    }
    let bits = v.to_bits();
    let sign = (bits >> 63) as u32;
    let e = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if e < -126 {
        return f32::from_bits(sign << 31);
    }
    if e > 127 {
        return f32::from_bits((sign << 31) | 0x7f80_0000);
    }
    let frac = bits & ((1u64 << 52) - 1);
    let mut m = (frac >> 29) as u32;
    if frac & ((1u64 << 29) - 1) != 0 {
        m |= 1;
    }
    f32::from_bits((sign << 31) | (((e + 127) as u32) << 23) | m)
}

fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    let err = (a - (s - bb)) + (b - bb);
    (s, err)
}

/// `BFUnpack`'s flush: subnormal inputs read as zero.
fn ftz(x: f32) -> f32 {
    if x.is_subnormal() { 0.0f32.copysign(x) } else { x }
}

/// `BFAdd`.
fn bf_add(a: f32, b: f32) -> f32 {
    let (a, b) = (ftz(a), ftz(b));
    if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
        return a + b;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() && b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    let (s, err) = two_sum(a as f64, b as f64);
    if s == 0.0 && err == 0.0 {
        return 0.0;
    }
    bf_round(s, err)
}

/// `BFMulH`: the product of two bfloat16s (exact in 16 bits), flushed or overflowed.
fn bf_mul(a: f32, b: f32) -> f32 {
    let (a, b) = (ftz(a), ftz(b));
    let p = a as f64 * b as f64;
    if p.is_nan() || p.is_infinite() {
        return p as f32;
    }
    if p == 0.0 {
        return if p.is_sign_negative() { -0.0 } else { 0.0 };
    }
    bf_round(p, 0.0)
}

/// `vbfdotq_f32`'s lane: `acc + (a0*b0 + a1*b1)`.
fn bfdot_lane(acc: f32, a0: u16, b0: u16, a1: u16, b1: u16) -> f32 {
    let prod = bf_add(bf_mul(bf16_to_f32(a0), bf16_to_f32(b0)), bf_mul(bf16_to_f32(a1), bf16_to_f32(b1)));
    bf_add(acc, prod)
}

// ---------------------------------------------------------------------------------------------
// Distance functions

fn f32s(b: &[u8], dim: usize) -> Vec<f32> {
    (0..dim).map(|i| f32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())).collect()
}

fn f64s(b: &[u8], dim: usize) -> Vec<f64> {
    (0..dim).map(|i| f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())).collect()
}

fn u16s(b: &[u8], dim: usize) -> Vec<u16> {
    (0..dim).map(|i| u16::from_le_bytes([b[i * 2], b[i * 2 + 1]])).collect()
}

/// `FP32_L2SqrSIMD16_NEON` / `FP32_InnerProductSIMD16_NEON` (the sum; IP returns 1 - sum).
fn fp32_neon(a: &[f32], b: &[f32], l2: bool) -> f32 {
    let dim = a.len();
    let mut sum = [[0f32; 4]; 4];
    let step = |s: &mut [f32; 4], x: &[f32], y: &[f32]| {
        for l in 0..4 {
            if l2 {
                let d = x[l] - y[l];
                s[l] = d.mul_add(d, s[l]);
            } else {
                s[l] = x[l].mul_add(y[l], s[l]);
            }
        }
    };
    let mut p = 0;
    for _ in 0..dim / 16 {
        for s in &mut sum {
            step(s, &a[p..p + 4], &b[p..p + 4]);
            p += 4;
        }
    }
    let residual = dim % 16;
    for s in sum.iter_mut().take(residual / 4) {
        step(s, &a[p..p + 4], &b[p..p + 4]);
        p += 4;
    }
    let fr = residual % 4;
    if fr > 0 {
        let mut x = [0f32; 4];
        let mut y = [0f32; 4];
        x[..fr].copy_from_slice(&a[p..p + fr]);
        y[..fr].copy_from_slice(&b[p..p + fr]);
        step(&mut sum[3], &x, &y);
    }
    let c: Vec<f32> = (0..4).map(|l| (sum[0][l] + sum[1][l]) + (sum[2][l] + sum[3][l])).collect();
    let h0 = c[0] + c[2];
    let h1 = c[1] + c[3];
    h0 + h1
}

/// `FP64_L2SqrSIMD8_NEON` / `FP64_InnerProductSIMD8_NEON` (the sum).
fn fp64_neon(a: &[f64], b: &[f64], l2: bool) -> f64 {
    let dim = a.len();
    let mut sum = [[0f64; 2]; 4];
    let step = |s: &mut [f64; 2], x: &[f64], y: &[f64]| {
        for l in 0..2 {
            if l2 {
                let d = x[l] - y[l];
                s[l] = d.mul_add(d, s[l]);
            } else {
                s[l] = x[l].mul_add(y[l], s[l]);
            }
        }
    };
    let mut p = 0;
    for _ in 0..dim / 8 {
        for s in &mut sum {
            step(s, &a[p..p + 2], &b[p..p + 2]);
            p += 2;
        }
    }
    let residual = dim % 8;
    for s in sum.iter_mut().take(residual / 2) {
        step(s, &a[p..p + 2], &b[p..p + 2]);
        p += 2;
    }
    if residual % 2 == 1 {
        step(&mut sum[3], &[a[p], 0.0], &[b[p], 0.0]);
    }
    let c0 = (sum[0][0] + sum[1][0]) + (sum[2][0] + sum[3][0]);
    let c1 = (sum[0][1] + sum[1][1]) + (sum[2][1] + sum[3][1]);
    c0 + c1
}

/// The NEON_HP / NEON_BF16 kernels' shape (chunks of 32 in four 8-lane accumulators, the
/// partial chunk of `dim % 8` first): calls `step(acc, lanes_a, lanes_b)` in the kernels'
/// order, the partial chunk's missing lanes zeroed.
fn chunked8<A>(a: &[u16], b: &[u16], acc: &mut [A; 4], mut step: impl FnMut(&mut A, &[u16; 8], &[u16; 8])) {
    let dim = a.len();
    let residual = dim % 32;
    let mut p = 0;
    let cr = residual % 8;
    if cr != 0 {
        let mut x = [0u16; 8];
        let mut y = [0u16; 8];
        x[..cr].copy_from_slice(&a[..cr]);
        y[..cr].copy_from_slice(&b[..cr]);
        step(&mut acc[0], &x, &y);
        p = cr;
    }
    let lanes = |v: &[u16], p: usize| -> [u16; 8] { v[p..p + 8].try_into().unwrap() };
    for (i, min) in [(1usize, 8usize), (2, 16), (3, 24)] {
        if residual >= min {
            step(&mut acc[i], &lanes(a, p), &lanes(b, p));
            p += 8;
        }
    }
    while p < dim {
        for s in acc.iter_mut() {
            step(s, &lanes(a, p), &lanes(b, p));
            p += 8;
        }
    }
}

/// `FP16_L2Sqr_NEON_HP` / `FP16_InnerProduct_NEON_HP` (the sum): half precision accumulators,
/// pairwise half precision folds, then single precision.
fn fp16_neon(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let mut acc = [[0u16; 8]; 4];
    chunked8(a, b, &mut acc, |s, x, y| {
        for l in 0..8 {
            s[l] = if l2 {
                let d = f16_sub(x[l], y[l]);
                f16_fma(d, d, s[l])
            } else {
                f16_fma(x[l], y[l], s[l])
            };
        }
    });
    let padd = |x: &[u16; 8], y: &[u16; 8]| -> [u16; 8] {
        [
            f16_add(x[0], x[1]),
            f16_add(x[2], x[3]),
            f16_add(x[4], x[5]),
            f16_add(x[6], x[7]),
            f16_add(y[0], y[1]),
            f16_add(y[2], y[3]),
            f16_add(y[4], y[5]),
            f16_add(y[6], y[7]),
        ]
    };
    let a1 = padd(&acc[0], &acc[2]);
    let a2 = padd(&acc[1], &acc[3]);
    let a1 = padd(&a1, &a2);
    let s: Vec<f32> = (0..4).map(|l| fp16_to_fp32(a1[l]) + fp16_to_fp32(a1[l + 4])).collect();
    (s[0] + s[2]) + (s[1] + s[3])
}

/// `BF16_L2Sqr_NEON` / `BF16_InnerProduct_NEON` (the sum): single precision accumulators,
/// L2 by fused multiply-adds of the low then high halves, IP by BFDOT.
fn bf16_neon(a: &[u16], b: &[u16], l2: bool) -> f32 {
    let mut acc = [[0f32; 4]; 4];
    chunked8(a, b, &mut acc, |s, x, y| {
        if l2 {
            for half in 0..2 {
                for l in 0..4 {
                    let d = bf16_to_f32(x[half * 4 + l]) - bf16_to_f32(y[half * 4 + l]);
                    s[l] = d.mul_add(d, s[l]);
                }
            }
        } else {
            for l in 0..4 {
                s[l] = bfdot_lane(s[l], x[2 * l], y[2 * l], x[2 * l + 1], y[2 * l + 1]);
            }
        }
    });
    let padd = |x: &[f32; 4], y: &[f32; 4]| -> [f32; 4] { [x[0] + x[1], x[2] + x[3], y[0] + y[1], y[2] + y[3]] };
    let a1 = padd(&acc[0], &acc[2]);
    let a2 = padd(&acc[1], &acc[3]);
    let a1 = padd(&a1, &a2);
    (a1[0] + a1[2]) + (a1[1] + a1[3])
}

/// A field's distance function (`spaces::GetDistFunc` on the reference's CPU).
pub(super) fn distance(ty: VecType, metric: Metric, dim: usize, a: &[u8], b: &[u8]) -> f64 {
    let l2 = metric == Metric::L2;
    match ty {
        VecType::Float32 => {
            let s = fp32_neon(&f32s(a, dim), &f32s(b, dim), l2);
            (if l2 { s } else { 1.0f32 - s }) as f64
        }
        VecType::Float64 => {
            let s = fp64_neon(&f64s(a, dim), &f64s(b, dim), l2);
            if l2 { s } else { 1.0 - s }
        }
        VecType::Float16 => {
            let (x, y) = (u16s(a, dim), u16s(b, dim));
            if dim >= 8 {
                let s = fp16_neon(&x, &y, l2);
                return (if l2 { s } else { 1.0f32 - s }) as f64;
            }
            let mut res = 0f32;
            for i in 0..dim {
                if l2 {
                    let t = fp16_to_fp32(x[i]) - fp16_to_fp32(y[i]);
                    res = t.mul_add(t, res);
                } else {
                    res += fp16_to_fp32(x[i]) * fp16_to_fp32(y[i]);
                }
            }
            (if l2 { res } else { 1.0f32 - res }) as f64
        }
        VecType::BFloat16 => {
            let (x, y) = (u16s(a, dim), u16s(b, dim));
            if dim >= 8 {
                let s = bf16_neon(&x, &y, l2);
                return (if l2 { s } else { 1.0f32 - s }) as f64;
            }
            let mut res = 0f32;
            for i in 0..dim {
                if l2 {
                    let d = bf16_to_f32(x[i]) - bf16_to_f32(y[i]);
                    res = d.mul_add(d, res);
                } else {
                    res += bf16_to_f32(x[i]) * bf16_to_f32(y[i]);
                }
            }
            (if l2 { res } else { 1.0f32 - res }) as f64
        }
        VecType::Int8 | VecType::Uint8 => {
            let val = |v: &[u8], i: usize| -> i64 { if ty == VecType::Int8 { v[i] as i8 as i64 } else { v[i] as i64 } };
            let simd = dim >= 16;
            match metric {
                Metric::L2 => {
                    let s: i64 = (0..dim).map(|i| (val(a, i) - val(b, i)).pow(2)).sum();
                    s as f32 as f64
                }
                Metric::Ip => {
                    let s: i64 = (0..dim).map(|i| val(a, i) * val(b, i)).sum();
                    // the scalar subtracts in integers, the kernels in floats
                    (if simd { 1.0f32 - s as f32 } else { (1 - s) as f32 }) as f64
                }
                Metric::Cosine => {
                    let s: i64 = (0..dim).map(|i| val(a, i) * val(b, i)).sum();
                    let n1 = f32::from_le_bytes(a[dim..dim + 4].try_into().unwrap());
                    let n2 = f32::from_le_bytes(b[dim..dim + 4].try_into().unwrap());
                    (1.0f32 - s as f32 / (n1 * n2)) as f64
                }
            }
        }
    }
}

/// The index's normalization (`GetNormalizeFunc`), in place; integers append their norm.
fn normalize(ty: VecType, dim: usize, v: &mut Vec<u8>) {
    match ty {
        VecType::Float32 => {
            let x = f32s(v, dim);
            let mut sum = 0f64;
            for &e in &x {
                sum += e as f64 * e as f64;
            }
            let norm = sum.sqrt() as f32;
            for (i, e) in x.iter().enumerate() {
                v[i * 4..i * 4 + 4].copy_from_slice(&(e / norm).to_le_bytes());
            }
        }
        VecType::Float64 => {
            // the compiled loop: pairs multiplied then added, the odd tail fused
            let x = f64s(v, dim);
            let mut sum = 0f64;
            let pairs = if dim == 1 { 0 } else { dim & !1 };
            for &e in &x[..pairs] {
                sum += e * e;
            }
            for &e in &x[pairs..] {
                sum = e.mul_add(e, sum);
            }
            let norm = sum.sqrt();
            for (i, e) in x.iter().enumerate() {
                v[i * 8..i * 8 + 8].copy_from_slice(&(e / norm).to_le_bytes());
            }
        }
        VecType::Float16 | VecType::BFloat16 => {
            let half = ty == VecType::Float16;
            let x: Vec<f32> = u16s(v, dim).into_iter().map(|h| if half { fp16_to_fp32(h) } else { bf16_to_f32(h) }).collect();
            let mut sum = 0f32;
            for &e in &x {
                sum += e * e;
            }
            let norm = sum.sqrt();
            for (i, e) in x.iter().enumerate() {
                let h = if half { fp32_to_fp16(e / norm) } else { f32_to_bf16(e / norm) };
                v[i * 2..i * 2 + 2].copy_from_slice(&h.to_le_bytes());
            }
        }
        VecType::Int8 | VecType::Uint8 => {
            let mut sum: i32 = 0;
            for &b in &v[..dim] {
                let e = if ty == VecType::Int8 { b as i8 as i32 } else { b as i32 };
                sum = sum.wrapping_add(e * e);
            }
            let norm = (sum as f64).sqrt() as f32;
            v.truncate(dim);
            v.extend_from_slice(&norm.to_le_bytes());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The index

/// A score as an ordered key: C++'s `<` on non-NaN values (zeros alike).
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub(super) struct Score(pub(super) f64);

impl Eq for Score {}

#[allow(clippy::derive_ord_xor_partial_ord)]
impl Ord for Score {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&o.0).unwrap_or_else(|| self.0.is_nan().cmp(&o.0.is_nan()))
    }
}

pub(super) fn key(s: f64) -> Score {
    Score(if s == 0.0 { 0.0 } else { s })
}

/// A query reply's result: the label (doc id) and its distance.
pub type VecRes = (u64, f64);

/// The search mode a query ran in (`VecSearchMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    StandardKnn,
    HybridAdhocBf,
    HybridBatches,
    HybridBatchesToAdhocBf,
    RangeQuery,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::StandardKnn => "STANDARD_KNN",
            Mode::HybridAdhocBf => "HYBRID_ADHOC_BF",
            Mode::HybridBatches => "HYBRID_BATCHES",
            Mode::HybridBatchesToAdhocBf => "HYBRID_BATCHES_TO_ADHOC_BF",
            Mode::RangeQuery => "RANGE_QUERY",
        }
    }
}

/// A field's vector index.
#[derive(Clone, Debug)]
pub struct VecIndex {
    pub algo: Algo,
    pub ty: VecType,
    pub dim: usize,
    pub metric: Metric,
    pub multi: bool,
    pub m: usize,
    pub block_size: usize,
    /// `getStoredDataSize`.
    pub stored: usize,
    data: Vec<u8>,
    labels: Vec<u64>,
    lookup: HashMap<u64, Vec<u32>>,
    /// An HNSW field's graph and tiered layer.
    pub hnsw: Option<super::hnsw::Hnsw>,
    /// `flatBufferLimit` (`search-tiered-hnsw-buffer-limit` when the index was created).
    pub buffer_limit: usize,
    /// SVS-VAMANA's training threshold, and whether its flat buffer ever reached it.
    svs_threshold: usize,
    trained: bool,
    /// The allocator's view for FT.INFO (see [`VecIndex::memory`]).
    alloc: Alloc,
    /// `lastMode`: the last search mode (FT.DEBUG VECSIM_INFO), None for `EMPTY_MODE`.
    last_mode: std::cell::Cell<Option<Mode>>,
    /// A tiered index's flat buffer's own `lastMode`.
    front_mode: std::cell::Cell<Option<Mode>>,
}

/// What a FLAT index's allocator holds besides the vectors: `idToLabelMapping`'s capacity, its
/// label lookup's bucket count and its data blocks vector's capacity.
#[derive(Clone, Debug)]
struct Alloc {
    cap: usize,
    buckets: usize,
    blocks_cap: usize,
}

impl VecIndex {
    pub fn new(p: &VectorParams) -> VecIndex {
        VecIndex::with_limit(p, 1024)
    }

    /// A field's index, the tiered buffer limit given.
    pub fn with_limit(p: &VectorParams, buffer_limit: usize) -> VecIndex {
        let stored = super::vector::stored_data_size(p);
        // `min(DEFAULT_BLOCK_SIZE, limit / element size)` (BLOCK_SIZE itself is ignored)
        let block_size = if p.block_size == 0 { 1024 } else { p.block_size };
        let hnsw = (p.algo == Algo::Hnsw).then(|| super::hnsw::Hnsw::new(p.ty, p.metric, p.dim, stored, p.multi, p.m, p.ef_construction, p.ef_runtime, p.epsilon, block_size));
        VecIndex {
            algo: p.algo,
            ty: p.ty,
            dim: p.dim,
            metric: p.metric,
            multi: p.multi,
            m: if p.m == 0 { 16 } else { p.m },
            block_size,
            stored,
            data: Vec::new(),
            labels: Vec::new(),
            lookup: HashMap::new(),
            hnsw,
            buffer_limit,
            svs_threshold: if p.training_threshold == 0 { 10 * 1024 } else { p.training_threshold },
            trained: false,
            alloc: Alloc { cap: 0, buckets: 1, blocks_cap: 0 },
            last_mode: std::cell::Cell::new(None),
            front_mode: std::cell::Cell::new(None),
        }
    }

    /// `indexSize`: the vectors stored (an HNSW graph's marked deleted ones too).
    pub fn size(&self) -> usize {
        match &self.hnsw {
            Some(h) => h.size(),
            None => self.labels.len(),
        }
    }

    /// HNSW's elements marked deleted (`numMarkedDeleted`).
    pub fn marked(&self) -> usize {
        self.hnsw.as_ref().map_or(0, |h| h.marked)
    }

    /// HNSW's vectors inserted straight into the graph (`directHNSWInsertions`).
    pub fn direct_insertions(&self) -> usize {
        self.hnsw.as_ref().map_or(0, |h| h.direct)
    }

    /// HNSW's `maxLevel` and entry point's label (None while the graph is empty).
    pub fn entry_point(&self) -> Option<(usize, u64)> {
        self.hnsw.as_ref().and_then(|h| h.entry_point())
    }

    /// The vectors searchable.
    fn count(&self) -> usize {
        self.labels.len()
    }

    /// `indexLabelCount`.
    pub fn label_count(&self) -> usize {
        match &self.hnsw {
            Some(h) => h.label_count(),
            None => self.lookup.len(),
        }
    }

    fn vector(&self, id: usize) -> &[u8] {
        &self.data[id * self.stored..(id + 1) * self.stored]
    }

    /// `preprocessForStorage` / `preprocessQuery`: a copy, normalized for cosine.
    pub fn preprocess(&self, blob: &[u8]) -> Vec<u8> {
        let mut v = blob[..blob.len().min(self.ty.size() * self.dim)].to_vec();
        if self.metric == Metric::Cosine {
            normalize(self.ty, self.dim, &mut v);
        }
        v.resize(self.stored, 0);
        v
    }

    /// The distance between a stored vector and a preprocessed query.
    pub fn distance(&self, stored: &[u8], q: &[u8]) -> f64 {
        distance(self.ty, self.metric, self.dim, stored, q)
    }

    /// `addVector`: a single-value index overwrites a label's vector (as given); a tiered HNSW
    /// index writes as the worker pool's mode says.
    pub fn add(&mut self, blob: &[u8], label: u64, mode: super::hnsw::WriteMode) {
        if self.hnsw.is_some() {
            let v = self.preprocess(blob);
            let limit = self.buffer_limit;
            if let Some(h) = &mut self.hnsw {
                h.add(&v, label, mode, limit);
            }
            return;
        }
        if !self.multi
            && let Some(ids) = self.lookup.get(&label)
        {
            let id = ids[0] as usize;
            let n = blob.len().min(self.stored);
            self.data[id * self.stored..id * self.stored + n].copy_from_slice(&blob[..n]);
            return;
        }
        let v = self.preprocess(blob);
        let id = self.labels.len() as u32;
        self.grow();
        self.data.extend_from_slice(&v);
        if self.algo == Algo::Svs && self.labels.len() + 1 >= self.svs_threshold {
            self.trained = true;
        }
        self.labels.push(label);
        self.lookup.entry(label).or_default().push(id);
    }

    /// `removeVector`: the last vector moves into the hole.
    fn remove_id(&mut self, id: u32) {
        let last = self.labels.len() as u32 - 1;
        if id != last {
            let last_label = self.labels[last as usize];
            self.labels[id as usize] = last_label;
            if let Some(ids) = self.lookup.get_mut(&last_label)
                && let Some(p) = ids.iter().rposition(|&x| x == last)
            {
                ids[p] = id;
            }
            let s = self.stored;
            self.data.copy_within(last as usize * s..(last as usize + 1) * s, id as usize * s);
        }
        self.labels.pop();
        self.data.truncate(self.labels.len() * self.stored);
        self.shrink();
    }

    /// `deleteVector`: the label's vectors, each removed in turn (a moved vector of the same
    /// label is seen at its new id).
    pub fn delete(&mut self, label: u64, mode: super::hnsw::WriteMode) -> usize {
        if let Some(h) = &mut self.hnsw {
            return h.delete(label, mode);
        }
        let Some(n) = self.lookup.get(&label).map(|v| v.len()) else { return 0 };
        for i in 0..n {
            let id = self.lookup[&label][i];
            self.remove_id(id);
        }
        self.lookup.remove(&label);
        n
    }

    /// `getDistanceFrom_Unsafe`: a label's (closest) distance, NaN without one.
    pub fn distance_from(&self, label: u64, q: &[u8]) -> f64 {
        if let Some(h) = &self.hnsw {
            return h.distance_from(label, q);
        }
        let Some(ids) = self.lookup.get(&label) else { return f64::NAN };
        let mut dist = f64::INFINITY;
        for &id in ids {
            let d = self.distance(self.vector(id as usize), q);
            dist = if dist < d { dist } else { d };
        }
        dist
    }

    /// Every stored vector's `(score, label)`, in storage order.
    fn scan<'s>(&'s self, q: &'s [u8]) -> impl Iterator<Item = (f64, u64)> + 's {
        (0..self.count()).map(move |id| (self.distance(self.vector(id), q), self.labels[id]))
    }

    /// `topKQuery` (sorted by score, then label).
    pub fn top_k(&self, blob: &[u8], k: usize, ef_runtime: usize) -> Vec<VecRes> {
        self.last_mode.set(Some(Mode::StandardKnn));
        if let Some(h) = &self.hnsw {
            return h.top_k(&self.preprocess(blob), k, ef_runtime);
        }
        if k == 0 {
            return Vec::new();
        }
        let q = self.preprocess(blob);
        // the max priority queue of (score, label); multi keeps a label's best score
        let mut heap: BTreeSet<(Score, u64)> = BTreeSet::new();
        let mut best: HashMap<u64, Score> = HashMap::new();
        let mut upper = f64::MIN;
        for (score, label) in self.scan(&q) {
            if score < upper || heap.len() < k {
                if self.multi {
                    match best.get(&label).copied() {
                        None => {
                            heap.insert((key(score), label));
                            best.insert(label, key(score));
                        }
                        Some(old) if old.0 > score => {
                            heap.remove(&(old, label));
                            heap.insert((key(score), label));
                            best.insert(label, key(score));
                        }
                        Some(_) => {}
                    }
                } else {
                    heap.insert((key(score), label));
                }
                if heap.len() > k {
                    let top = *heap.iter().next_back().unwrap();
                    heap.remove(&top);
                    best.remove(&top.1);
                }
                upper = heap.iter().next_back().unwrap().0.0;
            }
        }
        heap.into_iter().map(|(s, l)| (l, s.0)).collect()
    }

    /// `rangeQuery`, sorted by label (`BY_ID`).
    pub fn range(&self, blob: &[u8], radius: f64, epsilon: f64) -> Vec<VecRes> {
        let mut out = self.range_unsorted(blob, radius, epsilon);
        out.sort_by_key(|r| r.0);
        out
    }

    /// `rangeQuery`'s results in the order the scan found them.
    pub fn range_unsorted(&self, blob: &[u8], radius: f64, epsilon: f64) -> Vec<VecRes> {
        self.last_mode.set(Some(Mode::RangeQuery));
        if let Some(h) = &self.hnsw {
            return h.range(&self.preprocess(blob), radius, epsilon);
        }
        let q = self.preprocess(blob);
        let radius = if self.ty == VecType::Float64 { radius } else { radius as f32 as f64 };
        let mut out: Vec<VecRes> = Vec::new();
        let mut at: HashMap<u64, usize> = HashMap::new();
        for (score, label) in self.scan(&q) {
            if score <= radius {
                if self.multi {
                    match at.get(&label) {
                        Some(&i) => {
                            if out[i].1 > score {
                                out[i].1 = score;
                            }
                        }
                        None => {
                            at.insert(label, out.len());
                            out.push((label, score));
                        }
                    }
                } else {
                    out.push((label, score));
                }
            }
        }
        out
    }

    /// `preferAdHocSearch` (a tiered index asks its bigger tier: the graph, or for SVS-VAMANA
    /// its flat buffer).
    pub fn prefer_adhoc(&self, subset: usize, k: usize) -> bool {
        self.prefer_adhoc_check(subset, k, true)
    }

    /// `preferAdHocSearch` with its mode: ad hoc on the first check, batches switching to ad
    /// hoc on a later one, else batches.
    pub fn prefer_adhoc_check(&self, subset: usize, k: usize, initial: bool) -> bool {
        if self.hnsw.is_some() && self.size() == 0 {
            // the tiered index asks its flat buffer (as empty, so ad hoc)
            self.front_mode.set(Some(if initial { Mode::HybridAdhocBf } else { Mode::HybridBatchesToAdhocBf }));
            return true;
        }
        let res = self.prefer_adhoc_tree(subset, k);
        self.last_mode.set(Some(match (res, initial) {
            (true, true) => Mode::HybridAdhocBf,
            (true, false) => Mode::HybridBatchesToAdhocBf,
            (false, _) => Mode::HybridBatches,
        }));
        res
    }

    /// `setLastSearchMode` (an explicit HYBRID_POLICY).
    pub fn set_last_mode(&self, mode: Mode) {
        self.last_mode.set(Some(mode));
    }

    /// `VecSimSearchMode_ToString(lastMode)`.
    pub fn last_mode_name(&self) -> &'static str {
        self.last_mode.get().map_or("EMPTY_MODE", Mode::name)
    }

    /// The tiered flat buffer's `lastMode`.
    pub fn front_mode_name(&self) -> &'static str {
        self.front_mode.get().map_or("EMPTY_MODE", Mode::name)
    }

    fn prefer_adhoc_tree(&self, subset: usize, k: usize) -> bool {
        let index_size = self.size();
        let subset = subset.min(index_size);
        let d = self.dim;
        let r = if index_size == 0 { 0.0f32 } else { subset as f32 / self.label_count() as f32 };
        if self.algo == Algo::Hnsw {
            let m = self.m;
            return if index_size <= 30000 {
                if index_size <= 5500 || r <= 0.17 {
                    true
                } else if k <= 12 {
                    if d <= 55 { false } else { m > 10 }
                } else {
                    true
                }
            } else if r < 0.07 {
                if index_size <= 750000 {
                    true
                } else if k <= 7 {
                    false
                } else {
                    r <= 0.03
                }
            } else if d <= 75 {
                false
            } else if k <= 12 {
                if r <= 0.21 {
                    if m <= 57 { index_size <= 75000 } else { true }
                } else {
                    false
                }
            } else if m <= 10 {
                r <= 0.17
            } else if index_size <= 300000 {
                true
            } else {
                r <= 0.17
            };
        }
        if index_size <= 5500 {
            true
        } else if d <= 300 {
            if r <= 0.15 {
                true
            } else if r <= 0.35 {
                if d <= 75 { false } else { index_size <= 550000 }
            } else {
                false
            }
        } else if r <= 0.55 {
            true
        } else if d <= 750 {
            false
        } else {
            r <= 0.75
        }
    }

    /// `newBatchIterator`.
    pub fn batches(&self, blob: &[u8], ef_runtime: usize) -> Batches {
        if self.hnsw.is_some() {
            return Batches::Tiered(Box::new(super::hnsw::TieredBatch::new(self.preprocess(blob), ef_runtime)));
        }
        Batches::Flat(FlatBatches { q: self.preprocess(blob), scores: Vec::new(), start: 0, returned: 0, label_count: self.label_count(), computed: false })
    }

}

/// A batch iterator: brute force, or the tiered HNSW index's.
#[derive(Clone, Debug)]
pub enum Batches {
    Flat(FlatBatches),
    Tiered(Box<super::hnsw::TieredBatch>),
}

impl Batches {
    /// `isDepleted`'s negation.
    pub fn has_next(&self) -> bool {
        match self {
            Batches::Flat(b) => b.has_next(),
            Batches::Tiered(b) => b.has_next(),
        }
    }

    /// `getNextResults(n, BY_ID)`.
    pub fn next(&mut self, ix: &VecIndex, n: usize) -> Vec<VecRes> {
        match self {
            Batches::Flat(b) => b.next(ix, n),
            Batches::Tiered(b) => match &ix.hnsw {
                Some(h) => b.next(h, n),
                None => Vec::new(),
            },
        }
    }
}

/// A brute force batch iterator (`BF_BatchIterator`).
#[derive(Clone, Debug)]
pub struct FlatBatches {
    q: Vec<u8>,
    scores: Vec<(f64, u64)>,
    start: usize,
    returned: usize,
    label_count: usize,
    computed: bool,
}

impl FlatBatches {
    /// `isDepleted`'s negation.
    pub fn has_next(&self) -> bool {
        self.returned != self.label_count
    }

    /// `getNextResults(n, BY_ID)`.
    pub fn next(&mut self, ix: &VecIndex, n: usize) -> Vec<VecRes> {
        if !self.computed {
            self.computed = true;
            self.label_count = ix.label_count();
            if ix.multi {
                let mut order: Vec<u64> = Vec::new();
                let mut best: HashMap<u64, f64> = HashMap::new();
                for (s, l) in ix.scan(&self.q) {
                    match best.get_mut(&l) {
                        None => {
                            order.push(l);
                            best.insert(l, s);
                        }
                        Some(b) => {
                            if *b > s {
                                *b = s;
                            }
                        }
                    }
                }
                self.scores = order.into_iter().map(|l| (best[&l], l)).collect();
            } else {
                self.scores = ix.scan(&self.q).collect();
            }
        }
        let remaining = self.scores.len() - self.start;
        let mut rep: Vec<VecRes> = if (self.label_count - self.returned) / 1000 > n {
            self.heap_based(n)
        } else {
            let n = n.min(remaining);
            let rest = &mut self.scores[self.start..];
            rest.sort_by_key(|a| (key(a.0), a.1));
            let out: Vec<VecRes> = rest[..n].iter().map(|&(s, l)| (l, s)).collect();
            self.start += n;
            out
        };
        self.returned += rep.len();
        rep.sort_by_key(|r| r.0);
        rep
    }

    /// `heapBasedSearch`: the scan keeps the best `n` (strictly better scores replace the
    /// worst), and the chosen ones move to the front.
    fn heap_based(&mut self, n: usize) -> Vec<VecRes> {
        let mut heap: BTreeSet<(Score, u64)> = BTreeSet::new();
        let mut pos: HashMap<u64, usize> = HashMap::new();
        let mut upper = f64::MIN;
        for i in self.start..self.scores.len() {
            let (s, l) = self.scores[i];
            if heap.len() >= n {
                if s < upper {
                    let top = *heap.iter().next_back().unwrap();
                    heap.remove(&top);
                    pos.remove(&top.1);
                } else {
                    continue;
                }
            }
            heap.insert((key(s), l));
            pos.insert(l, i);
            upper = heap.iter().next_back().unwrap().0.0;
        }
        let out: Vec<VecRes> = heap.iter().map(|&(s, l)| (l, s.0)).collect();
        // `swapScores`
        let chosen: BTreeSet<usize> = pos.values().copied().collect();
        let next_start = self.start + out.len();
        let mut reuse = chosen.range(next_start..);
        let mut it = chosen.iter().peekable();
        for ind in self.start..next_start {
            if it.peek() == Some(&&ind) {
                it.next();
            } else if let Some(&r) = reuse.next() {
                self.scores[r] = self.scores[ind];
            }
        }
        self.start = next_start;
        out
    }
}

/// libstdc++'s `__prime_list` from 17 on (bucket counts for `unordered_map`).
const PRIMES: &[u32] = &[
    17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 103, 109, 113, 127, 137, 139, 149, 157, 167,
    179, 193, 199, 211, 227, 241, 257, 277, 293, 313, 337, 359, 383, 409, 439, 467, 503, 541, 577, 619, 661, 709, 761, 823,
    887, 953, 1031, 1109, 1193, 1289, 1381, 1493, 1613, 1741, 1879, 2029, 2179, 2357, 2549, 2753, 2971, 3209, 3469, 3739,
    4027, 4349, 4703, 5087, 5503, 5953, 6427, 6949, 7517, 8123, 8783, 9497, 10273, 11113, 12011, 12983, 14033, 15173,
    16411, 17749, 19183, 20753, 22447, 24281, 26267, 28411, 30727, 33223, 35933, 38873, 42043, 45481, 49201, 53201, 57557,
    62233, 67307, 72817, 78779, 85229, 92203, 99733, 107897, 116731, 126271, 136607, 147793, 159871, 172933, 187091,
    202409, 218971, 236897, 256279, 277261, 299951, 324503, 351061, 379787, 410857, 444487, 480881, 520241, 562841,
    608903, 658753, 712697, 771049, 834181, 902483, 976369, 1056323, 1142821, 1236397, 1337629, 1447153, 1565659,
    1693859, 1832561, 1982627, 2144977, 2320627, 2510653, 2716249, 2938679, 3179303, 3439651, 3721303, 4026031, 4355707,
    4712381, 5098259, 5515729, 5967347, 6456007, 6984629, 7556579, 8175383, 8844859, 9569143, 10352717, 11200489,
    12117689, 13109983, 14183539, 15345007, 16601593, 17961079, 19431899, 21023161, 22744717, 24607243, 26622317,
    28802401, 31160981, 33712729, 36473443, 39460231, 42691603, 46187573, 49969847, 54061849, 58488943, 63278561,
    68460391, 74066549, 80131819, 86693767, 93793069, 101473717, 109783337, 118773397, 128499677, 139022417, 150406843,
    162723577, 176048909, 190465427, 206062531, 222936881, 241193053, 260944219, 282312799, 305431229, 330442829,
    357502601, 386778277, 418451333, 452718089, 489790921, 529899637, 573292817, 620239453, 671030513, 725980837,
    785430967, 849749479, 919334987, 994618837, 1076067617, 1164186217, 1259520799, 1362662261, 1474249943, 1594975441,
    1725587117, 1866894511, 2019773507, 2185171673, 2364114217, 2557710269, 2767159799, 2993761039, 3238918481,
    3504151727, 3791104843, 4101556399, 4294967291,
];

/// `_Prime_rehash_policy::_M_next_bkt`.
pub(super) fn next_bkt(n: usize) -> usize {
    const FAST: [usize; 14] = [2, 2, 2, 3, 5, 5, 7, 7, 11, 11, 11, 11, 13, 13];
    match n {
        0 => 1,
        1..14 => FAST[n],
        _ => PRIMES.iter().map(|&p| p as usize).find(|&p| p >= n).unwrap_or(n),
    }
}

/// An allocation the allocator counts: the size plus its 8-byte header.
pub(super) fn hdr(n: usize) -> usize {
    if n == 0 { 0 } else { n + 8 }
}

/// A bucket array's allocation (a single bucket lives in the table).
pub(super) fn bucket_bytes(b: usize) -> usize {
    if b <= 1 { 0 } else { hdr(b * 8) }
}

/// A multi-value label's lookup entry: the node (its id vector inline) and the ids' storage
/// (capacity doubling from 1).
fn multi_entry(ids: usize) -> usize {
    88 + hdr(ids.max(1).next_power_of_two() * 4)
}

impl VecIndex {
    /// `unordered_map::reserve(n)` with `elems` entries.
    fn rehash(&mut self, elems: usize, n: usize) {
        self.alloc.buckets = next_bkt((elems + 1).max(n));
    }

    /// A new vector's bookkeeping before it is stored under `label`: FLAT's `growByBlock`
    /// and a new data block; an HNSW element and its random level.
    fn grow(&mut self) {
        let count = self.count();
        let b = self.block_size;
        if count >= self.alloc.cap {
            self.alloc.cap += b;
            let (e, c) = (self.lookup.len(), self.alloc.cap);
            self.rehash(e, c);
        }
        if count.is_multiple_of(b) && count / b + 1 > self.alloc.blocks_cap {
            self.alloc.blocks_cap = (2 * self.alloc.blocks_cap).max(1);
        }
    }

    /// FLAT's `shrinkByBlock` after a vector left (its label still counted in a multi index).
    fn shrink(&mut self) {
        let (count, b) = (self.count(), self.block_size);
        if !count.is_multiple_of(b) {
            return;
        }
        let elems = if self.multi { self.lookup.len() } else { self.lookup.len().saturating_sub(1) };
        if count == 0 {
            self.alloc.cap = 0;
            self.rehash(elems, 0);
        } else if self.alloc.cap >= count + 2 * b {
            self.alloc.cap -= b;
            let c = self.alloc.cap;
            self.rehash(elems, c);
        }
    }

    /// FT.INFO's `flat_buffer_size`: an untrained SVS-VAMANA index keeps its vectors in the
    /// tiered flat buffer (HNSW's background ingestion empties its buffer).
    pub fn flat_buffer(&self) -> usize {
        if self.algo == Algo::Svs && !self.trained { self.count() } else { 0 }
    }

    /// FT.INFO's `memory` (`VecSimIndex_StatsInfo`): what the index's allocator counts. FLAT
    /// is exact: the index (488 bytes, a cosine preprocessor 64 more), the data blocks vector
    /// (48 per block), the blocks, `idToLabelMapping`, the lookup's buckets and entries. A
    /// tiered HNSW index adds its layer, its flat buffer and the graph (`Hnsw::memory`).
    /// SVS-VAMANA holds its vectors in the tiered flat buffer until trained.
    pub fn memory(&self) -> usize {
        if self.hnsw.is_some() {
            let (layer, buffer, graph) = self.tiered_parts();
            return layer + buffer + graph;
        }
        let b = self.block_size;
        let cos = if self.metric == Metric::Cosine { 64 } else { 0 };
        let a = &self.alloc;
        let blocks = |n: usize| n.div_ceil(b) * hdr(b * self.stored);
        let entries: usize = if self.multi { self.lookup.values().map(|v| multi_entry(v.len())).sum() } else { 32 * self.lookup.len() };
        let flat = 488 + cos + hdr(a.blocks_cap * 48) + blocks(self.count()) + hdr(a.cap * 8) + bucket_bytes(a.buckets) + entries;
        if self.algo == Algo::Svs { flat + 984 } else { flat }
    }
}

impl VecIndex {
    /// A tiered HNSW index's parts (`MANAGEMENT_LAYER_MEMORY`, the flat buffer's and the graph's
    /// `MEMORY`) once background ingestion settled: the buffer keeps a block's bookkeeping once
    /// a vector came through it (with workers).
    pub fn tiered_parts(&self) -> (usize, usize, usize) {
        let cos = if self.metric == Metric::Cosine { 64 } else { 0 };
        let Some(h) = &self.hnsw else { return (0, 0, self.memory()) };
        let buffer = 488 + cos + if h.flat_used { 80 } else { 0 };
        (h.layer_memory(), buffer, h.memory())
    }
}

/// `HNSWIndex::getRandomLevel`: `-ln(u) / ln(M)` truncated, `u` libstdc++'s
/// `generate_canonical<double, 53>` over two `minstd_rand0` draws (seeded with 100).
pub(super) fn hnsw_level(state: &mut u64, m: usize) -> usize {
    let mut draw = || {
        *state = *state * 16807 % 2147483647;
        (*state - 1) as f64
    };
    const R: f64 = 2147483646.0;
    let mut sum = draw();
    sum += draw() * R;
    let mut u = sum / (R * R);
    if u >= 1.0 {
        u = 1.0 - f64::EPSILON / 2.0;
    }
    let mult = 1.0 / (m as f64).ln();
    (-u.ln() * mult) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: f32) -> u16 {
        f64_to_f16(v as f64)
    }

    #[test]
    fn half_arithmetic() {
        assert_eq!(h(1.0), 0x3c00);
        assert_eq!(h(65504.0), 0x7bff);
        assert_eq!(h(65520.0), 0x7c00);
        assert_eq!(f16_to_f64(0x0001), 5.960464477539063e-8);
        assert_eq!(f16_add(h(1.0), h(2.0)), h(3.0));
        assert_eq!(f16_sub(h(1.0), h(1.0)), 0);
        // 1 + 2^-11 ties to even (1), 1 + 3*2^-11 to 1 + 2^-9
        assert_eq!(f16_add(F16_ONE, h(0.00048828125)), F16_ONE);
        assert_eq!(f16_fma(h(3.0), h(5.0), h(1.0)), h(16.0));
        assert_eq!(fp16_to_fp32(0x3555), 0.33325195);
        assert_eq!(fp32_to_fp16(0.33325195), 0x3555);
    }

    #[test]
    fn round_to_odd() {
        // 1 + 2^-30 rounds to odd: 1 + 2^-23
        assert_eq!(bf_add(1.0, 2f32.powi(-30)), 1.0 + 2f32.powi(-23));
        assert_eq!(bf_add(1.0, 1.0), 2.0);
        assert_eq!(bf_add(2f32.powi(-126), -2f32.powi(-126) * 1.5), -0.0);
    }

    #[test]
    fn allocator_view() {
        use super::super::hnsw::WriteMode::{Async, InPlace};
        assert_eq!((next_bkt(0), next_bkt(1), next_bkt(1024), next_bkt(2048)), (1, 2, 1031, 2179));
        let p = VectorParams { dim: 3, ..VectorParams::new(Algo::Flat, false) };
        let mut ix = VecIndex::new(&p);
        ix.add(&[0; 12], 1, InPlace);
        assert_eq!(ix.memory(), 29328);
        ix.add(&[0; 12], 2, InPlace);
        assert_eq!(ix.memory(), 29360);
        ix.delete(1, InPlace);
        ix.delete(2, InPlace);
        assert_eq!(ix.memory(), 568);
        // HNSW, M 2, through the flat buffer: the first element's level is 2
        let p = VectorParams { dim: 3, m: 2, ..VectorParams::new(Algo::Hnsw, false) };
        let mut ix = VecIndex::new(&p);
        ix.add(&[0; 12], 1, Async);
        assert_eq!(ix.memory(), 82560);
        assert_eq!(ix.entry_point(), Some((2, 1)));
        // in place, M 16, DIM 2: four elements on level 0
        let p = VectorParams { dim: 2, ..VectorParams::new(Algo::Hnsw, false) };
        let mut ix = VecIndex::new(&p);
        for (l, x) in [1.0f32, 2.0, 3.0, 4.0].iter().enumerate() {
            let mut b = x.to_le_bytes().to_vec();
            b.extend_from_slice(&2f32.to_le_bytes());
            ix.add(&b, l as u64 + 1, InPlace);
        }
        assert_eq!(ix.tiered_parts(), (600, 488, 194112));
    }

    #[test]
    fn index_deletes_swap() {
        use super::super::hnsw::WriteMode::InPlace;
        let p = VectorParams { dim: 1, ..VectorParams::new(Algo::Flat, false) };
        let mut ix = VecIndex::new(&p);
        for (i, v) in [1.0f32, 2.0, 3.0].iter().enumerate() {
            ix.add(&v.to_le_bytes(), i as u64 + 1, InPlace);
        }
        ix.delete(1, InPlace);
        assert_eq!(ix.labels, vec![3, 2]);
        assert_eq!(ix.top_k(&0f32.to_le_bytes(), 1, 0), vec![(2, 4.0)]);
    }
}
