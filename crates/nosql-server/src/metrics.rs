//! ScyllaDB's Prometheus endpoint (`GET /metrics`, port 9180) in single mode: the CQL
//! transport's counters, the default database's guardrail metrics and, when the server also
//! speaks the DynamoDB API, its Alternator metrics. Names and labels are ScyllaDB's (one
//! virtual shard, `shard="0"`), so tools and test suites that read them work unchanged.
//!
//! - `scylla_transport_cql_errors_total{shard,type}`: ERROR responses by code (only the types
//!   seen so far, as the reference lists them), protocol errors of connections that never
//!   reached a database included. The counters are process-wide.
//! - `scylla_transport_cql_client_timestamp_drift_histogram`: how far (µs) the client's
//!   timestamp of a QUERY / EXECUTE / BATCH is from the server clock when the request
//!   arrives, in ScyllaDB's buckets (4 per power of two from 512 µs to 32 s).
//! - `scylla_reactor_cpp_exceptions`: always 0 (roll has no C++ exceptions); suites check it
//!   doesn't grow.
//! - The database's `scylla_cql_*` guardrail and `scylla_database_large_*` counters
//!   (`nosql_engine::guardrails::Metrics`).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nosql_proto::error_body::code;

use crate::alternator::AlternatorOptions;
use crate::directory::Directory;

/// ScyllaDB's error type labels by protocol code.
const TYPES: &[(i32, &str)] = &[
    (code::SERVER_ERROR, "server_error"),
    (code::PROTOCOL_ERROR, "protocol_error"),
    (code::BAD_CREDENTIALS, "authentication_error"),
    (code::UNAVAILABLE, "unavailable"),
    (code::OVERLOADED, "overloaded"),
    (code::IS_BOOTSTRAPPING, "is_bootstrapping"),
    (code::TRUNCATE_ERROR, "truncate_error"),
    (code::WRITE_TIMEOUT, "write_timeout"),
    (code::READ_TIMEOUT, "read_timeout"),
    (code::READ_FAILURE, "read_failure"),
    (code::FUNCTION_FAILURE, "function_failure"),
    (code::WRITE_FAILURE, "write_failure"),
    (code::CDC_WRITE_FAILURE, "cdc_write_failure"),
    (code::CAS_WRITE_UNKNOWN, "cas_write_unknown"),
    (code::SYNTAX_ERROR, "syntax_error"),
    (code::UNAUTHORIZED, "unauthorized"),
    (code::INVALID, "invalid"),
    (code::CONFIG_ERROR, "config_error"),
    (code::ALREADY_EXISTS, "already_exists"),
    (code::UNPREPARED, "unprepared"),
    (code::SCYLLA_RATE_LIMIT, "rate_limit_error"),
];

/// Bucket bounds (µs): 4 linear steps per power of two, 640 … 33554432.
fn bounds() -> impl Iterator<Item = u64> {
    (9..25).flat_map(|p| (1..=4).map(move |i| (1u64 << p) + i * (1u64 << (p - 2))))
}

const BUCKETS: usize = 16 * 4;

