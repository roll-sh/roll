//! The guardrail counters of a database, rendered as ScyllaDB's Prometheus metrics
//! (`scylla_cql_*` guardrail violations, `scylla_cql_writes_per_consistency_level`,
//! `scylla_database_large_*_exceeding_threshold`), one virtual shard (`shard="0"`).
//!
//! As in ScyllaDB, the fail and maximum-RF counters are only listed once they count something,
//! and the per-consistency write counters only for levels that were used.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Consistency;

/// Guardrail violation and write counters.
#[derive(Debug, Default)]
pub struct Metrics {
    pub replication_strategy_warn_list_violations: AtomicU64,
    pub replication_strategy_fail_list_violations: AtomicU64,
    pub minimum_replication_factor_warn_violations: AtomicU64,
    pub minimum_replication_factor_fail_violations: AtomicU64,
    pub maximum_replication_factor_warn_violations: AtomicU64,
    pub maximum_replication_factor_fail_violations: AtomicU64,
    pub write_consistency_levels_warned_violations: AtomicU64,
    pub write_consistency_levels_disallowed_violations: AtomicU64,
    /// Writes per consistency level (by protocol code).
    pub writes_per_consistency_level: [AtomicU64; 12],
    pub large_partition_exceeding_threshold: AtomicU64,
    pub large_rows_exceeding_threshold: AtomicU64,
    pub large_cell_exceeding_threshold: AtomicU64,
    pub large_collection_exceeding_threshold: AtomicU64,
    /// Index reads per `(keyspace, index)`: count and microseconds
    /// (`scylla_index_query_latencies`).
    pub index_queries: parking_lot::Mutex<std::collections::BTreeMap<(String, String), (u64, u64)>>,
}

/// `(name, help, listed when zero)`.
const COUNTERS: &[(&str, &str, bool)] = &[
    (
        "scylla_cql_maximum_replication_factor_fail_violations",
        "Counts the number of maximum_replication_factor_fail_threshold guardrail violations, i.e. attempts to create a keyspace with RF on one of the DCs above the set guardrail.",
        false,
    ),
    (
        "scylla_cql_maximum_replication_factor_warn_violations",
        "Counts the number of maximum_replication_factor_warn_threshold guardrail violations, i.e. attempts to create a keyspace with RF on one of the DCs above the set guardrail.",
        false,
    ),
    (
        "scylla_cql_minimum_replication_factor_fail_violations",
        "Counts the number of minimum_replication_factor_fail_threshold guardrail violations, i.e. attempts to create a keyspace with RF on one of the DCs below the set guardrail.",
        false,
    ),
    (
        "scylla_cql_minimum_replication_factor_warn_violations",
        "Counts the number of minimum_replication_factor_warn_threshold guardrail violations, i.e. attempts to create a keyspace with RF on one of the DCs below the set guardrail.",
        true,
    ),
    (
        "scylla_cql_replication_strategy_fail_list_violations",
        "Counts the number of replication_strategy_fail_list guardrail violations, i.e. attempts to set a forbidden replication strategy in a keyspace via CREATE/ALTER KEYSPACE.",
        false,
    ),
    (
        "scylla_cql_replication_strategy_warn_list_violations",
        "Counts the number of replication_strategy_warn_list guardrail violations, i.e. attempts to set a discouraged replication strategy in a keyspace via CREATE/ALTER KEYSPACE.",
        true,
    ),
    (
        "scylla_cql_write_consistency_levels_disallowed_violations",
        "Counts the number of write_consistency_levels_disallowed guardrail violations, i.e. attempts to write with a forbidden consistency level.",
        true,
    ),
    (
        "scylla_cql_write_consistency_levels_warned_violations",
        "Counts the number of write_consistency_levels_warned guardrail violations, i.e. attempts to write with a discouraged consistency level.",
        true,
    ),
    (
        "scylla_database_large_cell_exceeding_threshold",
        "Number of large cells exceeding compaction_large_cell_warning_threshold_mb. Large cells have performance impact and should be avoided, check the documentation for details.",
        true,
    ),
    (
        "scylla_database_large_collection_exceeding_threshold",
        "Number of large collections exceeding compaction_collection_elements_count_warning_threshold. Large collections have performance impact and should be avoided, check the documentation for details.",
        true,
    ),
    (
        "scylla_database_large_partition_exceeding_threshold",
        "Number of large partitions exceeding compaction_large_partition_warning_threshold_mb. Large partitions have performance impact and should be avoided, check the documentation for details.",
        true,
    ),
    (
        "scylla_database_large_rows_exceeding_threshold",
        "Number of large rows exceeding compaction_large_row_warning_threshold_mb. Large rows have performance impact and should be avoided, check the documentation for details.",
        true,
    ),
];

