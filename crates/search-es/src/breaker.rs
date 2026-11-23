//! The node's circuit breakers: `request`, `inflight_requests` and `parent`.
//!
//! Elasticsearch charges memory a request holds on to while it runs (aggregation buffers and
//! reduces, buffered `_msearch` responses) to the `request` breaker, whose limit is
//! `indices.breaker.request.limit` (60% of the heap by default, or an absolute size), and
//! refuses a reservation that would cross it with a `circuit_breaking_exception` (429). roll
//! keeps one [`MemoryBudget`] per cluster for this (search-aggs' trackers reserve from it and
//! release on drop) and re-reads the limit from the cluster settings whenever a request takes
//! the breaker, so a settings change applies to the next request, as Elasticsearch's dynamic
//! setting does. The "heap" is the node's memory budget ([`NodeMetrics::heap_max`]).
//!
//! Every REST request's body is charged to `inflight_requests` until it is answered
//! (`RestController`: `network.breaker.inflight_requests.limit`, 100% of the heap, overhead 2),
//! except on the handlers Elasticsearch lets through whatever the memory (health, node and
//! cluster stats, cluster state and settings, task listing and cancelling). Each charge is
//! then checked against the `parent` breaker (`indices.breaker.total.limit`, 95%). The parent
//! sums what the children hold, with their overheads, where Elasticsearch measures the real
//! heap: roll has no garbage-collected heap whose usage would mean the same, so only memory
//! roll accounts can trip it.
//!
//! A tenant can raise `indices.breaker.request.limit` (to `-1` even), so on a node serving many
//! clusters it can't be the only bound. Two bounds only the operator sets hold whatever it says:
//! - the `request` breaker's budget is a child of the node's budget
//!   ([`crate::ClusterConfig::request_memory`]), so everything charged to it (aggregations'
//!   trackers and reduces, buffered `_msearch` responses, ES|QL's pages and tables, snapshot
//!   transfers) is charged to the node's too, and refused when either would go over;
//! - its effective limit is the setting's, at most [`REQUEST_MAX_LIMIT`] (by default a quarter
//!   of the node's budget when there is one). The setting itself reads and writes as on
//!   Elasticsearch; node stats show the effective limit.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use search_aggs::memory::MemoryBudget;

use crate::api::nodes::NodeMetrics;
use crate::cluster::Cluster;
use crate::meta::ClusterState;

/// The breakers of a node.
#[derive(Debug)]
pub struct Breakers {
    request: Arc<MemoryBudget>,
    /// The node's budget, shared by every cluster it serves (`None`: none); `request`'s parent.
    node: Option<Arc<MemoryBudget>>,
    /// The most `request`'s limit may be ([`REQUEST_MAX_LIMIT`]).
    max_limit: u64,
    /// Bytes of the request bodies being handled.
    in_flight: AtomicU64,
    in_flight_tripped: AtomicU64,
    parent_tripped: AtomicU64,
    /// The heap limits are relative to, and when it was sampled (ms; refreshed each second,
    /// so charging a request body costs no metrics sample).
    heap: std::sync::atomic::AtomicI64,
    heap_at: std::sync::atomic::AtomicI64,
}

impl Default for Breakers {
    fn default() -> Self {
        Breakers {
            request: MemoryBudget::new(u64::MAX),
            node: None,
            max_limit: u64::MAX,
            in_flight: AtomicU64::new(0),
            in_flight_tripped: AtomicU64::new(0),
            parent_tripped: AtomicU64::new(0),
            heap: std::sync::atomic::AtomicI64::new(0),
            heap_at: std::sync::atomic::AtomicI64::new(i64::MIN),
        }
    }
}

/// Node setting: the most one cluster's `request` breaker may allow, whatever
/// `indices.breaker.request.limit` says: a byte size, or a percentage of the node's budget for
/// requests; `-1`: no maximum. Default: `25%` on a node with a budget, none otherwise.
pub const REQUEST_MAX_LIMIT: &str = "roll.breaker.request.max_limit";

