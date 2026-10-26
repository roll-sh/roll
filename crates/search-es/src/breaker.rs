//! The node's `request` circuit breaker.
//!
//! Elasticsearch charges memory a request holds on to while it runs (aggregation buffers and
//! reduces, buffered `_msearch` responses) to the `request` breaker, whose limit is
//! `indices.breaker.request.limit` (60% of the heap by default, or an absolute size), and
//! refuses a reservation that would cross it with a `circuit_breaking_exception` (429). roll
//! keeps one [`MemoryBudget`] per cluster for this (search-aggs' trackers reserve from it and
//! release on drop) and re-reads the limit from the cluster settings whenever a request takes
//! the breaker, so a settings change applies to the next request, as Elasticsearch's dynamic
//! setting does. The "heap" is the node's memory budget ([`NodeMetrics::heap_max`]).

use std::sync::Arc;

use search_aggs::memory::MemoryBudget;

use crate::api::nodes::NodeMetrics;
use crate::cluster::Cluster;
use crate::meta::ClusterState;

/// The breakers of a node (only `request` accounts anything).
#[derive(Debug)]
pub struct Breakers {
    request: Arc<MemoryBudget>,
}

impl Default for Breakers {
    fn default() -> Self {
        Breakers { request: MemoryBudget::new(u64::MAX) }
    }
}

/// `indices.breaker.request.limit` against a heap of `heap` bytes: a percentage of it or an
/// absolute size (Elasticsearch's `memorySizeSetting`); `None` when the value is malformed
/// (validated when the setting is put, so only defaults reach here).
pub fn memory_size(value: &str, heap: i64) -> Option<u64> {
    let v = value.trim();
    if let Some(p) = v.strip_suffix('%') {
        let pct: f64 = p.trim().parse().ok()?;
        if !(0.0..=100.0).contains(&pct) {
            return None;
        }
        return Some((heap.max(0) as f64 * pct / 100.0) as u64);
    }
    let b = crate::units::ByteSize::parse(v, "indices.breaker.request.limit").ok()?.bytes();
    // `-1`: no limit.
    Some(if b < 0 { u64::MAX } else { b as u64 })
}

impl Breakers {
    /// The `request` breaker with its limit brought up to date.
    pub fn request(&self, cluster: &Cluster, st: &ClusterState) -> Arc<MemoryBudget> {
        let heap = NodeMetrics::sample(cluster).heap_max;
        let setting = st.metadata.cluster_setting("indices.breaker.request.limit").unwrap_or("60%");
        let limit = memory_size(setting, heap).or_else(|| memory_size("60%", heap)).unwrap_or(u64::MAX);
        self.request.set_limit(limit);
        self.request.clone()
    }

    /// The `request` breaker as it stands (node stats), without refreshing its limit.
    pub fn request_budget(&self) -> &Arc<MemoryBudget> {
        &self.request
    }
}

/// Bytes reserved on a breaker, given back when dropped (a request's accounting is released
/// when it finishes, however it finishes).
pub struct Reservation {
    budget: Arc<MemoryBudget>,
    bytes: u64,
}

impl Reservation {
    pub fn new(budget: Arc<MemoryBudget>) -> Reservation {
        Reservation { budget, bytes: 0 }
    }

