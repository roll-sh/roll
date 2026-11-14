//! Elasticsearch's `HistogramUnionState`: the sketch behind `percentiles`, `percentile_ranks`
//! and `boxplot` once `exponential_histogram` data takes part.
//!
//! It holds an optional t-digest (raw values, `histogram` fields) and an optional merged
//! exponential histogram (`ExponentialHistogramState`, at most 320 buckets). Reads use whichever
//! is present; with both, the digest's centroids are converted to a scale-38 exponential
//! histogram and merged with the other (`getCombinedState`). Aggregations over numeric and
//! `histogram` fields keep using a bare [`TDigest`]: a union holding only a digest reads exactly
//! like it, so the union only appears where exponential histograms do.

use crate::error::Result;
use crate::exp_histogram::{self, DEFAULT_MAX_BUCKETS, ExpHistogram, Merger};
use crate::metrics::simple::{java_max, java_min};
use crate::metrics::tdigest::TDigest;

/// `HistogramUnionState`.
#[derive(Clone, Debug)]
pub struct UnionState {
    pub tdigest: Option<TDigest>,
    pub exp: Option<Merger>,
    /// `TDigestInitParams` for a digest created later.
    compression: f64,
    high_accuracy: bool,
}

impl UnionState {
    /// `create(breaker, executionHint, compression)`: nothing allocated yet.
    pub fn new(compression: f64, high_accuracy: bool) -> UnionState {
        UnionState { tdigest: None, exp: None, compression, high_accuracy }
    }

    /// `wrap(tdigest)`.
    pub fn wrap(t: TDigest) -> UnionState {
        let compression = t.compression();
        let high_accuracy = matches!(t, TDigest::Avl(_));
        UnionState { tdigest: Some(t), exp: None, compression, high_accuracy }
    }

    /// `createUsingParamsFrom`: an empty state configured like `other` (an empty digest when
    /// `other` has one).
    pub fn like(other: &UnionState) -> UnionState {
        UnionState {
            tdigest: other.tdigest.as_ref().map(TDigest::empty_like),
            exp: None,
            compression: other.compression,
            high_accuracy: other.high_accuracy,
        }
    }

    /// `compression()`.
    pub fn compression(&self) -> f64 {
        self.tdigest.as_ref().map_or(self.compression, TDigest::compression)
    }

    fn exp_mut(&mut self) -> &mut Merger {
        self.exp.get_or_insert_with(|| Merger::new(DEFAULT_MAX_BUCKETS))
    }

    fn tdigest_mut(&mut self) -> &mut TDigest {
        let (c, h) = (self.compression, self.high_accuracy);
        self.tdigest.get_or_insert_with(|| TDigest::new(c, h))
    }

    /// `add(ExponentialHistogram)`.
    pub fn add_exp(&mut self, h: &ExpHistogram) {
        self.exp_mut().add(h);
    }

    /// `add(double)`: a raw value into the digest.
    pub fn add_value(&mut self, v: f64) -> Result<()> {
        self.tdigest_mut().add(v)
    }

    /// `add(TDigestState)`.
    pub fn add_tdigest(&mut self, t: &mut TDigest) -> Result<()> {
        self.tdigest_mut().merge(t)
    }

    /// `add(HistogramUnionState)`.
    pub fn add(&mut self, other: &mut UnionState) -> Result<()> {
        if let Some(e) = &other.exp {
            let h = e.get().clone();
            self.exp_mut().add(&h);
        }
        if let Some(t) = other.tdigest.as_mut() {
            self.add_tdigest(t)?;
        }
        Ok(())
    }

    /// `compress()`.
    pub fn compress(&mut self) {
        if let Some(t) = self.tdigest.as_mut() {
            t.compress();
        }
    }

    /// `size()`.
    pub fn size(&self) -> u64 {
        self.exp.as_ref().map_or(0, |e| e.get().value_count()) + self.tdigest.as_ref().map_or(0, TDigest::size)
    }

    /// `getCombinedState()` (for a compressed digest).
    fn combined(&self, e: &Merger, t: &TDigest) -> Merger {
        let mut m = Merger::new(DEFAULT_MAX_BUCKETS);
        m.add(e.get());
        m.add(&exp_histogram::from_centroids(&t.centroids_compressed()));
        m
    }

    /// `quantile(q)` of a result (compressed) state.
    pub fn quantile(&self, q: f64) -> f64 {
        match (&self.tdigest, &self.exp) {
            (Some(t), Some(e)) => exp_histogram::quantile(self.combined(e, t).get(), q),
            (Some(t), None) => t.quantile_compressed(q),
            (None, Some(e)) => exp_histogram::quantile(e.get(), q),
            (None, None) => f64::NAN,
        }
    }

