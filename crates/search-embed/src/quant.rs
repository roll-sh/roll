//! Dynamically quantized linear layers, as PyTorch's `quantized::linear_dynamic` runs them on
//! x86 with fbgemm. Elastic's `.multilingual-e5-small_linux-x86_64` model, which Elasticsearch
//! deploys on Linux x86-64 ML nodes, is the model with most linear layers quantized so (which
//! ones, and how, is [`crate::model`]'s table), so reproducing them reproduces Elasticsearch's
//! embeddings there:
//!
//! - weights: symmetric `qint8`, per tensor (`MinMaxObserver`) or per output row
//!   (`PerChannelMinMaxObserver`): scale `max|w| / 127.5`, zero point 0, `w * (1 / scale)`
//!   rounded half to even and clamped to `[-128, 127]` (checked against Elastic's published
//!   build: the same scales, and the same 8-bit weights);
//! - activations, per call: per-tensor `quint8` over the whole input with fbgemm's reduced
//!   range `[0, 127]` (`ChooseQuantizationParams(min, max, 0, 255, reduce_range = true)`), the
//!   zero point nudged as PyTorch nudges it, then fbgemm's legacy `Quantize`: `x · (1 / scale) +
//!   zero` in one fused multiply-add, rounded half to even;
//! - the products summed exactly in `i32`, then `(acc - x_zero · Σw) · (x_scale · w_scale) +
//!   bias` in `f32` (`ReQuantizeForFloat`).
//!
//! The integer sums are exact whatever the instructions, so the AVX-512 VNNI, AVX2 and portable
//! kernels give the same results. The reduced range keeps AVX2's 16-bit pair sums from
//! saturating (127 × 128 × 2 < 2¹⁵), as it does for fbgemm.

use crate::ops::round_even;

/// Outputs per packed block, and bytes of the reduction per output in a block.
const BLOCK: usize = 16;
const GROUP: usize = 4;

/// A quantized linear layer. The weights are packed for the kernels: blocks of [`BLOCK`] output
/// rows, and in each block, for every [`GROUP`] bytes of the input, those bytes of each row
/// (`[out / 16][k / 4][16][4]`), so one 64-byte load feeds 16 outputs' sums.
pub struct QLinear {
    packed: Vec<i8>,
    k: usize,
    /// `k` rounded up to a multiple of [`GROUP`].
    kp: usize,
    out: usize,
    /// `out` rounded up to a multiple of [`BLOCK`].
    outp: usize,
    /// Per output row (all equal per tensor).
    w_scale: Vec<f32>,
    /// `Σ w` per output row (`col_offsets`).
    sums: Vec<i32>,
    bias: Vec<f32>,
}

/// Where weight `(o, i)` sits in the packed layout.
fn at(kp: usize, o: usize, i: usize) -> usize {
    ((o / BLOCK * (kp / GROUP) + i / GROUP) * BLOCK + o % BLOCK) * GROUP + i % GROUP
}

/// PyTorch's `ChooseQuantizationParams` for `quint8` with `reduce_range` (`[0, 127]`).
pub fn activation_params(min: f32, max: f32) -> (f32, i32) {
    let (qmin, qmax) = (0i32, 127i32);
    let min = min.min(0.0);
    let max = max.max(0.0);
    let mut scale = (f64::from(max) - f64::from(min)) / f64::from(qmax - qmin);
    if scale as f32 == 0.0 || (1.0 / scale as f32).is_infinite() {
        scale = 0.1;
    }
    // `SMALL_SCALE_THRESHOLD`: a smaller scale widens the range around zero.
    const SMALL: f64 = 6.1e-5;
    let (mut min, mut max) = (f64::from(min), f64::from(max));
    if scale < SMALL {
        let org = scale;
        scale = SMALL;
        if min == 0.0 {
            max = SMALL * f64::from(qmax - qmin);
        } else if max == 0.0 {
            min = -SMALL * f64::from(qmax - qmin);
        } else {
            let amplifier = SMALL / org;
            min *= amplifier;
            max *= amplifier;
        }
    }
    let scale = scale as f32;
    let s = f64::from(scale);
    let from_min = f64::from(qmin) - min / s;
    let from_max = f64::from(qmax) - max / s;
    let from_min_error = f64::from(qmin).abs() - (min / s).abs();
    let from_max_error = f64::from(qmax).abs() - (max / s).abs();
    let initial = if from_min_error < from_max_error { from_min } else { from_max };
    let zero = if initial < f64::from(qmin) {
        qmin
    } else if initial > f64::from(qmax) {
        qmax
    } else {
        initial.round_ties_even() as i32
    };
    (scale, zero)
}

