//! Memory accounting (Elasticsearch's request circuit breaker for aggregations).
//!
//! A [`MemoryBudget`] is shared by everything one cluster runs (or one request, as the caller
//! decides). Aggregators reserve the bytes of their growing structures (bucket ordinal hashes,
//! per-bucket arrays, HyperLogLog registers, digests) through a [`MemTracker`], which releases
//! its reservation when dropped, so a request that fails or finishes gives everything back.
//! Reservations are amortized: trackers only call into the budget when a structure's
//! capacity grows, never per document.
//!
//! A budget can have a parent ([`MemoryBudget::with_parent`], one level): every reservation is
//! taken from both, so many budgets (one per cluster, each with a limit its tenant sets) share
//! one ceiling (the node's) that none of them can raise. The parent's limit also covers memory
//! held outside it ([`MemoryBudget::set_held_elsewhere`]: a node's resident clusters), and it
//! divides its room fairly: a child holding more than its share (the room over the children
//! holding something, at least two) may grow only while an eighth of the room stays free for
//! the others, so one busy tenant alone never takes the last of it. A
//! refusal by the parent is Elasticsearch's `[parent]` breaker, told in the child's own numbers
//! (what it holds and could have had), never the node's.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{AggError, Result};
use crate::json::Json;

/// A byte budget shared by concurrent aggregations.
#[derive(Debug)]
pub struct MemoryBudget {
    /// The limit; a node-wide breaker follows its (dynamic) setting through [`set_limit`](Self::set_limit).
    limit: AtomicU64,
    used: AtomicU64,
    /// Reservations refused by this budget's own limit (`tripped` in node stats).
    tripped: AtomicU64,
    /// Reservations the parent refused (Elasticsearch's `parent` breaker trips).
    parent_tripped: AtomicU64,
    /// The breaker name in messages (`request`).
    name: &'static str,
    /// Charged with every reservation of this one (the node's budget).
    parent: Option<Arc<MemoryBudget>>,
    /// Memory held outside this budget that its limit also covers.
    elsewhere: AtomicU64,
    /// Children holding something now (a parent divides its room among them).
    active: AtomicU64,
}

impl MemoryBudget {
    pub fn new(limit: u64) -> Arc<MemoryBudget> {
        MemoryBudget::with_parent(limit, None)
    }

    /// A budget whose reservations are also taken from `parent`.
    pub fn with_parent(limit: u64, parent: Option<Arc<MemoryBudget>>) -> Arc<MemoryBudget> {
        Arc::new(MemoryBudget {
            limit: AtomicU64::new(limit),
            used: AtomicU64::new(0),
            tripped: AtomicU64::new(0),
            parent_tripped: AtomicU64::new(0),
            name: "request",
            parent,
            elsewhere: AtomicU64::new(0),
            active: AtomicU64::new(0),
        })
    }