    /// `quantile(q)` of a live state (merging digests merge their pending samples first).
    pub fn quantile_live(&mut self, q: f64) -> f64 {
        match (self.tdigest.as_mut(), &self.exp) {
            (Some(t), None) => t.quantile(q),
            (Some(_), Some(_)) => self.compress_view().quantile(q),
            _ => self.quantile(q),
        }
    }

    /// `cdf(x)` of a result state.
    pub fn cdf(&self, x: f64) -> Result<f64> {
        Ok(match (&self.tdigest, &self.exp) {
            (Some(t), Some(e)) => exp_histogram::cdf(self.combined(e, t).get(), x),
            (Some(t), None) => t.cdf_compressed(x)?,
            (None, Some(e)) => exp_histogram::cdf(e.get(), x),
            (None, None) => f64::NAN,
        })
    }

    /// `cdf(x)` of a live state.
    pub fn cdf_live(&mut self, x: f64) -> Result<f64> {
        match (self.tdigest.as_mut(), &self.exp) {
            (Some(t), None) => t.cdf(x),
            (Some(_), Some(_)) => self.compress_view().cdf(x),
            _ => self.cdf(x),
        }
    }

    /// Mixed live reads (only reduces mix the two, so shard-side reads never get here).
    fn compress_view(&mut self) -> UnionState {
        let mut v = self.clone();
        v.compress();
        v
    }

    /// `centroids()`: ascending `(mean, count)`.
    pub fn centroids(&self) -> Vec<(f64, u64)> {
        match (&self.tdigest, &self.exp) {
            (Some(t), Some(e)) => self.combined(e, t).get().centroids(),
            (Some(t), None) => t.centroids_compressed(),
            (None, Some(e)) => e.get().centroids(),
            (None, None) => Vec::new(),
        }
    }

    /// `getMin()`: `+Infinity` when empty.
    pub fn min(&self) -> f64 {
        let mut m = f64::INFINITY;
        if let Some(t) = &self.tdigest {
            m = java_min(m, t.min());
        }
        if let Some(e) = &self.exp {
            let v = e.get().min;
            m = java_min(m, if v.is_nan() { f64::INFINITY } else { v });
        }
        m
    }

    /// `getMax()`: `-Infinity` when empty.
    pub fn max(&self) -> f64 {
        let mut m = f64::NEG_INFINITY;
        if let Some(t) = &self.tdigest {
            m = java_max(m, t.max());
        }
        if let Some(e) = &self.exp {
            let v = e.get().max;
            m = java_max(m, if v.is_nan() { f64::NEG_INFINITY } else { v });
        }
        m
    }

    pub fn ram_bytes(&self) -> usize {
        48 + self.tdigest.as_ref().map_or(0, TDigest::ram_bytes) + self.exp.as_ref().map_or(0, Merger::ram_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exp_histogram::{ExpHistogramValue, decode_into, encode};

    fn doc(pos: &[(i64, u64)], min: f64, max: f64) -> ExpHistogram {
        let v = ExpHistogramValue { scale: 3, positive: pos.to_vec(), min: Some(min), max: Some(max), ..Default::default() };
        let mut h = ExpHistogram::empty();
        decode_into(&encode(&v), &mut h).unwrap();
        h
    }

    #[test]
    fn reads_follow_the_present_parts() {
        let mut u = UnionState::new(100.0, false);
        assert!(u.quantile(0.5).is_nan());
        assert_eq!((u.min(), u.max(), u.size()), (f64::INFINITY, f64::NEG_INFINITY, 0));
        u.add_exp(&doc(&[(0, 2), (8, 1)], 1.0, 2.0));
        assert_eq!(u.size(), 3);
        assert_eq!((u.min(), u.max()), (1.0, 2.0));
        let exp_only = u.quantile(0.5);
        // A digest joins: reads now go through the combined state.
        let mut t = TDigest::new(100.0, false);
        t.add(1.5).unwrap();
        u.add_tdigest(&mut t).unwrap();
        u.compress();
        assert_eq!(u.size(), 4);
        let mixed = u.quantile(0.5);
        assert!(mixed.is_finite() && mixed != exp_only);
        assert_eq!(u.centroids().iter().map(|c| c.1).sum::<u64>(), 4);
        let mut like = UnionState::like(&u);
        assert!(like.tdigest.is_some() && like.exp.is_none());
        like.add(&mut u).unwrap();
        assert_eq!(like.size(), 4);
    }
}
