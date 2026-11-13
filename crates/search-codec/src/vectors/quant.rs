//! Scalar quantization of float vectors (Elasticsearch's `int8_hnsw` / `int4_hnsw`).
//!
//! Components are clamped to a `[min, max]` interval taken from quantiles of the segment's
//! values (the confidence interval, `1 - 1/(dims + 1)` by default like Lucene) and mapped
//! linearly to `0..=levels` (127 for 8-bit, 15 for 4-bit; 7 usable bits keep `u8 × u8` sums
//! inside the 16-bit lanes of `pmaddwd`). With `alpha = (max - min) / levels`,
//!
//! ```text
//! x·y ≈ alpha² Σ qx qy + c(x) + c(y),   c(v) = alpha·min·Σ qv + dims·min² / 2
//! |x - y|² ≈ alpha² Σ (qx - qy)²
//! ```
//!
//! so each stored vector carries its correction `c(v)` and a quantized dot product is one
//! integer kernel plus two adds. Cosine fields quantize unit-normalized vectors. Quantized
//! scores steer the graph search; final hits are rescored with the raw floats.

use super::distance::{dot_u4, dot_u8, l2sq_u4, l2sq_u8};
use crate::info::VectorSimilarity;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScalarQuantizer {
    pub min: f32,
    pub max: f32,
    pub levels: u8,
    /// `alpha²`, cached: it scales every quantized product.
    alpha2: f32,
}

fn f32_at(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())
}

/// Copies a vector, normalizing it to unit length.
pub(crate) fn normalized(v: &[u8], out: &mut Vec<f32>) {
    out.clear();
    let n = v.len() / 4;
    let mut norm = 0f64;
    for i in 0..n {
        let x = f32_at(v, i) as f64;
        norm += x * x;
    }
    let inv = if norm > 0.0 { 1.0 / norm.sqrt() } else { 0.0 };
    for i in 0..n {
        out.push((f32_at(v, i) as f64 * inv) as f32);
    }
}

impl ScalarQuantizer {
    pub(crate) fn new(min: f32, max: f32, levels: u8) -> ScalarQuantizer {
        let mut q = ScalarQuantizer { min, max, levels, alpha2: 0.0 };
        let a = q.alpha();
        q.alpha2 = a * a;
        q
    }

    /// Learns the interval from up to 25 000 vectors (`count` vectors of `dims` floats in
    /// `data`).
    pub(crate) fn train(
        data: &[u8],
        dims: usize,
        count: usize,
        bits: u8,
        confidence_interval: Option<f32>,
        normalize: bool,
    ) -> ScalarQuantizer {
        let levels = if bits <= 4 { 15 } else { 127 };
        let ci = match confidence_interval {
            Some(c) if c > 0.0 && c <= 1.0 => c as f64,
            _ => 1.0 - 1.0 / (dims as f64 + 1.0),
        };
        let vb = dims * 4;
        let step = count.div_ceil(25_000).max(1);
        let mut vals: Vec<f32> = Vec::with_capacity((count / step + 1) * dims);
        let mut tmp = Vec::new();
        let mut i = 0;
        while i < count {
            let v = &data[i * vb..(i + 1) * vb];
            if normalize {
                normalized(v, &mut tmp);
                vals.extend_from_slice(&tmp);
            } else {
                for d in 0..dims {
                    vals.push(f32_at(v, d));
                }
            }
            i += step;
        }
        vals.retain(|v| v.is_finite());
        if vals.is_empty() {
            return ScalarQuantizer::new(0.0, 0.0, levels);
        }
        let lo_q = (1.0 - ci) / 2.0;
        let n = vals.len();
        let lo_i = ((n as f64 - 1.0) * lo_q).round() as usize;
        let hi_i = ((n as f64 - 1.0) * (1.0 - lo_q)).round() as usize;
        let (_, lo, _) = vals.select_nth_unstable_by(lo_i, |a, b| a.total_cmp(b));
        let lo = *lo;
        let (_, hi, _) = vals.select_nth_unstable_by(hi_i, |a, b| a.total_cmp(b));
        let hi = *hi;
        ScalarQuantizer::new(lo, hi.max(lo), levels)
    }

    fn alpha(&self) -> f32 {
        if self.max > self.min {
            (self.max - self.min) / self.levels as f32
        } else {
            1.0
        }
    }

    /// Whether codes are 4-bit, packed two per byte: byte `j` holds code `j` in its low nibble
    /// and code `j + ceil(dims / 2)` in its high nibble, so a dot product of two packed vectors
    /// is `Σ lo·lo + hi·hi` without unpacking either.
    #[inline]
    pub(crate) fn packed(&self) -> bool {
        self.levels <= 15
    }

    /// Bytes of codes per vector.
    pub(crate) fn code_len(&self, dims: usize) -> usize {
        if self.packed() { dims.div_ceil(2) } else { dims }
    }

