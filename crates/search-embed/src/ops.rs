//! The element-wise work of a layer, as PyTorch's vectorized CPU kernels compute it: softmax
//! (`max`, `exp(x − max)`, times `1 / sum`), GELU with `erf` (`Vectorized<float>::erf`:
//! Abramowitz and Stegun 7.1.26 with fused multiply-adds), and layer normalization
//! (`(x · rstd + (−mean · rstd)) · γ + β`). Each is written once and compiled twice: for the
//! baseline target and, chosen at run time, with AVX2 and FMA, so the compiler vectorizes it
//! (a 512-token input has 38 million attention scores). Rust never fuses `a * b + c` on its own,
//! so both builds compute the same values.

/// Runs `$body` (a call of an `#[inline(always)]` function) with AVX2 and FMA when the CPU has
/// them.
macro_rules! dispatch {
    ($name:ident, $generic:ident, ($($arg:ident: $ty:ty),*)) => {
        pub fn $name($($arg: $ty),*) {
            #[cfg(target_arch = "x86_64")]
            {
                #[target_feature(enable = "avx2,fma")]
                fn fast($($arg: $ty),*) {
                    $generic($($arg),*)
                }
                if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
                    // SAFETY: the features are present.
                    return unsafe { fast($($arg),*) };
                }
            }
            $generic($($arg),*)
        }
    };
}

/// `v` rounded half to even, for `|v| < 2²²` (adding and subtracting 1.5 · 2²³ in the default
/// rounding mode).
#[inline(always)]
pub fn round_even(v: f32) -> f32 {
    const MAGIC: f32 = 12_582_912.0;
    (v + MAGIC) - MAGIC
}

/// `e^x`, computed in `f64` (a degree-10 polynomial after `x = n ln 2 + r`) and rounded once, so
/// within half an ulp but for the rarest ties. An `f32` polynomial (Cephes' `expf`, about one ulp
/// like Sleef's `expf_u10` in PyTorch's kernels) is a fifth faster overall but moves more 8-bit
/// activations across a rounding step than Elasticsearch's (agreement mean 0.99895 against
/// 0.99906), so the closer one stays.
#[inline(always)]
pub fn exp(x: f32) -> f32 {
    // 1.5 · 2⁵²: adding it rounds to an integer, which its low bits then hold.
    const MAGIC: f64 = 6_755_399_441_055_744.0;
    let x = f64::from(x.clamp(-104.0, 88.8));
    let t = x * std::f64::consts::LOG2_E + MAGIC;
    let n = t - MAGIC;
    let ni = (t.to_bits() as i64).wrapping_sub(MAGIC.to_bits() as i64);
    let r = x - n * std::f64::consts::LN_2;
    let mut p = 1.0 / 3_628_800.0;
    for c in [1.0 / 362_880.0, 1.0 / 40_320.0, 1.0 / 5_040.0, 1.0 / 720.0, 1.0 / 120.0, 1.0 / 24.0, 1.0 / 6.0, 0.5, 1.0, 1.0] {
        p = p * r + c;
    }
    (p * f64::from_bits((ni.wrapping_add(1023) as u64) << 52)) as f32
}

/// `erf(x)` as PyTorch's `Vectorized<float>::erf`.
#[inline(always)]
pub fn erf(x: f32) -> f32 {
    const P: f32 = 0.327_591_1;
    const P1: f32 = 0.254_829_6;
    const P2: f32 = -0.284_496_74;
    const P3: f32 = 1.421_413_8;
    const P4: f32 = -1.453_152_1;
    const P5: f32 = 1.061_405_4;
    let a = x.abs();
    let t = 1.0 / P.mul_add(a, 1.0);
    let r = P5.mul_add(t, P4).mul_add(t, P3).mul_add(t, P2).mul_add(t, P1);
    let e = -exp(-(x * x));
    (e * t).mul_add(r, 1.0).copysign(x)
}

#[inline(always)]
fn gelu_generic(xs: &mut [f32]) {
    for x in xs.iter_mut() {
        *x = (*x * 0.5) * (1.0 + erf(*x * std::f32::consts::FRAC_1_SQRT_2));
    }
}

dispatch!(gelu, gelu_generic, (xs: &mut [f32]));

/// Lanes of the partial reductions, as a `Vectorized<float>` of AVX2 accumulates them.
const LANES: usize = 8;

/// The largest of `xs`, reduced lane by lane (exact whatever the order).
#[inline(always)]
fn max_of(xs: &[f32]) -> f32 {
    let mut m = [f32::NEG_INFINITY; LANES];
    let (chunks, rest) = xs.as_chunks::<LANES>();
    for c in chunks {
        for i in 0..LANES {
            m[i] = if c[i] > m[i] { c[i] } else { m[i] };
        }
    }
    rest.iter().chain(&m).fold(f32::NEG_INFINITY, |a, b| if *b > a { *b } else { a })
}

/// The sum of `xs` in [`LANES`] partial sums.
#[inline(always)]
fn sum_of(xs: &[f32]) -> f32 {
    let mut s = [0f32; LANES];
    let (chunks, rest) = xs.as_chunks::<LANES>();
    for c in chunks {
        for i in 0..LANES {
            s[i] += c[i];
        }
    }
    s.iter().sum::<f32>() + rest.iter().sum::<f32>()
}