/// The transport counters (process-wide).
pub struct Transport {
    errors: [AtomicU64; TYPES.len()],
    drift_sum: AtomicU64,
    drift_count: AtomicU64,
    drift: [AtomicU64; BUCKETS],
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

static TRANSPORT: Transport = Transport {
    errors: [ZERO; TYPES.len()],
    drift_sum: ZERO,
    drift_count: ZERO,
    drift: [ZERO; BUCKETS],
};

/// Counts an ERROR response.
pub fn count_error(error_code: i32) {
    if let Some(i) = TYPES.iter().position(|(c, _)| *c == error_code) {
        TRANSPORT.errors[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// Records the drift of a request's client timestamp (µs since the epoch).
pub fn observe_client_timestamp(client_micros: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);
    let drift = now.saturating_sub(client_micros).unsigned_abs();
    let t = &TRANSPORT;
    t.drift_sum.fetch_add(drift, Ordering::Relaxed);
    t.drift_count.fetch_add(1, Ordering::Relaxed);
    if let Some(i) = bounds().position(|b| drift < b) {
        t.drift[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// The transport metrics in the Prometheus text format.
pub fn render_transport(out: &mut String) {
    let t = &TRANSPORT;
    let name = "scylla_transport_cql_errors_total";
    let _ = writeln!(out, "# HELP {name} Counts the total number of returned CQL errors.\n# TYPE {name} counter");
    for (i, (_, ty)) in TYPES.iter().enumerate() {
        let n = t.errors[i].load(Ordering::Relaxed);
        if n > 0 {
            let _ = writeln!(out, "{name}{{shard=\"0\",type=\"{ty}\"}} {n}");
        }
    }
    let name = "scylla_transport_cql_client_timestamp_drift_histogram";
    let _ = writeln!(
        out,
        "# HELP {name} A histogram of the drift in microseconds between the client-provided CQL timestamp and the server time at request arrival. Useful for detecting transport delays before requests reach the CQL server.\n# TYPE {name} histogram"
    );
    let label = "scheduling_group_name=\"sl:default\"";
    let _ = writeln!(out, "{name}_sum{{{label}}} {}", t.drift_sum.load(Ordering::Relaxed));
    let _ = writeln!(out, "{name}_count{{{label}}} {}", t.drift_count.load(Ordering::Relaxed));
    let mut cumulative = 0;
    for (i, b) in bounds().enumerate() {
        cumulative += t.drift[i].load(Ordering::Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"{b}.000000\",{label}}} {cumulative}");
    }
    let name = "scylla_reactor_cpp_exceptions";
    let _ = writeln!(out, "# HELP {name} Total number of C++ exceptions\n# TYPE {name} counter\n{name}{{shard=\"0\"}} 0");
}

/// Everything `GET /metrics` shows for the directory's default database.
pub async fn render(dir: &Arc<dyn Directory>, alternator: Option<&AlternatorOptions>) -> String {
    let mut out = String::new();
    render_transport(&mut out);
    if let Some(spec) = dir.route_sni(None).await
        && let Ok(l) = dir.open(&spec).await
    {
        l.db.guardrail_metrics().render(&mut out);
        // WASM UDF instances kept for functions that exist (ScyllaDB's spelling).
        let name = "scylla_user_functions_cache_instace_count_any";
        let n = nosql_engine::udf::wasm::instance_count(&l.db.schema());
        let _ = writeln!(out, "# TYPE {name} gauge\n{name}{{shard=\"0\"}} {n}");
        render_memory(&mut out, l.db.store());
        // One expiration service: CQL per-row TTL and Alternator TTL share its counters.
        let mut expiration: Vec<(&str, f64)> =
            l.db.expiration_metrics().counters().iter().map(|(n, v)| (*n, *v as f64)).collect();
        if let Some(opts) = alternator {
            let alt = crate::alternator::alternator_of(&l, opts).metrics.render();
            out.push_str(&take_expiration(&alt, &mut expiration));
        }
        for (name, v) in expiration {
            let _ = writeln!(out, "# TYPE {name} counter\n{name}{{shard=\"0\"}} {v}");
        }
    }
    out
}

/// The node's memory under ScyllaDB's names: memtables (`dirty` memory), writes blocked on it,
/// and the block cache (ScyllaDB's row cache).
fn render_memory(out: &mut String, db: &nosql_storage::db::Database) {
    let budget = db.memory().node();
    let blocked = &budget.metrics;
    let cache = db.cache();
    let gauges = [
        ("scylla_memory_dirty_bytes", "Holds the current size of all (\"regular\", \"system\" and \"streaming\") non-free memory in bytes: used memory + released memory that hasn't been returned to a free memory pool yet.", budget.used()),
        ("scylla_memory_regular_dirty_bytes", "Holds the current size of a all non-free memory in bytes: used memory + released memory that hasn't been returned to a free memory pool yet.", budget.used()),
        ("scylla_database_requests_blocked_memory_current", "Holds the current number of requests blocked due to reaching the memory quota.", blocked.blocked_now.load(Ordering::Relaxed)),
        ("scylla_cache_bytes_used", "current bytes used by the cache out of the total size of memory", cache.memory_bytes()),
        ("scylla_cache_bytes_total", "total size of memory for the cache", cache.memory_budget()),
    ];
    for (name, help, v) in gauges {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name}{{shard=\"0\"}} {v}");
    }
    let name = "scylla_database_requests_blocked_memory";
    let _ = writeln!(
        out,
        "# HELP {name} Holds the current number of requests blocked due to reaching the memory quota.\n# TYPE {name} counter\n{name}{{shard=\"0\"}} {}",
        blocked.blocked_writes.load(Ordering::Relaxed)
    );
}

/// `text` without its `scylla_expiration_*` counters, whose values are added to `into`.
fn take_expiration(text: &str, into: &mut [(&str, f64)]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let name = line.strip_prefix("# TYPE ").unwrap_or(line).split(['{', ' ']).next().unwrap_or("");
        if let Some((_, total)) = into.iter_mut().find(|(n, _)| *n == name) {
            if !line.starts_with('#') {
                *total += line.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The `/metrics` endpoint; with `alternator`, the Alternator metrics too.
pub fn router(dir: Arc<dyn Directory>, alternator: Option<AlternatorOptions>) -> axum::Router {
    let state = Arc::new((dir, alternator));
    axum::Router::new().route(
        "/metrics",
        axum::routing::get(move || {
            let state = state.clone();
            async move { render(&state.0, state.1.as_ref()).await }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_and_render() {
        let b: Vec<u64> = bounds().collect();
        assert_eq!(&b[..5], &[640, 768, 896, 1024, 1280]);
        assert_eq!(*b.last().unwrap(), 33_554_432);
        count_error(code::PROTOCOL_ERROR);
        observe_client_timestamp(0);
        let mut s = String::new();
        render_transport(&mut s);
        assert!(s.lines().any(|l| l.starts_with("scylla_transport_cql_errors_total{shard=\"0\",type=\"protocol_error\"} ")));
        assert!(s.contains("scylla_transport_cql_client_timestamp_drift_histogram_count{scheduling_group_name=\"sl:default\"} "));
        assert!(s.contains("scylla_reactor_cpp_exceptions{shard=\"0\"} 0\n"));
    }

    #[test]
    fn expiration_counters_merge() {
        let mut into = [("scylla_expiration_scan_passes", 2.0), ("scylla_expiration_items_deleted", 0.0)];
        let alt = "# TYPE scylla_alternator_x counter\nscylla_alternator_x{shard=\"0\"} 1\n# TYPE scylla_expiration_scan_passes counter\nscylla_expiration_scan_passes{shard=\"0\"} 3\n";
        assert_eq!(take_expiration(alt, &mut into), "# TYPE scylla_alternator_x counter\nscylla_alternator_x{shard=\"0\"} 1\n");
        assert_eq!(into[0].1, 5.0);
    }
}
