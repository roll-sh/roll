//! Binary quantization of float vectors (Elasticsearch's `bbq_hnsw` / `bbq_flat`).
//!
//! Vectors are centered on the segment's centroid `c` (of unit vectors for cosine fields), and
//! each residual `r = x - c` keeps one bit per dimension: `x̂ᵢ = l + bᵢ·Δ`, where `l` and
//! `l + Δ` are the two centers of a 1-D 2-means clustering of the residual's components (the
//! least-squares optimal one-bit reconstruction of that vector, refined from a split at the
//! mean). Queries are quantized asymmetrically, to 4 bits per dimension over their own
//! `[min, max]`, stored as 4 bit planes, so
//!
//! ```text
//! r_q·r_x ≈ d·l_q·l_x + l_q·Δ_x·Σb + l_x·Δ_q·ΣQ + Δ_q·Δ_x·Σₖ 2ᵏ·popcount(planeₖ ∧ b)
//! q·x = r_q·r_x + r_q·c + r_x·c + c·c          |q - x|² = |r_q|² + |r_x|² - 2·r_q·r_x
//! ```
//!
//! Each indexed vector stores its code (`⌈d/64⌉` words), `l`, `Δ`, `Σb` and one exact
//! correction (`r_x·c`, or `|r_x|²` for `l2_norm`), so an estimate is four AND+popcount passes
//! over `d/64` words and a few multiply-adds: 32× less memory than floats and no float math per
//! dimension. Estimates only steer the graph search; hits are rescored with the raw floats.

use crate::io::Out;
use crate::info::VectorSimilarity;

/// Bytes of the per-vector corrections after the code.
const TAIL: usize = 16;

#[inline]
fn f32_at(b: &[u8], i: usize) -> f32 {
    f32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap())
}

/// The segment-level parameters of a binary-quantized field.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BinaryQuantizer {
    pub dims: usize,
    pub sim: VectorSimilarity,
    pub centroid: Vec<f32>,
    pub c_dot_c: f32,
}

/// A query (or, while building the graph, an inserted vector) in 4-bit planes.
#[derive(Clone, Debug, Default)]
pub(crate) struct BinaryQuery {
    /// `planes[k * words + w]`: bit `k` of every dimension's 4-bit code.
    planes: Vec<u64>,
    lower: f32,
    step: f32,
    sum: f32,
    /// `r_q·c` (dot-like similarities) or `|r_q|²` (l2).
    extra: f32,
}

impl BinaryQuantizer {
    /// Words of code per vector.
    #[inline]
    pub(crate) fn words(&self) -> usize {
        self.dims.div_ceil(64)
    }

    /// Bytes per indexed vector record.
    #[inline]
    pub(crate) fn stride(&self) -> usize {
        self.words() * 8 + TAIL
    }

    /// The centroid of `vectors` (each `dims` floats, already normalized for cosine).
    pub(crate) fn train(vectors: &mut dyn Iterator<Item = Vec<f32>>, dims: usize, sim: VectorSimilarity) -> Self {
        let mut sum = vec![0f64; dims];
        let mut n = 0usize;
        for v in vectors {
            for (s, x) in sum.iter_mut().zip(&v) {
                *s += *x as f64;
            }
            n += 1;
        }
        let centroid: Vec<f32> = sum.iter().map(|s| if n == 0 { 0.0 } else { (s / n as f64) as f32 }).collect();
        let c_dot_c = centroid.iter().map(|x| x * x).sum();
        BinaryQuantizer { dims, sim, centroid, c_dot_c }
    }

    fn residual(&self, v: &[f32], out: &mut Vec<f32>) {
        out.clear();
        out.extend(v.iter().zip(&self.centroid).map(|(x, c)| x - c));
    }

    fn extra(&self, r: &[f32]) -> f32 {
        match self.sim {
            VectorSimilarity::L2Norm => r.iter().map(|x| x * x).sum(),
            _ => r.iter().zip(&self.centroid).map(|(x, c)| x * c).sum(),
        }
    }