/// Divides each value by `divisor`, then takes the softmax of each row of `n` values, in place.
/// Each step is its own loop, so each vectorizes.
#[inline(always)]
fn softmax_generic(xs: &mut [f32], n: usize, divisor: f32) {
    for row in xs.chunks_exact_mut(n) {
        for v in row.iter_mut() {
            *v /= divisor;
        }
        let max = max_of(row);
        for v in row.iter_mut() {
            *v = exp(*v - max);
        }
        let inv = 1.0 / sum_of(row);
        for v in row.iter_mut() {
            *v *= inv;
        }
    }
}

dispatch!(softmax_rows_scaled, softmax_generic, (xs: &mut [f32], n: usize, divisor: f32));

/// Layer normalization of one row, in place.
#[inline(always)]
fn layer_norm_generic(x: &mut [f32], gamma: &[f32], beta: &[f32], eps: f32) {
    let n = x.len() as f64;
    let mean = x.iter().map(|v| f64::from(*v)).sum::<f64>() / n;
    let var = x.iter().map(|v| (f64::from(*v) - mean).powi(2)).sum::<f64>() / n;
    let (mean, var) = (mean as f32, var as f32);
    let rstd = 1.0 / (var + eps).sqrt();
    let bias = -mean * rstd;
    for ((v, g), b) in x.iter_mut().zip(gamma).zip(beta) {
        *v = (*v * rstd + bias) * g + b;
    }
}

dispatch!(layer_norm, layer_norm_generic, (x: &mut [f32], gamma: &[f32], beta: &[f32], eps: f32));

/// Quantizes `src` to `dst` as fbgemm's legacy `Quantize`: `x · inv + zero` in one fused
/// multiply-add (an FMA instruction with the `fma` feature, `fmaf` otherwise: the same value),
/// rounded half to even, clamped to a byte.
#[inline(always)]
fn quantize_generic(src: &[f32], dst: &mut [u8], inv: f32, zero: f32) {
    for (d, v) in dst.iter_mut().zip(src) {
        *d = round_even(v.mul_add(inv, zero)).clamp(0.0, 255.0) as u8;
    }
}

dispatch!(quantize, quantize_generic, (src: &[f32], dst: &mut [u8], inv: f32, zero: f32));

/// The smallest and largest of `xs` (`(+∞, −∞)` when empty).
pub fn min_max(xs: &[f32]) -> (f32, f32) {
    let mut out = (f32::INFINITY, f32::NEG_INFINITY);
    min_max_into(xs, &mut out);
    out
}

#[inline(always)]
fn min_max_generic(xs: &[f32], out: &mut (f32, f32)) {
    let (mut lo, mut hi) = ([out.0; LANES], [out.1; LANES]);
    let (chunks, rest) = xs.as_chunks::<LANES>();
    for c in chunks {
        for i in 0..LANES {
            lo[i] = if c[i] < lo[i] { c[i] } else { lo[i] };
            hi[i] = if c[i] > hi[i] { c[i] } else { hi[i] };
        }
    }
    for v in rest.iter().chain(&lo) {
        out.0 = if *v < out.0 { *v } else { out.0 };
    }
    for v in rest.iter().chain(&hi) {
        out.1 = if *v > out.1 { *v } else { out.1 };
    }
}

dispatch!(min_max_into, min_max_generic, (xs: &[f32], out: &mut (f32, f32)));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_erf_and_rounding_are_accurate() {
        for i in -2000..2000 {
            let x = i as f32 / 100.0;
            let (e, want) = (exp(x), (x as f64).exp());
            // Within about an ulp of the true value.
            assert!(((e as f64 - want) / want).abs() < 1.2e-7, "exp({x}) = {e}, want {want}");
        }
        // A&S 7.1.26 is within 1.5e-7 of erf.
        for (x, want) in [(0.5f32, 0.520_499_877_8f64), (1.0, 0.842_700_792_9), (-2.0, -0.995_322_265), (0.0, 0.0), (3.0, 0.999_977_909_5)] {
            assert!((erf(x) as f64 - want).abs() < 4e-7, "erf({x}) = {}", erf(x));
        }
        for (v, want) in [(0.5f32, 0.0f32), (1.5, 2.0), (2.5, 2.0), (-0.5, 0.0), (-1.5, -2.0), (126.4, 126.0), (126.6, 127.0)] {
            assert_eq!(round_even(v), want, "{v}");
            assert_eq!(v.round_ties_even(), want);
        }
    }

    #[test]
    fn rows_normalize_and_sum_to_one() {
        let mut s = vec![1.0f32, 2.0, 3.0, 0.0, 0.0, 0.0];
        softmax_rows_scaled(&mut s, 3, 2.0);
        assert!((s[..3].iter().sum::<f32>() - 1.0).abs() < 1e-6 && (s[3] - 1.0 / 3.0).abs() < 1e-7);
        let mut x = vec![1.0f32, 2.0, 3.0, 4.0];
        layer_norm(&mut x, &[1.0; 4], &[0.0; 4], 1e-12);
        let mean: f32 = x.iter().sum::<f32>() / 4.0;
        let var: f32 = x.iter().map(|v| v * v).sum::<f32>() / 4.0;
        assert!(mean.abs() < 1e-6 && (var - 1.0).abs() < 1e-5, "{x:?}");
        assert_eq!(min_max(&[3.0, -1.0, 2.0]), (-1.0, 3.0));
    }
}