impl QLinear {
    /// `weight`: `[out, k]` row-major (`nn.Linear.weight`); `per_channel`: a scale per output
    /// row instead of one for the tensor.
    pub fn new(weight: &[f32], bias: Vec<f32>, out: usize, k: usize, per_channel: bool) -> QLinear {
        assert_eq!(weight.len(), out * k);
        // `MinMaxObserver`: `max(-min_neg, max_pos) / ((127 - -128) / 2)`, at least `eps`.
        let scale = |ws: &[f32]| (ws.iter().fold(0f32, |m, w| m.max(w.abs())) / 127.5).max(f32::EPSILON);
        let tensor = scale(weight);
        let w_scale: Vec<f32> = (0..out).map(|o| if per_channel { scale(&weight[o * k..(o + 1) * k]) } else { tensor }).collect();
        let kp = k.div_ceil(GROUP) * GROUP;
        let outp = out.div_ceil(BLOCK) * BLOCK;
        let mut packed = vec![0i8; outp * kp];
        let mut sums = vec![0i32; out];
        for o in 0..out {
            let inv = 1.0 / w_scale[o];
            for i in 0..k {
                let q = round_even(weight[o * k + i] * inv).clamp(-128.0, 127.0) as i8;
                packed[at(kp, o, i)] = q;
                sums[o] += i32::from(q);
            }
        }
        QLinear { packed, k, kp, out, outp, w_scale, sums, bias }
    }

    /// The quantized weight `(o, i)`.
    pub fn weight(&self, o: usize, i: usize) -> i8 {
        self.packed[at(self.kp, o, i)]
    }

    pub fn bytes(&self) -> u64 {
        (self.packed.len() + 4 * self.sums.len() + 4 * self.bias.len() + 4 * self.w_scale.len()) as u64
    }

    pub fn out_features(&self) -> usize {
        self.out
    }

    /// `x`: `[rows, k]` row-major; `[rows, out]`.
    pub fn forward(&self, x: &[f32], rows: usize) -> Vec<f32> {
        assert_eq!(x.len(), rows * self.k);
        let (min, max) = if rows == 0 { (0.0, 0.0) } else { crate::ops::min_max(x) };
        let (x_scale, x_zero) = activation_params(min, max);
        let inv = 1.0 / x_scale;
        let mut xq = vec![0u8; rows * self.kp];
        for (dst, src) in xq.chunks_exact_mut(self.kp).zip(x.chunks_exact(self.k)) {
            crate::ops::quantize(src, dst, inv, x_zero as f32);
        }
        let mut acc = vec![0i32; rows * self.outp];
        dot(&xq, &self.packed, rows, self.outp, self.kp, &mut acc);
        let mut y = vec![0f32; rows * self.out];
        for (dst, a) in y.chunks_exact_mut(self.out).zip(acc.chunks_exact(self.outp)) {
            for o in 0..self.out {
                let raw = a[o] - x_zero * self.sums[o];
                dst[o] = raw as f32 * (x_scale * self.w_scale[o]) + self.bias[o];
            }
        }
        y
    }
}

