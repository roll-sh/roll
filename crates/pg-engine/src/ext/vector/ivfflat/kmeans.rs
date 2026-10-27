//! ivfflat's k-means (`ivfkmeans.c`): k-means++ initialization, then Elkan's accelerated
//! Lloyd iterations (spherical for the classes with a k-means norm), with the reference's
//! float arithmetic: the same float / double conversions, in the same order.

use super::types::{Kind, Procs};
use crate::Result;
use crate::error::{Error, code};
use crate::exec::ExecCtx;
use crate::types::Value;

/// The random draws of pg_global_prng_state (`RandomInt`, `RandomDouble`).
struct Rng<'a, 'b>(&'a ExecCtx<'b>);

impl Rng<'_, '_> {
    fn int(&self) -> u32 {
        self.0.sess.global_random_u32()
    }

    fn double(&self) -> f64 {
        self.0.sess.global_random_double()
    }
}

/// `VECTOR_ARRAY_SIZE`: the bytes of a VectorArray of `length` items of `size`.
pub(super) fn vector_array_size(length: usize, size: usize) -> usize {
    // sizeof(VectorArrayData)
    32 + length * super::page::maxalign(size)
}

/// `IvfflatCheckMemoryUsage` against maintenance_work_mem (KB).
pub(super) fn check_memory_usage(total: usize, mwm: i64) -> Result<()> {
    if total / 1024 > mwm as usize {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("memory required is {} MB, maintenance_work_mem is {} MB", total / (1024 * 1024) + 1, mwm / 1024),
        ));
    }
    Ok(())
}

/// `IvfflatKmeans`: `lists` centers of `samples` (`memory_used`: the build's VectorArrays so
/// far; `mwm`: maintenance_work_mem).
pub(super) fn kmeans(samples: &[Value], lists: usize, dims: usize, procs: &Procs, memory_used: usize, mwm: i64, ctx: &ExecCtx) -> Result<Vec<Value>> {
    let rng = Rng(ctx);
    let centers = if samples.is_empty() { random_centers(lists, dims, procs, &rng)? } else { elkan(samples, lists, dims, procs, memory_used, mwm, &rng, ctx)? };
    check_centers(&centers, lists, dims, procs)?;
    Ok(centers)
}

/// `IvfflatNormVectors`: each normalized, its dimensions checked.
fn norm_vectors(v: &mut [Value], dims: usize, kind: Kind) -> Result<()> {
    for x in v.iter_mut() {
        let n = kind.normalize(x);
        // Safety check
        super::types::check_expected_dim(dims as i32, kind.dimensions(&n))?;
        *x = n;
    }
    Ok(())
}

/// `RandomCenters`: the quick approach when there is no data.
fn random_centers(lists: usize, dims: usize, procs: &Procs, rng: &Rng) -> Result<Vec<Value>> {
    let mut x = vec![0f32; dims];
    let mut centers = Vec::with_capacity(lists);
    // Fill with random data
    while centers.len() < lists {
        for xi in x.iter_mut() {
            *xi = rng.double() as f32;
        }
        centers.push(procs.kind.center(&x));
    }
    if procs.kmeans_norm.is_some() {
        norm_vectors(&mut centers, dims, procs.kind)?;
    }
    Ok(centers)
}

/// `InitCenters`: k-means++, filling `lower_bound` with each sample's distance to each
/// center.
fn init_centers(samples: &[Value], num_centers: usize, procs: &Procs, lower_bound: &mut [f32], rng: &Rng, ctx: &ExecCtx) -> Result<Vec<Value>> {
    let n = samples.len();
    let dist = procs.kmeans_distance;
    let mut weight = vec![f32::MAX; n];
    let mut centers = Vec::with_capacity(num_centers);
    // Choose an initial center uniformly at random
    centers.push(samples[(rng.int() % n as u32) as usize].clone());
    for i in 0..num_centers {
        ctx.check()?;
        let mut sum = 0.0f64;
        for j in 0..n {
            // Only need to compute distance for new center
            let mut distance = dist(&samples[j], &centers[i]);
            // Set lower bound
            lower_bound[j * num_centers + i] = distance as f32;
            // Use distance squared for weighted probability distribution
            distance *= distance;
            if distance < weight[j] as f64 {
                weight[j] = distance as f32;
            }
            sum += weight[j] as f64;
        }
        // Only compute lower bound on last iteration
        if i + 1 == num_centers {
            break;
        }
        // Choose new center using weighted probability distribution.
        let mut choice = sum * rng.double();
        let mut j = 0;
        while j < n - 1 {
            choice -= weight[j] as f64;
            if choice <= 0.0 {
                break;
            }
            j += 1;
        }
        centers.push(samples[j].clone());
    }
    Ok(centers)
}