    /// Quantizes `v` (floats) into `out` (`code_len(v.len())` bytes); returns the correction.
    pub(crate) fn quantize(&self, v: &[f32], out: &mut [u8]) -> f32 {
        let alpha = self.alpha();
        let inv = 1.0 / alpha;
        let mut sum = 0u32;
        let packed = self.packed();
        let half = v.len().div_ceil(2);
        if packed {
            out.fill(0);
        }
        for (j, &x) in v.iter().enumerate() {
            // max/min rather than `clamp`, which panics when a corrupt section has min > max.
            let c = (x.max(self.min).min(self.max) - self.min) * inv;
            let q = c.round().clamp(0.0, self.levels as f32) as u8;
            if !packed {
                out[j] = q;
            } else if j < half {
                out[j] |= q;
            } else {
                out[j - half] |= q << 4;
            }
            sum += q as u32;
        }
        alpha * self.min * sum as f32 + v.len() as f32 * self.min * self.min / 2.0
    }

    /// Approximate raw similarity (dot product, or squared distance for L2) of two quantized
    /// vectors with their corrections.
    #[inline]
    pub(crate) fn raw(&self, sim: VectorSimilarity, a: &[u8], ca: f32, b: &[u8], cb: f32) -> f32 {
        let a2 = self.alpha2;
        match (sim, self.packed()) {
            (VectorSimilarity::L2Norm, false) => a2 * l2sq_u8(a, b) as f32,
            (VectorSimilarity::L2Norm, true) => a2 * l2sq_u4(a, b) as f32,
            (_, false) => a2 * dot_u8(a, b) as f32 + ca + cb,
            (_, true) => a2 * dot_u4(a, b) as f32 + ca + cb,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    fn packed_int4_matches_unpacked_codes() {
        let mut rng = StdRng::seed_from_u64(2);
        for dims in [1usize, 2, 7, 8, 33, 128] {
            let n = 300;
            let data: Vec<f32> = (0..n * dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
            let bytes: Vec<u8> = data.iter().flat_map(|x| x.to_le_bytes()).collect();
            let q = ScalarQuantizer::train(&bytes, dims, n, 4, None, false);
            assert!(q.packed());
            let cl = q.code_len(dims);
            assert_eq!(cl, dims.div_ceil(2));
            let unpacked = ScalarQuantizer::new(q.min, q.max, 16); // byte codes
            let alpha = q.alpha();
            for i in 0..20 {
                let (a, b) = (&data[i * dims..(i + 1) * dims], &data[(i + 1) * dims..(i + 2) * dims]);
                let (mut pa, mut pb) = (vec![0u8; cl], vec![0u8; cl]);
                let ca = q.quantize(a, &mut pa);
                let cb = q.quantize(b, &mut pb);
                // Reference: one code per byte with the same rounding.
                let codes = |v: &[f32]| -> Vec<u8> {
                    v.iter().map(|&x| ((x.max(q.min).min(q.max) - q.min) / alpha).round().clamp(0.0, 15.0) as u8).collect()
                };
                let (ua, ub) = (codes(a), codes(b));
                let dot: i32 = ua.iter().zip(&ub).map(|(&x, &y)| x as i32 * y as i32).sum();
                let l2: i32 = ua.iter().zip(&ub).map(|(&x, &y)| (x as i32 - y as i32).pow(2)).sum();
                assert_eq!(dot_u4(&pa, &pb), dot);
                assert_eq!(l2sq_u4(&pa, &pb), l2);
                let want = alpha * alpha * dot as f32 + ca + cb;
                assert_eq!(q.raw(VectorSimilarity::DotProduct, &pa, ca, &pb, cb), want);
                assert!(!unpacked.packed());
            }
        }
    }

    #[test]
    fn quantized_dot_approximates_float_dot() {
        let mut rng = StdRng::seed_from_u64(1);
        let dims = 64;
        let n = 500;
        let data: Vec<f32> = (0..n * dims).map(|_| rng.gen_range(-1.0..1.0)).collect();
        let bytes: Vec<u8> = data.iter().flat_map(|x| x.to_le_bytes()).collect();
        let q = ScalarQuantizer::train(&bytes, dims, n, 8, None, false);
        assert!(q.min < -0.9 && q.max > 0.9);
        let mut qa = vec![0u8; dims];
        let mut qb = vec![0u8; dims];
        let mut max_err = 0f32;
        for i in 0..50 {
            let a = &data[i * dims..(i + 1) * dims];
            let b = &data[(i + 1) * dims..(i + 2) * dims];
            let ca = q.quantize(a, &mut qa);
            let cb = q.quantize(b, &mut qb);
            let exact: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let approx = q.raw(VectorSimilarity::DotProduct, &qa, ca, &qb, cb);
            max_err = max_err.max((exact - approx).abs());
            let el2: f32 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
            let al2 = q.raw(VectorSimilarity::L2Norm, &qa, ca, &qb, cb);
            assert!((el2 - al2).abs() < 0.05 * el2.max(1.0), "{el2} vs {al2}");
        }
        assert!(max_err < 0.15, "max error {max_err}");
    }
}
