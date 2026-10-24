//! ScyllaDB's guardrails and live configuration (`docs/cql/guardrails.rst` of ScyllaDB):
//!
//! - `UPDATE system.config SET value = ? WHERE name = ?` for the live-updatable options
//!   ([`config_table`]), applied through [`crate::Database::set_settings`], and the
//!   `WriteFailure` every other ScyllaDB virtual table answers to a write.
//! - Replication strategy and factor guardrails (`replication_strategy_{warn,fail}_list`,
//!   `{minimum,maximum}_replication_factor_{warn,fail}_threshold`): checked in
//!   `crate::schema::replication`, counted here.
//! - The write consistency guardrails ([`write_cl`]): `write_consistency_levels_{warned,disallowed}`.
//! - The large data guardrails of `large_data_guardrails_enabled` tables ([`large_data`]).
//! - The counters behind ScyllaDB's `scylla_cql_*` guardrail metrics ([`metrics`]), rendered
//!   by the server's Prometheus endpoint.
//!
//! The options live in [`crate::config::ScyllaOptions`]; a Cassandra database ignores them.

pub mod config_table;
pub mod large_data;
pub mod metrics;
pub mod write_cl;

pub use metrics::Metrics;

use crate::database::Database;

impl Database {
    /// The guardrail counters (`scylla_cql_*` metrics).
    pub fn guardrail_metrics(&self) -> &Metrics {
        &self.live.metrics
    }
}
