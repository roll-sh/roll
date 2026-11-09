//! Storage metrics: bytes hot/cold and an append latency histogram.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Upper bounds of the latency buckets, in microseconds (the last bucket is unbounded).
pub const LATENCY_BOUNDS_US: [u64; 16] =
    [100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_500_000, 5_000_000, 10_000_000];

/// A snapshot of a store's metrics.
#[derive(Clone, Debug, Default, Serialize)]
pub struct StorageStats {
    pub backend: String,
    /// Logs currently open.
    pub logs: usize,
    /// Bytes on keepers (or, for S3 Express, in the hot object tier).
    pub bytes_hot: u64,
    /// Bytes in object storage.
    pub bytes_cold: u64,
    /// Bytes of cold segments cached in memory.
    pub cache_bytes: u64,
    pub appends: u64,
    pub append_errors: u64,
    pub appended_bytes: u64,
    pub append_latency: Histogram,
    /// Per-keeper reachability (empty for S3 Express).
    pub keepers: Vec<KeeperStatus>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct KeeperStatus {
    pub addr: String,
    pub az: String,
    pub connected: bool,
    /// Bytes to and from the keeper as encoded, and as they crossed the network (compressed).
    pub sent_bytes: u64,
    pub sent_wire_bytes: u64,
    pub received_bytes: u64,
    pub received_wire_bytes: u64,
}

/// Cumulative histogram snapshot: `counts[i]` appends took at most `LATENCY_BOUNDS_US[i]`
/// (exclusive of earlier buckets); the last count is everything slower.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Histogram {
    pub bounds_us: Vec<u64>,
    pub counts: Vec<u64>,
    pub count: u64,
    pub sum_us: u64,
}

impl Histogram {
    /// Approximate quantile (upper bound of the bucket holding it), in microseconds.
    pub fn quantile(&self, q: f64) -> u64 {
        let want = ((self.count as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return self.bounds_us.get(i).copied().unwrap_or(u64::MAX);
            }
        }
        0
    }
}

#[derive(Default)]
pub(crate) struct Metrics {
    buckets: [AtomicU64; 17],
    count: AtomicU64,
    sum_us: AtomicU64,
    errors: AtomicU64,
    bytes: AtomicU64,
}

impl Metrics {
    pub fn record(&self, took: Duration, bytes: u64, ok: bool) {
        if !ok {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let us = took.as_micros() as u64;
        let i = LATENCY_BOUNDS_US.partition_point(|b| *b < us);
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn fill(&self, s: &mut StorageStats) {
        s.appends = self.count.load(Ordering::Relaxed);
        s.append_errors = self.errors.load(Ordering::Relaxed);
        s.appended_bytes = self.bytes.load(Ordering::Relaxed);
        s.append_latency = Histogram {
            bounds_us: LATENCY_BOUNDS_US.to_vec(),
            counts: self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect(),
            count: s.appends,
            sum_us: self.sum_us.load(Ordering::Relaxed),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram() {
        let m = Metrics::default();
        for ms in [1, 1, 2, 3, 40] {
            m.record(Duration::from_millis(ms), 10, true);
        }
        m.record(Duration::ZERO, 0, false);
        let mut s = StorageStats::default();
        m.fill(&mut s);
        assert_eq!((s.appends, s.append_errors, s.appended_bytes), (5, 1, 50));
        assert_eq!(s.append_latency.quantile(0.5), 2_500);
        assert_eq!(s.append_latency.quantile(1.0), 50_000);
    }
}