    /// Appends the record of vector `v` (`dims` floats) to `out`; `scratch` is reused.
    pub(crate) fn encode(&self, v: &[f32], scratch: &mut Vec<f32>, out: &mut Vec<u8>) {
        self.residual(v, scratch);
        let r = &scratch[..];
        // 1-D 2-means: start from a split at the mean, then alternate assignment and centers.
        let d = r.len().max(1) as f32;
        let mean = r.iter().sum::<f32>() / d;
        let (mut lo, mut hi) = (mean, mean);
        let mut threshold = mean;
        for _ in 0..6 {
            let (mut s0, mut n0, mut s1, mut n1) = (0f32, 0u32, 0f32, 0u32);
            for &x in r {
                if x > threshold {
                    s1 += x;
                    n1 += 1;
                } else {
                    s0 += x;
                    n0 += 1;
                }
            }
            lo = if n0 > 0 { s0 / n0 as f32 } else { mean };
            hi = if n1 > 0 { s1 / n1 as f32 } else { mean };
            let t = (lo + hi) / 2.0;
            if t == threshold {
                break;
            }
            threshold = t;
        }
        let mut ones = 0u32;
        for chunk in r.chunks(64) {
            let mut word = 0u64;
            for (bit, &x) in chunk.iter().enumerate() {
                if x > threshold {
                    word |= 1 << bit;
                }
            }
            ones += word.count_ones();
            out.put_u64(word);
        }
        for _ in r.len().div_ceil(64)..self.words() {
            out.put_u64(0);
        }
        out.put_f32(lo);
        out.put_f32(hi - lo);
        out.put_u32(ones);
        out.put_f32(self.extra(r));
    }

    /// Quantizes a query vector (`dims` floats, normalized for cosine) to 4-bit planes.
    pub(crate) fn query(&self, v: &[f32], scratch: &mut Vec<f32>, out: &mut BinaryQuery) {
        self.residual(v, scratch);
        let r = &scratch[..];
        let words = self.words();
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for &x in r {
            lo = lo.min(x);
            hi = hi.max(x);
        }
        if !lo.is_finite() || !hi.is_finite() || hi <= lo {
            lo = if lo.is_finite() { lo } else { 0.0 };
            hi = lo;
        }
        let step = if hi > lo { (hi - lo) / 15.0 } else { 0.0 };
        let inv = if step > 0.0 { 1.0 / step } else { 0.0 };
        out.planes.clear();
        out.planes.resize(4 * words, 0);
        let mut sum = 0u32;
        for (i, &x) in r.iter().enumerate() {
            let q = ((x - lo) * inv).round().clamp(0.0, 15.0) as u32;
            sum += q;
            for k in 0..4 {
                if q >> k & 1 != 0 {
                    out.planes[k * words + i / 64] |= 1 << (i % 64);
                }
            }
        }
        out.lower = lo;
        out.step = step;
        out.sum = sum as f32;
        out.extra = self.extra(r);
    }

    /// Estimated similarity of a query and an indexed record; higher is better (for `l2_norm`
    /// the negated squared distance).
    #[inline]
    pub(crate) fn estimate(&self, q: &BinaryQuery, record: &[u8]) -> f32 {
        let words = self.words();
        let qb = plane_dots(&q.planes, &record[..words * 8], words) as f32;
        let tail = &record[words * 8..];
        let lx = f32_at(tail, 0);
        let dx = f32_at(tail, 1);
        let ones = u32::from_le_bytes(tail[8..12].try_into().unwrap()) as f32;
        let ex = f32_at(tail, 3);
        let d = self.dims as f32;
        let rr = d * q.lower * lx + q.lower * dx * ones + lx * q.step * q.sum + q.step * dx * qb;
        match self.sim {
            VectorSimilarity::L2Norm => -(q.extra + ex - 2.0 * rr),
            _ => rr + q.extra + ex + self.c_dot_c,
        }
    }

    /// Serializes the quantizer (the section header).
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        out.put_u32(self.dims as u32);
        for &c in &self.centroid {
            out.put_f32(c);
        }
        out.put_f32(self.c_dot_c);
    }

    /// Reads a quantizer written by [`write`](Self::write); returns it and the header length.
    pub(crate) fn read(b: &[u8], dims: usize, sim: VectorSimilarity) -> Option<(BinaryQuantizer, usize)> {
        let mut r = crate::io::In::new(b);
        if r.u32().ok()? as usize != dims {
            return None;
        }
        let mut centroid = Vec::with_capacity(dims.min(b.len() / 4));
        for _ in 0..dims {
            centroid.push(r.f32().ok()?);
        }
        let c_dot_c = r.f32().ok()?;
        Some((BinaryQuantizer { dims, sim, centroid, c_dot_c }, r.pos()))
    }
}

/// `Σₖ 2ᵏ·popcount(planeₖ ∧ code)`: the dot product of a 4-bit query and a one-bit code.
#[inline]
fn plane_dots(planes: &[u64], code: &[u8], words: usize) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("popcnt") {
            // SAFETY: POPCNT was detected.
            return unsafe { plane_dots_popcnt(planes, code, words) };
        }
    }
    plane_dots_portable(planes, code, words)
}