/// `acc[r][o] = Σ_i x[r][i] · w[o][i]` over the packed weights; `x` rows of `kp` bytes, `acc`
/// rows of `outp`.
fn dot(x: &[u8], w: &[i8], rows: usize, outp: usize, kp: usize, acc: &mut [i32]) {
    assert!(x.len() >= rows * kp && w.len() >= outp * kp && acc.len() >= rows * outp && kp.is_multiple_of(GROUP) && outp.is_multiple_of(BLOCK));
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx512vnni") && std::arch::is_x86_feature_detected!("avx512f") {
            // SAFETY: the features are present, and the sizes were checked above.
            unsafe { x86::dot_vnni(x, w, rows, outp, kp, acc) };
            return;
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: as above.
            unsafe { x86::dot_avx2(x, w, rows, outp, kp, acc) };
            return;
        }
    }
    let groups = kp / GROUP;
    for r in 0..rows {
        let xr = &x[r * kp..(r + 1) * kp];
        for b in 0..outp / BLOCK {
            let mut s = [0i32; BLOCK];
            for g in 0..groups {
                let wg = &w[(b * groups + g) * BLOCK * GROUP..][..BLOCK * GROUP];
                for (j, sj) in s.iter_mut().enumerate() {
                    for t in 0..GROUP {
                        *sj += i32::from(xr[g * GROUP + t]) * i32::from(wg[j * GROUP + t]);
                    }
                }
            }
            acc[r * outp + b * BLOCK..][..BLOCK].copy_from_slice(&s);
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{BLOCK, GROUP};
    use std::arch::x86_64::*;

    /// The four input bytes of group `g` of row `r`, as one 32-bit lane value.
    #[inline(always)]
    fn group(x: &[u8], kp: usize, r: usize, g: usize) -> i32 {
        let o = r * kp + g * GROUP;
        let b: [u8; 4] = x[o..o + GROUP].try_into().unwrap_or([0; 4]);
        i32::from_le_bytes(b)
    }

    /// `R` rows against `B` blocks of 16 outputs: VNNI's `vpdpbusd` multiplies the four bytes
    /// of a row's group (broadcast) by the 16 outputs' four bytes and adds them into 16 sums.
    #[target_feature(enable = "avx512f,avx512vnni")]
    unsafe fn block_vnni<const R: usize, const B: usize>(x: &[u8], w: &[i8], r0: usize, b0: usize, outp: usize, kp: usize, acc: &mut [i32]) {
        let groups = kp / GROUP;
        let mut s = [[_mm512_setzero_si512(); B]; R];
        for g in 0..groups {
            let mut wv = [_mm512_setzero_si512(); B];
            for (j, v) in wv.iter_mut().enumerate() {
                let o = ((b0 + j) * groups + g) * BLOCK * GROUP;
                assert!(o + 64 <= w.len());
                // SAFETY: 64 bytes from `o`, within `w` (checked).
                *v = unsafe { _mm512_loadu_si512(w.as_ptr().add(o).cast()) };
            }
            for (i, si) in s.iter_mut().enumerate() {
                let a = _mm512_set1_epi32(group(x, kp, r0 + i, g));
                for (j, sij) in si.iter_mut().enumerate() {
                    *sij = _mm512_dpbusd_epi32(*sij, a, wv[j]);
                }
            }
        }
        for (i, si) in s.iter().enumerate() {
            for (j, sij) in si.iter().enumerate() {
                let o = (r0 + i) * outp + (b0 + j) * BLOCK;
                assert!(o + BLOCK <= acc.len());
                // SAFETY: 16 sums from `o`, within `acc` (checked).
                unsafe { _mm512_storeu_si512(acc.as_mut_ptr().add(o).cast(), *sij) };
            }
        }
    }

    #[target_feature(enable = "avx512f,avx512vnni")]
    pub unsafe fn dot_vnni(x: &[u8], w: &[i8], rows: usize, outp: usize, kp: usize, acc: &mut [i32]) {
        let blocks = outp / BLOCK;
        let mut r = 0;
        while r + 4 <= rows {
            let mut b = 0;
            while b + 2 <= blocks {
                // SAFETY: same features; indices within `rows` and `blocks`.
                unsafe { block_vnni::<4, 2>(x, w, r, b, outp, kp, acc) };
                b += 2;
            }
            if b < blocks {
                // SAFETY: as above.
                unsafe { block_vnni::<4, 1>(x, w, r, b, outp, kp, acc) };
            }
            r += 4;
        }
        for r in r..rows {
            for b in 0..blocks {
                // SAFETY: as above.
                unsafe { block_vnni::<1, 1>(x, w, r, b, outp, kp, acc) };
            }
        }
    }

    /// The same with AVX2, eight outputs per register: `vpmaddubsw` multiplies the broadcast
    /// bytes by the weights' and adds pairs into 16 bits (no saturation: the activations stay
    /// within `[0, 127]`), `vpmaddwd` adds the pairs into each output's 32-bit sum.
    #[target_feature(enable = "avx2")]
    unsafe fn block_avx2<const R: usize>(x: &[u8], w: &[i8], r0: usize, b: usize, outp: usize, kp: usize, acc: &mut [i32]) {
        let groups = kp / GROUP;
        let ones = _mm256_set1_epi16(1);
        let mut s = [[_mm256_setzero_si256(); 2]; R];
        for g in 0..groups {
            let o = (b * groups + g) * BLOCK * GROUP;
            assert!(o + 64 <= w.len());
            // SAFETY: 64 bytes from `o`, within `w` (checked).
            let (lo, hi) = unsafe { (_mm256_loadu_si256(w.as_ptr().add(o).cast()), _mm256_loadu_si256(w.as_ptr().add(o + 32).cast())) };
            for (i, si) in s.iter_mut().enumerate() {
                let a = _mm256_set1_epi32(group(x, kp, r0 + i, g));
                si[0] = _mm256_add_epi32(si[0], _mm256_madd_epi16(_mm256_maddubs_epi16(a, lo), ones));
                si[1] = _mm256_add_epi32(si[1], _mm256_madd_epi16(_mm256_maddubs_epi16(a, hi), ones));
            }
        }
        for (i, si) in s.iter().enumerate() {
            let o = (r0 + i) * outp + b * BLOCK;
            assert!(o + BLOCK <= acc.len());
            // SAFETY: 16 sums from `o`, within `acc` (checked).
            unsafe {
                _mm256_storeu_si256(acc.as_mut_ptr().add(o).cast(), si[0]);
                _mm256_storeu_si256(acc.as_mut_ptr().add(o + 8).cast(), si[1]);
            }
        }
    }

    #[target_feature(enable = "avx2")]
    pub unsafe fn dot_avx2(x: &[u8], w: &[i8], rows: usize, outp: usize, kp: usize, acc: &mut [i32]) {
        let blocks = outp / BLOCK;
        let mut r = 0;
        while r + 4 <= rows {
            for b in 0..blocks {
                // SAFETY: same feature; indices within `rows` and `blocks`.
                unsafe { block_avx2::<4>(x, w, r, b, outp, kp, acc) };
            }
            r += 4;
        }
        for r in r..rows {
            for b in 0..blocks {
                // SAFETY: as above.
                unsafe { block_avx2::<1>(x, w, r, b, outp, kp, acc) };
            }
        }
    }

    #[cfg(test)]
    mod tests {
        /// Both kernels on the same data, against the portable sums.
        #[test]
        fn kernels_agree() {
            let (rows, out, k) = (7, 40, 52);
            let w: Vec<f32> = (0..out * k).map(|i| ((i * 53 % 97) as f32 - 48.0) / 31.0).collect();
            let l = super::super::QLinear::new(&w, vec![0.0; out], out, k, false);
            let xq: Vec<u8> = (0..rows * l.kp).map(|i| (i * 31 % 128) as u8).collect();
            let mut want = vec![0i32; rows * l.outp];
            for r in 0..rows {
                for o in 0..out {
                    want[r * l.outp + o] = (0..k).map(|i| i32::from(xq[r * l.kp + i]) * i32::from(l.weight(o, i))).sum();
                }
            }
            if std::arch::is_x86_feature_detected!("avx2") {
                let mut acc = vec![0i32; rows * l.outp];
                // SAFETY: AVX2 is present; sizes as `dot` checks them.
                unsafe { super::dot_avx2(&xq, &l.packed, rows, l.outp, l.kp, &mut acc) };
                assert_eq!(acc, want, "AVX2");
            }
            if std::arch::is_x86_feature_detected!("avx512vnni") && std::arch::is_x86_feature_detected!("avx512f") {
                let mut acc = vec![0i32; rows * l.outp];
                // SAFETY: as above.
                unsafe { super::dot_vnni(&xq, &l.packed, rows, l.outp, l.kp, &mut acc) };
                assert_eq!(acc, want, "VNNI");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_parameters_follow_pytorch() {
        // [-1, 3] over [0, 127]: scale 4/127, zero point round(127 - 3/scale) = 32 (0.25·127).
        let (s, z) = activation_params(-1.0, 3.0);
        assert!((s - 4.0 / 127.0).abs() < 1e-9);
        assert_eq!(z, 32);
        // Positive only: the range is extended to 0.
        assert_eq!(activation_params(0.5, 2.0).1, 0);
        assert_eq!(activation_params(0.0, 0.0), (0.1, 0));
    }

    #[test]
    fn the_layer_is_exact_in_integers_and_close_to_the_float_product() {
        let (rows, out, k) = (5, 7, 100);
        let x: Vec<f32> = (0..rows * k).map(|i| ((i * 37 % 101) as f32 - 50.0) / 13.0).collect();
        let w: Vec<f32> = (0..out * k).map(|i| ((i * 53 % 97) as f32 - 48.0) / 31.0).collect();
        let l = QLinear::new(&w, vec![0.5; out], out, k, true);
        let y = l.forward(&x, rows);
        // The portable sums give the same answer as whichever kernel ran.
        let (x_scale, x_zero) = {
            let (lo, hi) = x.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
            activation_params(lo, hi)
        };
        let (mut err, mut norm) = (0f32, 0f32);
        for r in 0..rows {
            for o in 0..out {
                let mut acc = 0i32;
                for i in 0..k {
                    let t = (f64::from(x[r * k + i]) * f64::from(1.0 / x_scale) + f64::from(x_zero)) as f32;
                    let xq = t.round_ties_even().clamp(0.0, 255.0) as i32;
                    acc += xq * i32::from(l.weight(o, i));
                }
                let want = (acc - x_zero * l.sums[o]) as f32 * (x_scale * l.w_scale[o]) + 0.5;
                assert!((y[r * out + o] - want).abs() <= want.abs() * 1e-6 + 1e-6, "{r} {o}: {} vs {want}", y[r * out + o]);
                let exact: f32 = (0..k).map(|i| x[r * k + i] * w[o * k + i]).sum::<f32>() + 0.5;
                err += (y[r * out + o] - exact).powi(2);
                norm += exact * exact;
            }
        }
        // 7-bit activations and 8-bit weights: a few percent of the float product.
        assert!((err / norm).sqrt() < 0.05, "relative error {}", (err / norm).sqrt());
    }
}
