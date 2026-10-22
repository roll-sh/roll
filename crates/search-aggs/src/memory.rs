//! Memory accounting (Elasticsearch's request circuit breaker for aggregations).
//!
//! A [`MemoryBudget`] is shared by everything one cluster runs (or one request, as the caller
//! decides). Aggregators reserve the bytes of their growing structures (bucket ordinal hashes,
//! per-bucket arrays, HyperLogLog registers, digests) through a [`MemTracker`], which releases
//! its reservation when dropped, so a request that fails or finishes gives everything back.
//! Reservations are amortized: trackers only call into the budget when a structure's
//! capacity grows, never per document.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{AggError, Result};
use crate::json::Json;

/// A byte budget shared by concurrent aggregations.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: u64,
    used: AtomicU64,
    /// The breaker name in messages (`request`).
    name: &'static str,
}

impl MemoryBudget {
    pub fn new(limit: u64) -> Arc<MemoryBudget> {
        Arc::new(MemoryBudget { limit, used: AtomicU64::new(0), name: "request" })
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Reserves `bytes` for `label`, failing like Elasticsearch's `CircuitBreakingException`.
    pub fn reserve(&self, bytes: u64, label: &str) -> Result<()> {
        let new = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if new > self.limit {
            self.used.fetch_sub(bytes, Ordering::Relaxed);
            return Err(breaking_error(self.name, label, new, self.limit));
        }
        Ok(())
    }

    pub fn release(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// `CircuitBreakingException` as Elasticsearch renders it.
pub fn breaking_error(name: &str, label: &str, wanted: u64, limit: u64) -> AggError {
    AggError::new(
        429,
        "circuit_breaking_exception",
        format!(
            "[{name}] Data too large, data for [{label}] would be [{wanted}/{}], which is larger than the limit of [{limit}/{}]",
            byte_size(wanted),
            byte_size(limit)
        ),
    )
    .with_meta("bytes_wanted", Json::from(wanted))
    .with_meta("bytes_limit", Json::from(limit))
    .with_meta("durability", Json::str("TRANSIENT"))
}

/// `ByteSizeValue.toString()` (`1.5gb`, `512b`): `Strings.format1Decimals`, which keeps the
/// first decimal of Java's `Double.toString` (truncating: 2^34 - 8 bytes is `15.9gb`).
pub fn byte_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 5] = [("pb", 1 << 50), ("tb", 1 << 40), ("gb", 1 << 30), ("mb", 1 << 20), ("kb", 1 << 10)];
    let (value, suffix) = UNITS.iter().find(|(_, size)| bytes >= *size).map_or((bytes as f64, "b"), |&(u, size)| (bytes as f64 / size as f64, u));
    let p = crate::json::java_double(value);
    let Some(dot) = p.find('.') else { return format!("{p}{suffix}") };
    let exp = p.find('E').map_or("", |e| &p[e..]);
    let fraction = p.as_bytes().get(dot + 1).copied().unwrap_or(b'0');
    if fraction == b'0' { format!("{}{exp}{suffix}", &p[..dot]) } else { format!("{}{}{exp}{suffix}", &p[..=dot], fraction as char) }
}

/// One aggregator's reservation in a budget (released on drop).
#[derive(Debug, Default)]
pub struct MemTracker {
    budget: Option<Arc<MemoryBudget>>,
    reserved: u64,
    label: String,
}

impl MemTracker {
    pub fn new(budget: Option<Arc<MemoryBudget>>, agg_name: &str) -> MemTracker {
        MemTracker { budget, reserved: 0, label: format!("<agg [{agg_name}]>") }
    }

    /// A tracker reporting `label` verbatim in breaker messages.
    pub fn with_label(budget: Option<Arc<MemoryBudget>>, label: &str) -> MemTracker {
        MemTracker { budget, reserved: 0, label: label.to_string() }
    }

    /// Grows the reservation to at least `total` bytes.
    #[inline]
    pub fn ensure(&mut self, total: usize) -> Result<()> {
        let total = total as u64;
        if total <= self.reserved {
            return Ok(());
        }
        if let Some(b) = &self.budget {
            // Reserve with headroom so steady growth calls the budget rarely.
            let want = total + total / 8;
            b.reserve(want - self.reserved, &self.label)?;
            self.reserved = want;
        } else {
            self.reserved = total;
        }
        Ok(())
    }

    pub fn reserved(&self) -> u64 {
        self.reserved
    }
}

impl Drop for MemTracker {
    fn drop(&mut self) {
        if let Some(b) = &self.budget {
            b.release(self.reserved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaks_and_releases() {
        let b = MemoryBudget::new(1000);
        {
            let mut t = MemTracker::new(Some(b.clone()), "a");
            t.ensure(500).unwrap();
            let e = t.ensure(5000).unwrap_err();
            assert_eq!(e.kind, "circuit_breaking_exception");
            assert!(e.reason.starts_with("[request] Data too large, data for [<agg [a]>] would be ["), "{}", e.reason);
        }
        assert_eq!(b.used(), 0);
        assert_eq!(byte_size(1536), "1.5kb");
        assert_eq!(byte_size(100), "100b");
        assert_eq!(byte_size(17_179_869_176), "15.9gb");
        assert_eq!(byte_size(1 << 30), "1gb");
        assert_eq!(byte_size(2047), "1.9kb");
    }
}