/// The same loop compiled with the `popcnt` instruction (baseline x86-64 lacks it, and
/// `count_ones` would otherwise expand to a dozen bit operations).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "popcnt")]
unsafe fn plane_dots_popcnt(planes: &[u64], code: &[u8], words: usize) -> u32 {
    plane_dots_portable(planes, code, words)
}

#[inline(always)]
fn plane_dots_portable(planes: &[u64], code: &[u8], words: usize) -> u32 {
    let mut dots = [0u32; 4];
    for (w, c) in code.chunks_exact(8).take(words).enumerate() {
        let b = u64::from_le_bytes(c.try_into().unwrap());
        for (k, d) in dots.iter_mut().enumerate() {
            *d += (planes[k * words + w] & b).count_ones();
        }
    }
    dots[0] + 2 * dots[1] + 4 * dots[2] + 8 * dots[3]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    #[test]
    fn estimates_track_exact_scores() {
        let mut rng = StdRng::seed_from_u64(3);
        for dims in [1usize, 63, 64, 65, 384] {
            for sim in [VectorSimilarity::DotProduct, VectorSimilarity::L2Norm] {
                let n = 400;
                let data: Vec<Vec<f32>> =
                    (0..n).map(|_| (0..dims).map(|_| rng.gen_range(-1.0..1.0f32) + 0.3).collect()).collect();
                let q = BinaryQuantizer::train(&mut data.iter().cloned(), dims, sim);
                let mut recs = Vec::new();
                let mut scratch = Vec::new();
                for v in &data {
                    q.encode(v, &mut scratch, &mut recs);
                }
                assert_eq!(recs.len(), n * q.stride());
                let query: Vec<f32> = (0..dims).map(|_| rng.gen_range(-1.0..1.0f32) + 0.3).collect();
                let mut bq = BinaryQuery::default();
                q.query(&query, &mut scratch, &mut bq);
                let exact = |v: &[f32]| -> f32 {
                    match sim {
                        VectorSimilarity::L2Norm => -v.iter().zip(&query).map(|(a, b)| (a - b) * (a - b)).sum::<f32>(),
                        _ => v.iter().zip(&query).map(|(a, b)| a * b).sum(),
                    }
                };
                let est: Vec<f32> = (0..n).map(|i| q.estimate(&bq, &recs[i * q.stride()..(i + 1) * q.stride()])).collect();
                let ex: Vec<f32> = data.iter().map(|v| exact(v)).collect();
                // Rank correlation: the estimated top 10% should contain most of the exact top 2%.
                let top = |s: &[f32], k: usize| {
                    let mut idx: Vec<usize> = (0..n).collect();
                    idx.sort_by(|&a, &b| s[b].total_cmp(&s[a]));
                    idx.truncate(k);
                    idx
                };
                if dims >= 64 {
                    let want = top(&ex, n / 50);
                    let got: std::collections::HashSet<usize> = top(&est, n / 10).into_iter().collect();
                    let hit = want.iter().filter(|i| got.contains(i)).count();
                    assert!(hit * 10 >= want.len() * 7, "dims {dims} {sim:?}: {hit}/{}", want.len());
                }
                // Scale sanity: the estimate is in the ballpark of the exact value.
                let mean_abs_err: f32 = est.iter().zip(&ex).map(|(a, b)| (a - b).abs()).sum::<f32>() / n as f32;
                let mean_abs: f32 = ex.iter().map(|x| x.abs()).sum::<f32>() / n as f32;
                assert!(mean_abs_err <= mean_abs.max(1.0), "dims {dims} {sim:?}: err {mean_abs_err} vs {mean_abs}");
            }
        }
    }

    #[test]
    fn popcnt_matches_portable() {
        let mut rng = StdRng::seed_from_u64(4);
        for words in [1usize, 2, 6, 13] {
            let planes: Vec<u64> = (0..4 * words).map(|_| rng.r#gen()).collect();
            let code: Vec<u8> = (0..8 * words).map(|_| rng.r#gen()).collect();
            assert_eq!(plane_dots(&planes, &code, words), plane_dots_portable(&planes, &code, words));
        }
    }

    #[test]
    fn header_round_trips() {
        let q = BinaryQuantizer { dims: 3, sim: VectorSimilarity::Cosine, centroid: vec![0.5, -1.0, 2.0], c_dot_c: 5.25 };
        let mut b = Vec::new();
        q.write(&mut b);
        let (r, len) = BinaryQuantizer::read(&b, 3, VectorSimilarity::Cosine).unwrap();
        assert_eq!(r, q);
        assert_eq!(len, b.len());
        assert!(BinaryQuantizer::read(&b, 4, VectorSimilarity::Cosine).is_none());
        assert!(BinaryQuantizer::read(&b[..5], 3, VectorSimilarity::Cosine).is_none());
    }
}