impl Metrics {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts a write (statement or batch) at `cl`.
    pub fn count_write(&self, cl: Consistency) {
        if let Some(c) = self.writes_per_consistency_level.get(cl.code() as usize) {
            Self::inc(c);
        }
    }

    fn counter(&self, name: &str) -> u64 {
        let c = match name {
            "scylla_cql_maximum_replication_factor_fail_violations" => {
                &self.maximum_replication_factor_fail_violations
            }
            "scylla_cql_maximum_replication_factor_warn_violations" => {
                &self.maximum_replication_factor_warn_violations
            }
            "scylla_cql_minimum_replication_factor_fail_violations" => {
                &self.minimum_replication_factor_fail_violations
            }
            "scylla_cql_minimum_replication_factor_warn_violations" => {
                &self.minimum_replication_factor_warn_violations
            }
            "scylla_cql_replication_strategy_fail_list_violations" => {
                &self.replication_strategy_fail_list_violations
            }
            "scylla_cql_replication_strategy_warn_list_violations" => {
                &self.replication_strategy_warn_list_violations
            }
            "scylla_cql_write_consistency_levels_disallowed_violations" => {
                &self.write_consistency_levels_disallowed_violations
            }
            "scylla_cql_write_consistency_levels_warned_violations" => {
                &self.write_consistency_levels_warned_violations
            }
            "scylla_database_large_cell_exceeding_threshold" => {
                &self.large_cell_exceeding_threshold
            }
            "scylla_database_large_collection_exceeding_threshold" => {
                &self.large_collection_exceeding_threshold
            }
            "scylla_database_large_partition_exceeding_threshold" => {
                &self.large_partition_exceeding_threshold
            }
            _ => &self.large_rows_exceeding_threshold,
        };
        c.load(Ordering::Relaxed)
    }

    /// Counts an index read of `keyspace.index` that took `micros`.
    pub fn count_index_query(&self, keyspace: &str, index: &str, micros: u64) {
        let mut m = self.index_queries.lock();
        let e = m.entry((keyspace.to_string(), index.to_string())).or_default();
        e.0 += 1;
        e.1 += micros;
    }

    /// Appends the metrics in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        let queries = self.index_queries.lock().clone();
        if !queries.is_empty() {
            let name = "scylla_index_query_latencies";
            let _ = writeln!(out, "# HELP {name} Index query latencies\n# TYPE {name} histogram");
            for ((ks, idx), (n, us)) in &queries {
                let _ = writeln!(out, "{name}_sum{{idx=\"{idx}\",ks=\"{ks}\"}} {us}");
                let _ = writeln!(out, "{name}_count{{idx=\"{idx}\",ks=\"{ks}\"}} {n}");
            }
        }
        for (name, help, when_zero) in COUNTERS {
            let v = self.counter(name);
            if v == 0 && !when_zero {
                continue;
            }
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            let _ = writeln!(out, "{name}{{shard=\"0\"}} {v}");
        }
        let name = "scylla_cql_writes_per_consistency_level";
        let _ = writeln!(
            out,
            "# HELP {name} Counts the number of writes for each consistency level.\n# TYPE {name} counter"
        );
        let mut levels: Vec<(&str, u64)> = (0..12u16)
            .filter_map(|code| {
                let cl = Consistency::from_code(code)?;
                let n = self.writes_per_consistency_level[code as usize].load(Ordering::Relaxed);
                (n > 0).then(|| (cl.name(), n))
            })
            .collect();
        levels.sort();
        for (cl, n) in levels {
            let _ = writeln!(
                out,
                "{name}{{consistency_level=\"{cl}\",shard=\"0\"}} {n}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render() {
        let m = Metrics::default();
        Metrics::inc(&m.replication_strategy_warn_list_violations);
        m.count_write(Consistency::One);
        m.count_write(Consistency::One);
        m.count_write(Consistency::All);
        let mut s = String::new();
        m.render(&mut s);
        assert!(s.contains("\nscylla_cql_replication_strategy_warn_list_violations{shard=\"0\"} 1\n"));
        assert!(s.contains("scylla_cql_minimum_replication_factor_warn_violations{shard=\"0\"} 0\n"));
        assert!(!s.contains("scylla_cql_minimum_replication_factor_fail_violations"));
        assert!(s.contains(
            "scylla_cql_writes_per_consistency_level{consistency_level=\"ALL\",shard=\"0\"} 1\nscylla_cql_writes_per_consistency_level{consistency_level=\"ONE\",shard=\"0\"} 2\n"
        ));
        Metrics::inc(&m.maximum_replication_factor_fail_violations);
        let mut s = String::new();
        m.render(&mut s);
        assert!(s.contains("# TYPE scylla_cql_maximum_replication_factor_fail_violations counter\nscylla_cql_maximum_replication_factor_fail_violations{shard=\"0\"} 1\n"));
    }
}
