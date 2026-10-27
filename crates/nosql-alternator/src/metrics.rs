//! Alternator's metrics, named and labelled as ScyllaDB exports them on its Prometheus port
//! (`docs/alternator/compatibility.md`'s metrics table, `test/alternator/test_metrics.py`):
//!
//! - per operation (`op`): `scylla_alternator_operation`, the `op_latency` histogram (successful
//!   requests), `rcu_total` / `wcu_total`, `batch_item_count` (and its histogram),
//!   `returned_items` (and its histogram), `operation_size_kb` (a histogram of the item sizes an
//!   operation read or wrote), `reads_before_write`, `write_using_lwt`,
//!   `conditional_check_failed`, `returned_records` (GetRecords);
//! - the same per table (`scylla_alternator_table_*`, labelled `cf` and `ks`);
//! - `total_operations`, `unsupported_operations`, `user_errors` (400s but failed conditions),
//!   `system_errors` (500s), `authentication_failures`, `authorization_failures`;
//! - the TTL scanner's `scylla_expiration_scan_passes`, `scan_table`, `items_deleted`;
//! - the storage requests behind them: `scylla_storage_proxy_coordinator_{read,write}_latency`
//!   (labelled with the role's `scheduling_group_name`, `sl:<service level>`) and per table
//!   `scylla_column_family_{read,write}_latency`;
//! - the expression cache's `scylla_alternator_expression_cache_{hits,misses,evictions}`
//!   ([`crate::expr::cache`]).
//!
//! An operation records what it did in its request's [`RequestStats`]; [`Metrics::finish`]
//! folds that into the counters once the operation is over.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use parking_lot::Mutex;

/// Bucket bounds of ScyllaDB's `estimated_histogram_with_max<max>` (an
/// `approx_exponential_histogram<1, max, 4>`): four buckets per power of two, as Prometheus `le`
/// bounds (deduplicated), `+Inf` excluded.
pub fn expo_buckets(max: u64) -> Vec<f64> {
    const PRECISION: u32 = 2;
    let per_range = 1u64 << PRECISION;
    let (shift, scaled_min, scaled_max) = (PRECISION, 1u64 << PRECISION, max << PRECISION);
    let ranges = (scaled_max / scaled_min).ilog2() as u64;
    let n = ranges * per_range + 1;
    let base_shift = scaled_min.ilog2() as u64;
    let lower = |id: u64| -> u64 {
        if id == n - 1 {
            return max;
        }
        let range = id >> PRECISION;
        let limit = (scaled_min << range)
            + ((id & (per_range - 1)) << (range + base_shift - u64::from(PRECISION)));
        limit >> shift
    };
    let mut out: Vec<f64> = Vec::new();
    for i in 0..n - 1 {
        let b = lower(i + 1) as f64;
        if out.last() != Some(&b) {
            out.push(b);
        }
    }
    out
}

/// Which bucket layout a histogram uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buckets {
    /// Microseconds.
    Latency,
    /// Item counts.
    Items,
    /// Kilobytes.
    SizeKb,
}

impl Buckets {
    fn bounds(self) -> &'static [f64] {
        use std::sync::OnceLock;
        static LATENCY: OnceLock<Vec<f64>> = OnceLock::new();
        static ITEMS: OnceLock<Vec<f64>> = OnceLock::new();
        static SIZE: OnceLock<Vec<f64>> = OnceLock::new();
        match self {
            Buckets::Latency => LATENCY.get_or_init(|| expo_buckets(1 << 25)),
            Buckets::Items => ITEMS.get_or_init(|| expo_buckets(1 << 12)),
            Buckets::SizeKb => SIZE.get_or_init(|| expo_buckets(512)),
        }
    }
}

#[derive(Clone, Debug)]
struct Histogram {
    buckets: Buckets,
    /// Per bucket, not cumulative; the last one is `+Inf`.
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    fn new(buckets: Buckets) -> Histogram {
        Histogram {
            buckets,
            counts: vec![0; buckets.bounds().len() + 1],
            sum: 0.0,
            count: 0,
        }
    }

    fn observe(&mut self, v: f64) {
        let i = self.buckets.bounds().partition_point(|b| *b < v);
        self.counts[i] += 1;
        self.sum += v;
        self.count += 1;
    }
}