    pub fn parent(&self) -> Option<&Arc<MemoryBudget>> {
        self.parent.as_ref()
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Changes the limit; reservations already held stay.
    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    /// Memory held outside the budget that its limit covers too (refreshed by its owner).
    pub fn set_held_elsewhere(&self, bytes: u64) {
        self.elsewhere.store(bytes, Ordering::Relaxed);
    }

    pub fn held_elsewhere(&self) -> u64 {
        self.elsewhere.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// How many reservations this budget's own limit refused.
    pub fn tripped(&self) -> u64 {
        self.tripped.load(Ordering::Relaxed)
    }

    /// How many reservations the parent refused.
    pub fn parent_tripped(&self) -> u64 {
        self.parent_tripped.load(Ordering::Relaxed)
    }

    /// Adds `bytes` to what this budget holds; the operation that takes it from nothing to
    /// something counts it active in the parent (so every count is undone by exactly one
    /// operation: the one that takes it back to nothing).
    #[inline]
    fn hold(&self, bytes: u64) -> u64 {
        let before = self.used.fetch_add(bytes, Ordering::Relaxed);
        if before == 0
            && bytes > 0
            && let Some(p) = &self.parent
        {
            p.active.fetch_add(1, Ordering::Relaxed);
        }
        before
    }

    /// Takes `bytes` from what this budget holds; the operation that leaves nothing uncounts it.
    #[inline]
    fn unhold(&self, bytes: u64) {
        let before = self.used.fetch_sub(bytes, Ordering::Relaxed);
        if before == bytes
            && bytes > 0
            && let Some(p) = &self.parent
        {
            p.active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Reserves `bytes` for `label`, here and in the parent, failing like Elasticsearch's
    /// `CircuitBreakingException` when either refuses.
    pub fn reserve(&self, bytes: u64, label: &str) -> Result<()> {
        let before = self.hold(bytes);
        let new = before + bytes;
        let limit = self.limit();
        if new.saturating_add(self.held_elsewhere()) > limit {
            self.unhold(bytes);
            self.tripped.fetch_add(1, Ordering::Relaxed);
            return Err(breaking_error(self.name, label, new, limit));
        }
        if let Some(p) = &self.parent
            && !p.admit(before, bytes)
        {
            self.unhold(bytes);
            self.parent_tripped.fetch_add(1, Ordering::Relaxed);
            // In the cluster's own numbers only: nothing about the node's room or its tenants.
            return Err(parent_error(label, before, bytes, limit));
        }
        Ok(())
    }

    /// A child holding `held` asks for `bytes` more.
    fn admit(&self, held: u64, bytes: u64) -> bool {
        let room = self.limit().saturating_sub(self.held_elsewhere());
        let before = self.used.fetch_add(bytes, Ordering::Relaxed);
        let new = before + bytes;
        // At least two share it: a child alone still leaves room for the next to arrive.
        let fair = room / self.active.load(Ordering::Relaxed).max(2);
        let headroom = room / 8;
        if new <= room && (held + bytes <= fair || new + headroom <= room) {
            return true;
        }
        self.used.fetch_sub(bytes, Ordering::Relaxed);
        self.tripped.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Adds `bytes` without checking the limit (`addWithoutBreaking`), here and in the parent.
    pub fn force(&self, bytes: u64) {
        self.hold(bytes);
        if let Some(p) = &self.parent {
            p.used.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub fn release(&self, bytes: u64) {
        if let Some(p) = &self.parent {
            p.used.fetch_sub(bytes, Ordering::Relaxed);
        }
        self.unhold(bytes);
    }
}

/// The `[parent]` refusal Elasticsearch gives (`HierarchyCircuitBreakerService.checkParentLimit`),
/// in the child's numbers: it held `held`, wanted `bytes` more, and its own limit is `limit`.
pub fn parent_error(label: &str, held: u64, bytes: u64, limit: u64) -> AggError {
    let wanted = held + bytes;
    let sized = |b: u64| format!("{b}/{}", byte_size(b));
    AggError::new(
        429,
        "circuit_breaking_exception",
        format!(
            "[parent] Data too large, data for [{label}] would be [{}], which is larger than the limit of [{}], real usage: [{}], new bytes reserved: [{}], usages [request={}]{}",
            sized(wanted),
            sized(limit),
            sized(held),
            sized(bytes),
            sized(wanted),
            if crate::dialect::opensearch() { "" } else { CIRCUIT_BREAKER_DOCS }
        ),
    )
    .with_meta("bytes_wanted", Json::from(wanted))
    .with_meta("bytes_limit", Json::from(limit))
    .with_meta("durability", Json::str("TRANSIENT"))
}

/// What Elasticsearch 9.5.5 appends to every circuit breaker message
/// (`ReferenceDocs.CIRCUIT_BREAKER_ERRORS`); OpenSearch appends nothing.
pub const CIRCUIT_BREAKER_DOCS: &str = "; for more information, see https://www.elastic.co/docs/troubleshoot/elasticsearch/circuit-breaker-errors?version=9.5";

/// `CircuitBreakingException` as Elasticsearch renders it.
pub fn breaking_error(name: &str, label: &str, wanted: u64, limit: u64) -> AggError {
    AggError::new(
        429,
        "circuit_breaking_exception",
        format!(
            "[{name}] Data too large, data for [{label}] would be [{wanted}/{}], which is larger than the limit of [{limit}/{}]{}",
            byte_size(wanted),
            byte_size(limit),
            if crate::dialect::opensearch() { "" } else { CIRCUIT_BREAKER_DOCS }
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

    #[test]
    fn a_parent_bounds_its_children() {
        let node = MemoryBudget::new(1000);
        let (a, b) = (MemoryBudget::with_parent(u64::MAX, Some(node.clone())), MemoryBudget::with_parent(u64::MAX, Some(node.clone())));
        a.reserve(600, "x").unwrap();
        // Two hold something now: b's share is 500 and the node has 400 left.
        let e = b.reserve(600, "x").unwrap_err();
        assert!(e.reason.starts_with("[parent] Data too large, data for [x] would be [600/600b], which is larger than the limit of ["), "{}", e.reason);
        assert!(e.reason.contains("real usage: [0/0b], new bytes reserved: [600/600b]"), "{}", e.reason);
        assert_eq!((a.used(), b.used(), node.used(), b.parent_tripped()), (600, 0, 600, 1), "a refused reservation is held nowhere");
        b.reserve(400, "x").unwrap();
        a.release(600);
        assert_eq!((node.used(), node.active.load(Ordering::Relaxed)), (400, 1));
        b.release(400);
        assert_eq!((node.used(), node.active.load(Ordering::Relaxed)), (0, 0));
        // Over its share, a may only grow while an eighth of the node stays free for b.
        assert!(a.reserve(900, "x").is_err(), "alone, a still leaves an eighth");
        a.reserve(850, "x").unwrap();
        b.reserve(50, "x").unwrap();
        assert!(a.reserve(50, "x").is_err());
        a.release(850);
        b.release(50);
        // Memory held elsewhere counts against the parent's limit.
        node.set_held_elsewhere(900);
        assert!(a.reserve(200, "x").is_err());
        assert_eq!(node.used(), 0);
    }

    #[test]
    fn concurrent_reservations_leave_nothing_counted() {
        let node = MemoryBudget::new(10_000);
        let kids: Vec<_> = (0..4).map(|_| MemoryBudget::with_parent(u64::MAX, Some(node.clone()))).collect();
        std::thread::scope(|s| {
            for t in 0..8 {
                let kid = kids[t % 4].clone();
                s.spawn(move || {
                    for i in 0..20_000u64 {
                        // Some fit, some are refused (by the node, its share or the headroom).
                        let bytes = 1 + (i * 7919 + t as u64 * 104_729) % 4000;
                        if kid.reserve(bytes, "x").is_ok() {
                            kid.release(bytes);
                        }
                    }
                });
            }
        });
        assert_eq!((node.used(), node.active.load(Ordering::Relaxed)), (0, 0));
        assert!(kids.iter().all(|k| k.used() == 0));
    }
}