/// A [`REQUEST_MAX_LIMIT`] value against a node budget of `of` bytes: `None` for `-1` (no
/// maximum), else the bytes; a percentage must be above 0 and at most 100, a size above 0.
pub fn request_max_limit(value: &str, of: i64) -> Result<Option<u64>, String> {
    let v = value.trim();
    if v == "-1" {
        return Ok(None);
    }
    if let Some(p) = v.strip_suffix('%') {
        let pct: f64 = p.trim().parse().map_err(|_| format!("[{REQUEST_MAX_LIMIT}] must be a byte size, a percentage or -1, got [{value}]"))?;
        if !(pct > 0.0 && pct <= 100.0) {
            return Err(format!("[{REQUEST_MAX_LIMIT}] must be a percentage above 0 and at most 100, got [{value}]"));
        }
        return Ok(Some((of.max(0) as f64 * pct / 100.0) as u64));
    }
    let b = crate::units::ByteSize::parse(v, REQUEST_MAX_LIMIT).map_err(|_| format!("[{REQUEST_MAX_LIMIT}] must be a byte size, a percentage or -1, got [{value}]"))?.bytes();
    if b <= 0 {
        return Err(format!("[{REQUEST_MAX_LIMIT}] must be above 0 (or -1 for no maximum), got [{value}]"));
    }
    Ok(Some(b as u64))
}

/// The overhead `inflight_requests` multiplies its bytes by (`network.breaker.inflight_requests.overhead`).
const IN_FLIGHT_OVERHEAD: f64 = 2.0;

/// The label Elasticsearch charges request bodies under.
const HTTP_REQUEST: &str = "<http_request>";

/// A limit setting against the heap, `-1` meaning none (`ChildMemoryCircuitBreaker`).
fn limit_of(st: &ClusterState, key: &str, default: &str, heap: i64) -> i64 {
    let v = st.metadata.cluster_setting(key).unwrap_or(default);
    match memory_size(v, heap).or_else(|| memory_size(default, heap)) {
        Some(u64::MAX) | None => -1,
        Some(b) => b.min(i64::MAX as u64) as i64,
    }
}

/// `CircuitBreakingException` (429, `TRANSIENT`) with Elasticsearch's suffix.
fn breaking(reason: String, wanted: i64, limit: i64) -> crate::error::EsError {
    let suffix = if crate::opensearch::active() { "" } else { search_aggs::memory::CIRCUIT_BREAKER_DOCS };
    crate::error::EsError::new(429, "circuit_breaking_exception", format!("{reason}{suffix}"))
        .with_meta("bytes_wanted", crate::json::Value::from(wanted))
        .with_meta("bytes_limit", crate::json::Value::from(limit))
        .with_meta("durability", crate::json::Value::from("TRANSIENT"))
}

/// `[name/human]` as breaker messages print sizes.
fn sized(bytes: i64) -> String {
    format!("{bytes}/{}", crate::units::ByteSize(bytes).human())
}

/// A request body charged to `inflight_requests`, released when dropped (the response sent).
pub struct InFlight<'a> {
    breakers: &'a Breakers,
    bytes: u64,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.breakers.in_flight.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// Handlers Elasticsearch never refuses for memory (`canTripCircuitBreaker() == false`), so a