type Labels = Vec<(&'static str, String)>;

#[derive(Default)]
struct Registry {
    counters: BTreeMap<(&'static str, Labels), f64>,
    histograms: BTreeMap<(&'static str, Labels), Histogram>,
}

fn labels(l: &[(&'static str, &str)]) -> Labels {
    l.iter().map(|(k, v)| (*k, v.to_string())).collect()
}

/// A database's Alternator metrics.
#[derive(Default)]
pub struct Metrics {
    r: Mutex<Registry>,
}

/// What one request did, for its metrics.
#[derive(Default)]
pub struct RequestStats(Mutex<Stats>);

#[derive(Default)]
struct Stats {
    /// Tables the operation was on (in order, once each).
    tables: Vec<String>,
    rcu: Vec<(String, f64)>,
    /// With the operation they are counted under, when not the request's (a batch's puts are
    /// PutItem's).
    wcu: Vec<(Option<&'static str>, String, f64)>,
    /// Items per table of a batch.
    batch_items: Vec<(String, u64)>,
    returned_items: Vec<(String, u64)>,
    returned_records: Vec<(String, u64)>,
    /// Item sizes read or written, in KB.
    sizes: Vec<(String, f64)>,
    reads_before_write: Vec<String>,
    lwt: Vec<String>,
    condition_failed: Vec<String>,
    storage_reads: Vec<String>,
    storage_writes: Vec<String>,
    authorization_failures: u64,
    /// The scheduling group of the request's role (`sl:default` when unset).
    scheduling_group: Option<String>,
}

impl RequestStats {
    fn with(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.0.lock());
    }

    /// The operation is on `table`.
    pub fn table(&self, table: &str) {
        self.with(|s| {
            if !s.tables.iter().any(|t| t == table) {
                s.tables.push(table.to_string());
            }
        });
    }

    pub fn rcu(&self, table: &str, units: f64) {
        self.with(|s| s.rcu.push((table.to_string(), units)));
    }

    pub fn wcu(&self, table: &str, units: f64) {
        self.with(|s| s.wcu.push((None, table.to_string(), units)));
    }

    /// Write units counted under operation `op` (BatchWriteItem's puts are PutItem's).
    pub fn wcu_as(&self, op: &'static str, table: &str, units: f64) {
        self.with(|s| s.wcu.push((Some(op), table.to_string(), units)));
    }

    pub fn batch_items(&self, table: &str, n: u64) {
        self.with(|s| s.batch_items.push((table.to_string(), n)));
    }

    pub fn returned_items(&self, table: &str, n: u64) {
        self.with(|s| s.returned_items.push((table.to_string(), n)));
    }

    pub fn returned_records(&self, table: &str, n: u64) {
        self.with(|s| s.returned_records.push((table.to_string(), n)));
    }

    /// An item of `bytes` the operation read or wrote.
    pub fn item_size(&self, table: &str, bytes: usize) {
        self.with(|s| s.sizes.push((table.to_string(), bytes as f64 / 1024.0)));
    }

    /// A write: whether it read the item first and whether it was an LWT.
    pub fn write(&self, table: &str, read_before: bool, lwt: bool) {
        self.with(|s| {
            if read_before {
                s.reads_before_write.push(table.to_string());
            }
            if lwt {
                s.lwt.push(table.to_string());
            }
        });
    }

    pub fn condition_failed(&self, table: &str) {
        self.with(|s| s.condition_failed.push(table.to_string()));
    }

    /// A read (`write` false) or write of the table's storage.
    pub fn storage(&self, table: &str, write: bool) {
        self.with(|s| {
            if write {
                s.storage_writes.push(table.to_string())
            } else {
                s.storage_reads.push(table.to_string())
            }
        });
    }

    pub fn authorization_failure(&self) {
        self.with(|s| s.authorization_failures += 1);
    }

    /// Whether the request read or wrote a table's storage.
    pub fn uses_storage(&self) -> bool {
        let s = self.0.lock();
        !s.storage_reads.is_empty() || !s.storage_writes.is_empty()
    }

    /// Whether a write of the request was an LWT.
    pub fn used_lwt(&self) -> bool {
        !self.0.lock().lwt.is_empty()
    }

    /// The scheduling group (`sl:<service level>`) labelling the storage latencies.
    pub fn set_scheduling_group(&self, group: String) {
        self.with(|s| s.scheduling_group = Some(group));
    }
}

/// The CQL keyspace label of a table.
fn ks(table: &str) -> String {
    crate::storage::keyspace_name(table)
}

impl Metrics {
    pub fn add(&self, name: &'static str, l: &[(&'static str, &str)], v: f64) {
        *self.r.lock().counters.entry((name, labels(l))).or_default() += v;
    }

    pub fn observe(&self, name: &'static str, l: &[(&'static str, &str)], b: Buckets, v: f64) {
        self.r
            .lock()
            .histograms
            .entry((name, labels(l)))
            .or_insert_with(|| Histogram::new(b))
            .observe(v);
    }

    /// Folds a finished request into the counters. `op` is `None` for an unknown operation;
    /// `status` is the HTTP status of the answer.
    pub fn finish(
        &self,
        op: Option<&str>,
        stats: &RequestStats,
        status: u16,
        condition_failed: bool,
        micros: f64,
    ) {
        let s = std::mem::take(&mut *stats.0.lock());
        self.add("scylla_alternator_total_operations", &[], 1.0);
        let Some(op) = op else {
            self.add("scylla_alternator_unsupported_operations", &[], 1.0);
            if status >= 500 {
                self.add("scylla_alternator_system_errors", &[], 1.0);
            }
            return;
        };
        let o = [("op", op)];
        self.add("scylla_alternator_operation", &o, 1.0);
        if status >= 500 {
            self.add("scylla_alternator_system_errors", &[], 1.0);
        } else if status >= 400 && !condition_failed {
            self.add("scylla_alternator_user_errors", &[], 1.0);
        }
        if s.authorization_failures > 0 {
            self.add(
                "scylla_alternator_authorization_failures",
                &[],
                s.authorization_failures as f64,
            );
        }
        let ok = status < 400;
        if ok {
            self.observe("scylla_alternator_op_latency", &o, Buckets::Latency, micros);
        }
        for t in &s.tables {
            let k = ks(t);
            let tl = [("op", op), ("cf", t.as_str()), ("ks", k.as_str())];
            self.add("scylla_alternator_table_operation", &tl, 1.0);
            if ok {
                self.observe(
                    "scylla_alternator_table_op_latency",
                    &tl,
                    Buckets::Latency,
                    micros,
                );
            }
        }
        for (t, u) in &s.rcu {
            let k = ks(t);
            self.add("scylla_alternator_rcu_total", &o, *u);
            self.add(
                "scylla_alternator_table_rcu_total",
                &[("op", op), ("cf", t.as_str()), ("ks", k.as_str())],
                *u,
            );
        }
        for (as_op, t, u) in &s.wcu {
            let k = ks(t);
            let op = as_op.unwrap_or(op);
            self.add("scylla_alternator_wcu_total", &[("op", op)], *u);
            self.add(
                "scylla_alternator_table_wcu_total",
                &[("op", op), ("cf", t.as_str()), ("ks", k.as_str())],
                *u,
            );
        }
        if !s.batch_items.is_empty() {
            let total: u64 = s.batch_items.iter().map(|(_, n)| n).sum();
            self.add("scylla_alternator_batch_item_count", &o, total as f64);
            self.observe(
                "scylla_alternator_batch_item_count_histogram",
                &o,
                Buckets::Items,
                total as f64,
            );
            for (t, n) in &s.batch_items {
                let k = ks(t);
                let tl = [("op", op), ("cf", t.as_str()), ("ks", k.as_str())];
                self.add("scylla_alternator_table_batch_item_count", &tl, *n as f64);
                self.observe(
                    "scylla_alternator_table_batch_item_count_histogram",
                    &tl,
                    Buckets::Items,
                    *n as f64,
                );
            }
        }
        for (t, n) in &s.returned_items {
            let k = ks(t);
            let tl = [("op", op), ("cf", t.as_str()), ("ks", k.as_str())];
            self.add("scylla_alternator_returned_items", &o, *n as f64);
            self.observe(
                "scylla_alternator_returned_items_histogram",
                &o,
                Buckets::Items,
                *n as f64,
            );
            self.add("scylla_alternator_table_returned_items", &tl, *n as f64);
            self.observe(
                "scylla_alternator_table_returned_items_histogram",
                &tl,
                Buckets::Items,
                *n as f64,
            );
        }
        for (t, n) in &s.returned_records {
            let k = ks(t);
            self.add("scylla_alternator_returned_records", &o, *n as f64);
            self.add(
                "scylla_alternator_table_returned_records",
                &[("op", op), ("cf", t.as_str()), ("ks", k.as_str())],
                *n as f64,
            );
        }
        for (t, kb) in &s.sizes {
            let k = ks(t);
            self.observe(
                "scylla_alternator_operation_size_kb",
                &o,
                Buckets::SizeKb,
                *kb,
            );
            self.observe(
                "scylla_alternator_table_operation_size_kb",
                &[("op", op), ("cf", t.as_str()), ("ks", k.as_str())],
                Buckets::SizeKb,
                *kb,
            );
        }
        for (list, global, table) in [
            (
                &s.reads_before_write,
                "scylla_alternator_reads_before_write",
                "scylla_alternator_table_reads_before_write",
            ),
            (
                &s.lwt,
                "scylla_alternator_write_using_lwt",
                "scylla_alternator_table_write_using_lwt",
            ),
            (
                &s.condition_failed,
                "scylla_alternator_conditional_check_failed",
                "scylla_alternator_table_conditional_check_failed",
            ),
        ] {
            for t in list {
                let k = ks(t);
                self.add(global, &o, 1.0);
                self.add(
                    table,
                    &[("op", op), ("cf", t.as_str()), ("ks", k.as_str())],
                    1.0,
                );
            }
        }
        let group = s.scheduling_group.as_deref().unwrap_or("sl:default");
        for (list, coordinator, cf) in [
            (
                &s.storage_reads,
                "scylla_storage_proxy_coordinator_read_latency",
                "scylla_column_family_read_latency",
            ),
            (
                &s.storage_writes,
                "scylla_storage_proxy_coordinator_write_latency",
                "scylla_column_family_write_latency",
            ),
        ] {
            for t in list {
                let k = ks(t);
                self.observe(
                    coordinator,
                    &[("scheduling_group_name", group)],
                    Buckets::Latency,
                    micros,
                );
                self.observe(
                    cf,
                    &[("cf", t.as_str()), ("ks", k.as_str())],
                    Buckets::Latency,
                    micros,
                );
            }
        }
    }

    /// The Prometheus text exposition of every metric (`shard="0"` on each, as ScyllaDB's).
    pub fn render(&self) -> String {
        let r = self.r.lock();
        let mut out = String::new();
        let fmt_labels = |l: &Labels, extra: Option<(&str, &str)>| {
            let mut parts: Vec<String> = l
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                .collect();
            if let Some((k, v)) = extra {
                parts.push(format!("{k}=\"{v}\""));
            }
            parts.push("shard=\"0\"".into());
            parts.join(",")
        };
        let mut last = "";
        for ((name, l), v) in &r.counters {
            if *name != last {
                let _ = writeln!(out, "# TYPE {name} counter");
                last = name;
            }
            let _ = writeln!(out, "{name}{{{}}} {v}", fmt_labels(l, None));
        }
        let mut last = "";
        for ((name, l), h) in &r.histograms {
            if *name != last {
                let _ = writeln!(out, "# TYPE {name} histogram");
                last = name;
            }
            let mut cum = 0u64;
            for (i, b) in h.buckets.bounds().iter().enumerate() {
                cum += h.counts[i];
                let le = format!("{b:.6}");
                let _ = writeln!(
                    out,
                    "{name}_bucket{{{}}} {cum}",
                    fmt_labels(l, Some(("le", &le)))
                );
            }
            cum += h.counts[h.counts.len() - 1];
            let _ = writeln!(
                out,
                "{name}_bucket{{{}}} {cum}",
                fmt_labels(l, Some(("le", "+Inf")))
            );
            let _ = writeln!(out, "{name}_sum{{{}}} {}", fmt_labels(l, None), h.sum);
            let _ = writeln!(out, "{name}_count{{{}}} {}", fmt_labels(l, None), h.count);
        }
        out
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_as_scylla_has_them() {
        // test_metrics.py's expo_estimated_histogram_bucket_offsets(512).
        let b = expo_buckets(512);
        assert_eq!(
            &b[..12],
            &[
                1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 10.0, 12.0, 14.0, 16.0
            ]
        );
        assert_eq!(b.last(), Some(&512.0));
        assert!(b.contains(&112.0) && b.contains(&20.0));
        let items = expo_buckets(1 << 12);
        assert!(items.contains(&2.0) && items.contains(&3.0) && items.contains(&112.0));
    }

    #[test]
    fn counters_and_histograms_render() {
        let m = Metrics::default();
        let s = RequestStats::default();
        s.table("t");
        s.batch_items("t", 3);
        s.item_size("t", 17 * 1024);
        m.finish(Some("BatchWriteItem"), &s, 200, false, 10.0);
        let text = m.render();
        assert!(text.contains("scylla_alternator_operation{op=\"BatchWriteItem\",shard=\"0\"} 1"));
        assert!(text.contains("scylla_alternator_table_operation{op=\"BatchWriteItem\",cf=\"t\",ks=\"alternator_t\",shard=\"0\"} 1"));
        assert!(text.contains("scylla_alternator_batch_item_count_histogram_bucket{op=\"BatchWriteItem\",le=\"2.000000\",shard=\"0\"} 0"));
        assert!(text.contains("scylla_alternator_batch_item_count_histogram_bucket{op=\"BatchWriteItem\",le=\"3.000000\",shard=\"0\"} 1"));
        assert!(text.contains("scylla_alternator_operation_size_kb_bucket{op=\"BatchWriteItem\",le=\"16.000000\",shard=\"0\"} 0"));
        assert!(text.contains("scylla_alternator_operation_size_kb_bucket{op=\"BatchWriteItem\",le=\"20.000000\",shard=\"0\"} 1"));
        assert!(text.contains("scylla_alternator_total_operations{shard=\"0\"} 1"));
        m.finish(None, &RequestStats::default(), 400, false, 1.0);
        assert!(
            m.render()
                .contains("scylla_alternator_unsupported_operations{shard=\"0\"} 1")
        );
    }
}