/// `ComputeNewCenters`: each center the mean of its samples (a random point when it has
/// none), normalized for spherical k-means.
fn compute_new_centers(samples: &[Value], agg: &mut [f32], counts: &mut [i32], closest: &[usize], dims: usize, procs: &Procs, rng: &Rng) -> Result<Vec<Value>> {
    let num_centers = counts.len();
    // Reset sum and count
    agg.fill(0.0);
    counts.fill(0);
    // Increment sum of closest center (SumCenters)
    for (s, &c) in samples.iter().zip(closest) {
        procs.kind.sum_center(s, &mut agg[c * dims..(c + 1) * dims])?;
    }
    // Increment count of closest center
    for &c in closest {
        counts[c] += 1;
    }
    // Divide sum by count
    for i in 0..num_centers {
        let x = &mut agg[i * dims..(i + 1) * dims];
        if counts[i] > 0 {
            for v in x.iter_mut() {
                if v.is_infinite() {
                    *v = if *v > 0.0 { f32::MAX } else { -f32::MAX };
                }
            }
            for v in x.iter_mut() {
                *v /= counts[i] as f32;
            }
        } else {
            // TODO Handle empty centers properly
            for v in x.iter_mut() {
                *v = rng.double() as f32;
            }
        }
    }
    // Set new centers (UpdateCenters)
    let mut centers: Vec<Value> = agg.chunks_exact(dims).map(|x| procs.kind.center(x)).collect();
    // Normalize if needed
    if procs.kmeans_norm.is_some() {
        norm_vectors(&mut centers, dims, procs.kind)?;
    }
    Ok(centers)
}