    /// Reserves `bytes` more for `label`, or fails with Elasticsearch's
    /// `circuit_breaking_exception`.
    pub fn add(&mut self, bytes: u64, label: &str) -> crate::error::EsResult<()> {
        self.budget.reserve(bytes, label).map_err(crate::search::aggs::agg_error)?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

/// `TransportMultiSearchAction.estimateActualBytes`: what a buffered `_msearch` item costs
/// the coordinator, from its rendered response: a fixed shell, per hit a fixed overhead plus its
/// source, field entries and values, sort values and twice the serialized size of highlights and
/// explanations (inner hits recursively), the aggregations' serialized size, shard failures, and
/// twice the serialized size of suggestions and profiles.
pub fn estimate_search_response(v: &crate::json::Value) -> u64 {
    use crate::json::Value;
    const BASE_RESPONSE_OVERHEAD: u64 = 512;
    const PER_SHARD_FAILURE_OVERHEAD: u64 = 160;
    let mut buf = Vec::new();
    let mut bytes = BASE_RESPONSE_OVERHEAD;
    if let Some(Value::Arr(hits)) = v.get("hits").and_then(|h| h.get("hits")) {
        for h in hits {
            bytes += estimate_hit(h, &mut buf);
        }
    }
    if let Some(a) = v.get("aggregations") {
        bytes += serialized_len(a, &mut buf);
    }
    if let Some(Value::Arr(failures)) = v.get("_shards").and_then(|s| s.get("failures")) {
        for f in failures {
            bytes += PER_SHARD_FAILURE_OVERHEAD + estimate_failure(f);
        }
    }
    for k in ["suggest", "profile"] {
        if let Some(x) = v.get(k) {
            bytes += 2 * serialized_len(x, &mut buf);
        }
    }
    bytes
}

/// `estimateFailureBytes` for a failed `_msearch` item (its rendered `error`).
pub fn estimate_failure_item(error: &crate::json::Value) -> u64 {
    const BASE_FAILURE_ITEM_OVERHEAD: u64 = 64;
    BASE_FAILURE_ITEM_OVERHEAD + estimate_failure(error)
}

/// An exception's shell, stack and message (`estimateExceptionBytes`; roll keeps no stack, so a
/// typical depth stands in for it), with its causes.
fn estimate_failure(e: &crate::json::Value) -> u64 {
    const FRAMES: u64 = 32;
    const PER_STACK_FRAME_BYTES: u64 = 120;
    let mut bytes = 96 + 16 + FRAMES * 4 + FRAMES * PER_STACK_FRAME_BYTES;
    if let Some(r) = e.get("reason").and_then(crate::json::Value::as_str) {
        bytes += 32 + r.encode_utf16().count() as u64;
    }
    if let Some(c) = e.get("caused_by") {
        bytes += estimate_failure(c);
    }
    bytes
}

fn serialized_len(v: &crate::json::Value, buf: &mut Vec<u8>) -> u64 {
    buf.clear();
    crate::json::write::write_json(buf, v, false);
    buf.len() as u64
}

/// `estimateHitBytes`.
fn estimate_hit(h: &crate::json::Value, buf: &mut Vec<u8>) -> u64 {
    use crate::json::Value;
    const PER_HIT_OBJECT_OVERHEAD: u64 = 400;
    const PER_FIELD_OVERHEAD: u64 = 200;
    let mut bytes = PER_HIT_OBJECT_OVERHEAD;
    if let Some(src) = h.get("_source") {
        bytes += serialized_len(src, buf);
    }
    // Document fields (`fields`) and metadata fields (`_routing`, `_ignored`, …).
    if let Some(Value::Obj(fields)) = h.get("fields") {
        bytes += fields.len() as u64 * PER_FIELD_OVERHEAD;
        for v in fields.values() {
            match v {
                Value::Arr(a) => bytes += a.iter().map(value_bytes).sum::<u64>(),
                v => bytes += value_bytes(v),
            }
        }
    }
    for meta in ["_routing", "_ignored"] {
        if let Some(v) = h.get(meta) {
            bytes += PER_FIELD_OVERHEAD
                + match v {
                    Value::Arr(a) => a.iter().map(value_bytes).sum::<u64>(),
                    v => value_bytes(v),
                };
        }
    }
    if let Some(Value::Arr(sort)) = h.get("sort") {
        // Formatted and raw sort values both count.
        bytes += 64 + sort.len() as u64 * 8 + 2 * sort.iter().map(value_bytes).sum::<u64>();
    }
    for k in ["highlight", "_explanation"] {
        if let Some(x) = h.get(k) {
            bytes += 2 * serialized_len(x, buf);
        }
    }
    if let Some(Value::Obj(inner)) = h.get("inner_hits") {
        for ih in inner.values() {
            if let Some(Value::Arr(hits)) = ih.get("hits").and_then(|x| x.get("hits")) {
                for x in hits {
                    bytes += estimate_hit(x, buf);
                }
            }
        }
    }
    bytes
}

/// `estimateValueBytes`.
fn value_bytes(v: &crate::json::Value) -> u64 {
    use crate::json::Value;
    match v {
        Value::Str(s) => 32 + s.encode_utf16().count() as u64,
        Value::Num(_) => 24,
        _ => 32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits() {
        assert_eq!(memory_size("60%", 1000), Some(600));
        assert_eq!(memory_size("63000b", 1000), Some(63000));
        assert_eq!(memory_size("1kb", 0), Some(1024));
        assert_eq!(memory_size("-1", 10), Some(u64::MAX));
        assert_eq!(memory_size("101%", 10), None);
        assert_eq!(memory_size("x", 10), None);
    }
}