/// node under memory pressure can still be watched and fixed (checked against 9.5.5).
pub fn can_trip(api: crate::routes::Api) -> bool {
    use crate::routes::Api;
    !matches!(
        api,
        Api::Info
            | Api::Ping
            | Api::ClusterHealth
            | Api::NodesInfo
            | Api::NodesStats
            | Api::NodesUsage
            | Api::NodesHotThreads
            | Api::ClusterStats
            | Api::ClusterState
            | Api::ClusterGetSettings
            | Api::ClusterPutSettings
            | Api::TasksList
            | Api::TasksCancel
            | Api::IndicesStats
            | Api::HealthReport
            | Api::ClusterAllocationExplain
            | Api::ClusterRemoteInfo
            | Api::ClusterPostVotingConfigExclusions
            | Api::ClusterDeleteVotingConfigExclusions
    )
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
    /// Breakers whose `request` budget also charges the node's budget `node`, its limit held to
    /// [`REQUEST_MAX_LIMIT`] (`max`, the node setting's value when set).
    pub fn with_node(node: Option<Arc<MemoryBudget>>, max: Option<&str>) -> Breakers {
        let max_limit = match &node {
            Some(n) => {
                let of = n.limit().min(i64::MAX as u64) as i64;
                // rolld refuses a bad value at startup; one given otherwise (`-E`) falls back.
                let parsed = max.map(|v| request_max_limit(v, of)).unwrap_or(Ok(Some((of.max(0) / 4) as u64)));
                parsed.unwrap_or(Some((of.max(0) / 4) as u64))
            }
            // A cluster alone on its node: only an absolute maximum means anything.
            None => max.filter(|v| !v.trim().ends_with('%')).and_then(|v| request_max_limit(v, 0).ok().flatten()),
        }
        .unwrap_or(u64::MAX);
        Breakers { request: MemoryBudget::with_parent(u64::MAX, node.clone()), node, max_limit, ..Breakers::default() }
    }

    /// The node's budget for what requests hold, when the node has one.
    pub fn node(&self) -> Option<&Arc<MemoryBudget>> {
        self.node.as_ref()
    }

    /// A node's budget for what requests hold: `limit` bytes, the operator's memory budget
    /// for search, or else half of the host's memory.
    pub fn node_budget(limit: Option<u64>) -> Arc<MemoryBudget> {
        let host = crate::api::nodes::NodeMetrics::host_memory();
        MemoryBudget::new(limit.unwrap_or(if host > 0 { host as u64 / 2 } else { u64::MAX }))
    }

    /// The `request` breaker with its limit brought up to date: the setting's, held to the
    /// operator's maximum ([`REQUEST_MAX_LIMIT`]).
    pub fn request(&self, cluster: &Cluster, st: &ClusterState) -> Arc<MemoryBudget> {
        let heap = self.heap(cluster);
        let setting = st.metadata.cluster_setting("indices.breaker.request.limit").unwrap_or("60%");
        let limit = memory_size(setting, heap).or_else(|| memory_size("60%", heap)).unwrap_or(u64::MAX);
        self.request.set_limit(limit.min(self.max_limit));
        self.request.clone()
    }

    /// The `request` breaker as it stands (node stats), without refreshing its limit.
    pub fn request_budget(&self) -> &Arc<MemoryBudget> {
        &self.request
    }

    /// Charges a request body of `bytes` to `inflight_requests` until the guard drops
    /// (`addEstimateBytesAndMaybeBreak(contentLength, "<http_request>")`), then checks the
    /// parent; either refusal is Elasticsearch's 429 with its text and byte counts.
    pub fn charge_in_flight(&self, cluster: &Cluster, st: &ClusterState, bytes: u64) -> crate::error::EsResult<InFlight<'_>> {
        let heap = self.heap(cluster);
        let limit = limit_of(st, "network.breaker.inflight_requests.limit", "100%", heap);
        let trip = |wanted: i64| {
            self.in_flight_tripped.fetch_add(1, Ordering::Relaxed);
            breaking(format!("[inflight_requests] Data too large, data for [{HTTP_REQUEST}] would be [{}], which is larger than the limit of [{}]", sized(wanted), sized(limit)), wanted, limit)
        };
        if limit == 0 {
            return Err(trip(bytes as i64));
        }
        let new = self.in_flight.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let guard = InFlight { breakers: self, bytes };
        let with_overhead = (new as f64 * IN_FLIGHT_OVERHEAD) as i64;
        if limit > 0 && with_overhead > limit {
            return Err(trip(with_overhead));
        }
        self.check_parent(st, (bytes as f64 * IN_FLIGHT_OVERHEAD) as i64, HTTP_REQUEST, heap)?;
        Ok(guard)
    }

    /// The node's heap as of at most a second ago.
    fn heap(&self, cluster: &Cluster) -> i64 {
        let now = crate::time::now_millis();
        if now.saturating_sub(self.heap_at.load(Ordering::Relaxed)) >= 1000 {
            self.heap.store(NodeMetrics::sample(cluster).heap_max, Ordering::Relaxed);
            self.heap_at.store(now, Ordering::Relaxed);
        }
        self.heap.load(Ordering::Relaxed)
    }

    /// What each child holds with its overhead, in the order Elasticsearch lists them.
    fn usages(&self) -> [(&'static str, i64); 5] {
        [
            ("fielddata", 0),
            ("request", self.request.used().min(i64::MAX as u64) as i64),
            ("inflight_requests", (self.in_flight.load(Ordering::Relaxed) as f64 * IN_FLIGHT_OVERHEAD) as i64),
            ("model_inference", 0),
            ("eql_sequence", 0),
        ]
    }

    /// `HierarchyCircuitBreakerService.checkParentLimit` for `new_bytes` just reserved
    /// (already in a child's usage).
    fn check_parent(&self, st: &ClusterState, new_bytes: i64, label: &str, heap: i64) -> crate::error::EsResult<()> {
        let limit = limit_of(st, "indices.breaker.total.limit", "95%", heap);
        let usages = self.usages();
        let total: i64 = usages.iter().map(|(_, b)| b).sum();
        if limit < 0 || total <= limit {
            return Ok(());
        }
        self.parent_tripped.fetch_add(1, Ordering::Relaxed);
        let list: Vec<String> = usages.iter().map(|(n, b)| format!("{n}={}", sized(*b))).collect();
        Err(breaking(
            format!(
                "[parent] Data too large, data for [{label}] would be [{}], which is larger than the limit of [{}], real usage: [{}], new bytes reserved: [{}], usages [{}]",
                sized(total),
                sized(limit),
                sized(total - new_bytes),
                sized(new_bytes),
                list.join(", ")
            ),
            total,
            limit,
        ))
    }

    /// `(estimated bytes, tripped)` of `inflight_requests` and of the parent (node stats).
    pub fn in_flight_stats(&self) -> ((i64, i64), (i64, i64)) {
        let parent: i64 = self.usages().iter().map(|(_, b)| b).sum();
        (
            (self.in_flight.load(Ordering::Relaxed) as i64, self.in_flight_tripped.load(Ordering::Relaxed) as i64),
            // The node's budget refusing a request counts as Elasticsearch's parent breaker.
            (parent, (self.parent_tripped.load(Ordering::Relaxed) + self.request.parent_tripped()) as i64),
        )
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

/// What a JSON tree holds in memory (nodes, string capacities, map entries): the real size of
/// a kept response, where [`estimate_search_response`] is Elasticsearch's estimate of one.
pub fn json_bytes(v: &crate::json::Value) -> usize {
    use crate::json::Value;
    size_of::<Value>()
        + match v {
            Value::Str(s) => s.capacity(),
            Value::Arr(a) => (a.capacity().saturating_sub(a.len())) * size_of::<Value>() + a.iter().map(json_bytes).sum::<usize>(),
            // An index map: its entries (key, value, hash) and its index table.
            Value::Obj(m) => m.capacity() * (size_of::<String>() + size_of::<u64>() + size_of::<usize>()) + m.iter().map(|(k, v)| k.capacity() + json_bytes(v)).sum::<usize>(),
            _ => 0,
        }
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
pub(crate) fn estimate_hit(h: &crate::json::Value, buf: &mut Vec<u8>) -> u64 {
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
        assert_eq!(request_max_limit("25%", 400), Ok(Some(100)));
        assert_eq!(request_max_limit("1kb", 400), Ok(Some(1024)));
        assert_eq!(request_max_limit("-1", 400), Ok(None));
        for bad in ["0", "0%", "101%", "25 %x", "lots", "-2"] {
            assert!(request_max_limit(bad, 400).is_err(), "{bad}");
        }
    }
}