/// `ElkanKmeans`: Elkan's triangle-inequality bounds over the k-means distance (L2, not L2
/// squared; angular for inner product and cosine), at most 500 iterations.
#[allow(clippy::too_many_arguments)]
fn elkan(samples: &[Value], num_centers: usize, dims: usize, procs: &Procs, memory_used: usize, mwm: i64, rng: &Rng, ctx: &ExecCtx) -> Result<Vec<Value>> {
    let n = samples.len();
    let k = num_centers;
    let dist = procs.kmeans_distance;
    let itemsize = super::page::maxalign(procs.kind.item_size(dims as i32));
    // Calculate allocation sizes, the total, and check memory requirements
    let total = memory_used
        + vector_array_size(k, itemsize)
        + 4 * k * dims
        + 4 * k
        + 4 * n
        + 4 * n * k
        + 4 * n
        + 4 * k
        + 4 * k * k
        + 4 * k;
    check_memory_usage(total, mwm)?;
    let mut agg = vec![0f32; k * dims];
    let mut counts = vec![0i32; k];
    let mut closest = vec![0usize; n];
    let mut lower = vec![0f32; n * k];
    let mut upper = vec![0f32; n];
    let mut s = vec![0f32; k];
    let mut halfcdist = vec![0f32; k * k];
    let mut newcdist = vec![0f32; k];
    // Pick initial centers
    let mut centers = init_centers(samples, k, procs, &mut lower, rng, ctx)?;
    // Assign each x to its closest initial center c(x) = argmin d(x,c)
    for j in 0..n {
        let mut min = f32::MAX;
        let mut c = 0;
        for (kk, &d) in lower[j * k..(j + 1) * k].iter().enumerate() {
            if d < min {
                min = d;
                c = kk;
            }
        }
        upper[j] = min;
        closest[j] = c;
    }
    // Give 500 iterations to converge
    for iteration in 0..500 {
        let mut changes = 0;
        ctx.check()?;
        // Step 1: For all centers, compute distance
        for j in 0..k {
            for kk in j + 1..k {
                let d = (0.5 * dist(&centers[j], &centers[kk])) as f32;
                halfcdist[j * k + kk] = d;
                halfcdist[kk * k + j] = d;
            }
        }
        // For all centers c, compute s(c)
        for j in 0..k {
            let mut min = f32::MAX;
            for kk in 0..k {
                if j != kk && halfcdist[j * k + kk] < min {
                    min = halfcdist[j * k + kk];
                }
            }
            s[j] = min;
        }
        let rjreset = iteration != 0;
        for j in 0..n {
            // Step 2: Identify all points x such that u(x) <= s(c(x))
            if upper[j] <= s[closest[j]] {
                continue;
            }
            let mut rj = rjreset;
            for kk in 0..k {
                // Step 3: For all remaining points x and centers c
                let cj = closest[j];
                if kk == cj || upper[j] <= lower[j * k + kk] || upper[j] <= halfcdist[cj * k + kk] {
                    continue;
                }
                // Step 3a
                let dxcx = if rj {
                    let d = dist(&samples[j], &centers[cj]) as f32;
                    // d(x,c(x)) computed, which is a form of d(x,c)
                    lower[j * k + cj] = d;
                    upper[j] = d;
                    rj = false;
                    d
                } else {
                    upper[j]
                };
                // Step 3b
                if dxcx > lower[j * k + kk] || dxcx > halfcdist[cj * k + kk] {
                    let dxc = dist(&samples[j], &centers[kk]) as f32;
                    // d(x,c) calculated
                    lower[j * k + kk] = dxc;
                    if dxc < dxcx {
                        closest[j] = kk;
                        // c(x) changed
                        upper[j] = dxc;
                        changes += 1;
                    }
                }
            }
        }
        // Step 4: For each center c, let m(c) be mean of all points assigned
        let new_centers = compute_new_centers(samples, &mut agg, &mut counts, &closest, dims, procs, rng)?;
        // Step 5
        for j in 0..k {
            newcdist[j] = dist(&centers[j], &new_centers[j]) as f32;
        }
        for row in lower.chunks_exact_mut(k) {
            for (l, nd) in row.iter_mut().zip(&newcdist) {
                let d = *l - *nd;
                *l = if d < 0.0 { 0.0 } else { d };
            }
        }
        // Step 6: We reset r(x) before Step 3 in the next iteration
        for j in 0..n {
            upper[j] += newcdist[closest[j]];
        }
        // Step 7
        centers = new_centers;
        if changes == 0 && iteration != 0 {
            break;
        }
    }
    Ok(centers)
}

/// `CheckCenters`: all there, no NaN or infinite elements (`CheckElements`), no zero norm
/// where values are normalized (`CheckNorms`).
fn check_centers(centers: &[Value], lists: usize, dims: usize, procs: &Procs) -> Result<()> {
    if centers.len() != lists {
        return Err(crate::error::internal("Not enough centers. Please report a bug."));
    }
    let mut scratch = vec![0f32; dims];
    for c in centers {
        scratch.fill(0.0);
        procs.kind.sum_center(c, &mut scratch)?;
        for v in &scratch {
            if v.is_nan() {
                return Err(crate::error::internal("NaN detected. Please report a bug."));
            }
            if v.is_infinite() {
                return Err(crate::error::internal("Infinite value detected. Please report a bug."));
            }
        }
    }
    // Check NORM_PROC instead of KMEANS_NORM_PROC
    if let Some(norm) = procs.norm {
        for c in centers {
            if norm(c) == 0.0 {
                return Err(crate::error::internal("Zero norm detected. Please report a bug."));
            }
        }
    }
    Ok(())
}
