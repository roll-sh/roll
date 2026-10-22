//! A database's configuration: its dialect, the identity of the node it presents, and the
//! `cassandra.yaml` / `scylla.yaml` settings that change what clients can observe.
//!
//! Defaults are the reference images' (`cassandra:5.0.9`, `scylladb/scylla:2026.3.3` in developer
//! mode), so a database with default settings behaves like a freshly started reference.

use nosql_types::Dialect;

/// Settings mirroring the server configuration files, per dialect where the references differ.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// `batch_size_warn_threshold` (bytes). Cassandra 5.0: 5 KiB; ScyllaDB: 128 KiB.
    pub batch_size_warn_threshold: u64,
    /// `batch_size_fail_threshold` (bytes). Cassandra 5.0: 50 KiB; ScyllaDB: 1 MiB.
    pub batch_size_fail_threshold: u64,
    /// `unlogged_batch_across_partitions_warn_threshold` (Cassandra: 10).
    pub unlogged_batch_across_partitions_warn_threshold: u64,
    /// `tombstone_warn_threshold` (1000 in both).
    pub tombstone_warn_threshold: u64,
    /// `tombstone_failure_threshold` (100000 in both).
    pub tombstone_failure_threshold: u64,
    /// `materialized_views_enabled` (Cassandra: false; ScyllaDB always on).
    pub materialized_views_enabled: bool,
    /// `sasi_indexes_enabled` (Cassandra: false).
    pub sasi_indexes_enabled: bool,
    /// Cassandra's `cassandra.sai.vector.allow_custom_parameters` system property (false): SAI
    /// vector indexes may set `maximum_node_connections` / `construction_beam_width`.
    pub sai_vector_allow_custom_parameters: bool,
    /// `user_defined_functions_enabled` / `enable_user_defined_functions` (false in both).
    pub user_defined_functions_enabled: bool,
    /// Cassandra `dynamic_data_masking_enabled` (false): column masks can be created and are
    /// applied to roles without `UNMASK`.
    pub dynamic_data_masking_enabled: bool,
    /// ScyllaDB `enable_create_table_with_compact_storage` (false).
    pub compact_storage_enabled: bool,
    /// Cassandra `drop_compact_storage_enabled` (false).
    pub drop_compact_storage_enabled: bool,
    /// ScyllaDB `max_partition_key_restrictions_per_query` (100).
    pub max_partition_key_restrictions_per_query: u64,
    /// ScyllaDB `max_clustering_key_restrictions_per_query` (100).
    pub max_clustering_key_restrictions_per_query: u64,
    /// ScyllaDB `minimum_replication_factor_warn_threshold` (3; -1 disables).
    pub minimum_replication_factor_warn_threshold: i64,
    /// ScyllaDB: new NetworkTopologyStrategy keyspaces use tablets (`tablets_mode_for_new_keyspaces`).
    pub tablets_by_default: bool,
    /// Cassandra `allow_filtering_enabled` guardrail (true).
    pub allow_filtering_enabled: bool,
    /// `authenticator` class name (`AllowAllAuthenticator`).
    pub authenticator: String,
    /// `authorizer` class name (`AllowAllAuthorizer`).
    pub authorizer: String,
    /// Cassandra `network_authorizer` (`AllowAllNetworkAuthorizer`; `CassandraNetworkAuthorizer`
    /// enforces `ACCESS TO DATACENTERS`).
    pub network_authorizer: String,
    /// Cassandra `permissions_validity` (ms, 2000; 0 disables the cache).
    pub permissions_validity_ms: u64,
    /// Cassandra `roles_validity` (ms, 2000).
    pub roles_validity_ms: u64,
    /// Cassandra `credentials_validity` (ms, 2000).
    pub credentials_validity_ms: u64,
    /// Cassandra `cassandra.auth_bcrypt_gensalt_log2_rounds` (10).
    pub auth_bcrypt_log2_rounds: u32,
    /// Cassandra `cassandra.role_password_update_min_interval_in_ms` (5000; 0 disables).
    pub role_password_update_min_interval_ms: u64,
    /// Shards presented to ScyllaDB drivers (`--smp`).
    pub shards: u32,
    /// ScyllaDB `strict_allow_filtering`.
    pub strict_allow_filtering: StrictAllowFiltering,
    /// ScyllaDB's guardrails and the other options `system.config` updates live
    /// ([`crate::guardrails`]).
    pub scylla: ScyllaOptions,
}

/// ScyllaDB options behind its guardrails and query limits, and the bookkeeping of
/// `UPDATE system.config` ([`crate::guardrails::config_table`]). Cassandra databases ignore them.
#[derive(Clone, Debug, PartialEq)]
pub struct ScyllaOptions {
    /// `replication_strategy_warn_list` (`["SimpleStrategy"]`).
    pub replication_strategy_warn_list: Vec<String>,
    /// `replication_strategy_fail_list` (`[]`).
    pub replication_strategy_fail_list: Vec<String>,
    /// `restrict_replication_simplestrategy` (`"0"`): the legacy switch; `true` forbids
    /// SimpleStrategy, `warn` warns about it.
    pub restrict_replication_simplestrategy: String,
    /// `minimum_replication_factor_fail_threshold` (-1: off).
    pub minimum_replication_factor_fail_threshold: i64,
    /// `maximum_replication_factor_warn_threshold` (-1: off).
    pub maximum_replication_factor_warn_threshold: i64,
    /// `maximum_replication_factor_fail_threshold` (-1: off).
    pub maximum_replication_factor_fail_threshold: i64,
    /// `write_consistency_levels_warned` (`[]`).
    pub write_consistency_levels_warned: Vec<crate::error::Consistency>,
    /// `write_consistency_levels_disallowed` (`[]`).
    pub write_consistency_levels_disallowed: Vec<crate::error::Consistency>,
    /// `query_tombstone_page_limit` (10000): a page ends after this many tombstones.
    pub query_tombstone_page_limit: u64,
    /// The large data guardrails' thresholds (`large_data_guardrails_enabled` tables).
    pub large_data: LargeData,
    /// `enable_cassio_compatibility` (false).
    pub enable_cassio_compatibility: bool,
    /// `cql_duplicate_bind_variable_names_refer_to_same_variable` (true).
    pub duplicate_bind_names_same_variable: bool,
    /// `cql_in_bind_variable_name_uses_uppercase_operator` (true).
    pub in_bind_name_uppercase: bool,
    /// `restrict_future_timestamp` (true): writes more than 3 days in the future are refused.
    pub restrict_future_timestamp: bool,
    /// `enable_cql_config_updates` (true): `UPDATE system.config` is allowed.
    pub cql_config_updates: bool,
    /// Options set through `UPDATE system.config` (their `source` reads `cql`).
    pub cql_sourced: std::collections::BTreeSet<String>,
    /// Values of live options without behaviour in roll, as `system.config` shows them.
    pub values: std::collections::BTreeMap<String, String>,
    /// `alternator_ttl_period_in_seconds` (86400): how often the rows of per-row TTL tables are
    /// looked at for expiration ([`crate::rowttl`]).
    pub alternator_ttl_period_in_seconds: f64,
    /// `auth_certificate_role_queries` (`[{source: SUBJECT, query: "CN=([^,]+)"}]`): how
    /// `CertificateAuthenticator` finds the role in a client certificate; the first that matches
    /// wins ([`crate::auth::certificate`]).
    pub auth_certificate_role_queries: Vec<crate::auth::certificate::RoleQuery>,
    /// The audit settings (`audit`, `audit_categories`, `audit_keyspaces`, `audit_tables`,
    /// `audit_rules`; [`crate::audit`]).
    pub audit: crate::audit::Config,
}

/// ScyllaDB's large data thresholds (`compaction_*_warning_threshold` soft limits, `*_fail_*`
/// hard limits; a hard limit of 0 is off).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LargeData {
    /// `compaction_large_partition_warning_threshold_mb` (1000).
    pub partition_warn_mb: u64,
    /// `large_partition_fail_threshold_mb` (2000).
    pub partition_fail_mb: u64,
    /// `compaction_rows_count_warning_threshold` (100000).
    pub rows_warn: u64,
    /// `rows_count_fail_threshold` (200000).
    pub rows_fail: u64,
    /// `compaction_large_row_warning_threshold_mb` (10).
    pub row_warn_mb: u64,
    /// `large_row_fail_threshold_mb` (20).
    pub row_fail_mb: u64,
    /// `compaction_large_cell_warning_threshold_mb` (1).
    pub cell_warn_mb: u64,
    /// `large_cell_fail_threshold_mb` (2).
    pub cell_fail_mb: u64,
    /// `compaction_collection_elements_count_warning_threshold` (10000).
    pub collection_warn: u64,
    /// `large_collection_elements_fail_threshold` (20000).
    pub collection_fail: u64,
    /// `large_data_cql_warnings` (true): soft limit violations are sent as warnings.
    pub cql_warnings: bool,
}

impl Default for LargeData {
    fn default() -> Self {
        LargeData {
            partition_warn_mb: 1000,
            partition_fail_mb: 2000,
            rows_warn: 100_000,
            rows_fail: 200_000,
            row_warn_mb: 10,
            row_fail_mb: 20,
            cell_warn_mb: 1,
            cell_fail_mb: 2,
            collection_warn: 10_000,
            collection_fail: 20_000,
            cql_warnings: true,
        }
    }
}

impl Default for ScyllaOptions {
    fn default() -> Self {
        ScyllaOptions {
            replication_strategy_warn_list: vec!["SimpleStrategy".into()],
            replication_strategy_fail_list: Vec::new(),
            restrict_replication_simplestrategy: "0".into(),
            minimum_replication_factor_fail_threshold: -1,
            maximum_replication_factor_warn_threshold: -1,
            maximum_replication_factor_fail_threshold: -1,
            write_consistency_levels_warned: Vec::new(),
            write_consistency_levels_disallowed: Vec::new(),
            query_tombstone_page_limit: 10_000,
            large_data: LargeData::default(),
            enable_cassio_compatibility: false,
            duplicate_bind_names_same_variable: true,
            in_bind_name_uppercase: true,
            restrict_future_timestamp: true,
            cql_config_updates: true,
            cql_sourced: Default::default(),
            values: Default::default(),
            alternator_ttl_period_in_seconds: 86_400.0,
            auth_certificate_role_queries: crate::auth::certificate::RoleQuery::defaults(),
            audit: Default::default(),
        }
    }
}

/// ScyllaDB `strict_allow_filtering`: what happens to a query that filters a key range by its
/// clustering restrictions without ALLOW FILTERING (Cassandra refuses it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictAllowFiltering {
    /// Runs silently (`false`).
    False,
    /// Runs with "This query should use ALLOW FILTERING …" (`warn`, the default).
    Warn,
    /// Refused like a query on a non-key column (`true`; the suites run with it).
    True,
}

impl Settings {
    /// The reference image's defaults for `dialect`.
    pub fn defaults(dialect: Dialect) -> Self {
        let scylla = dialect.is_scylla();
        Settings {
            batch_size_warn_threshold: if scylla { 128 * 1024 } else { 5 * 1024 },
            batch_size_fail_threshold: if scylla { 1024 * 1024 } else { 50 * 1024 },
            unlogged_batch_across_partitions_warn_threshold: 10,
            tombstone_warn_threshold: 1000,
            tombstone_failure_threshold: 100_000,
            materialized_views_enabled: scylla,
            sasi_indexes_enabled: false,
            sai_vector_allow_custom_parameters: false,
            user_defined_functions_enabled: false,
            dynamic_data_masking_enabled: false,
            compact_storage_enabled: !scylla,
            drop_compact_storage_enabled: false,
            max_partition_key_restrictions_per_query: 100,
            max_clustering_key_restrictions_per_query: 100,
            minimum_replication_factor_warn_threshold: 3,
            tablets_by_default: scylla,
            allow_filtering_enabled: true,
            authenticator: "AllowAllAuthenticator".into(),
            authorizer: "AllowAllAuthorizer".into(),
            network_authorizer: "AllowAllNetworkAuthorizer".into(),
            permissions_validity_ms: 2000,
            roles_validity_ms: 2000,
            credentials_validity_ms: 2000,
            auth_bcrypt_log2_rounds: 10,
            role_password_update_min_interval_ms: 5000,
            shards: 1,
            strict_allow_filtering: StrictAllowFiltering::Warn,
            scylla: ScyllaOptions::default(),
        }
    }
}

/// Everything a [`crate::Database`] needs to know about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct DbConfig {
    /// The database id (also the storage database id).
    pub id: String,
    pub dialect: Dialect,
    /// `cluster_name` (ScyllaDB's image reports `""`, Cassandra's `Test Cluster`).
    pub cluster_name: String,
    pub datacenter: String,
    pub rack: String,
    /// Tokens owned by the presented node (`num_tokens`: 16 in Cassandra 5.0, 256 in ScyllaDB).
    pub num_tokens: u32,
    /// The node's host id; `None` derives a UUIDv5 from the database id.
    pub host_id: Option<[u8; 16]>,
    /// The address the node presents (`rpc_address`, `broadcast_address`, …).
    pub address: std::net::IpAddr,
    /// `native_transport_port`.
    pub native_port: u16,
    pub settings: Settings,
    /// The initial superuser of a hosted database (created at open when missing; also the role
    /// a `role@db` login of the platform credential acts as).
    pub superuser: Option<Superuser>,
    /// The platform's masking policy: `(keyspace, table, column)` → the masking call applied to
    /// that column for every role, superusers included, in both dialects
    /// ([`crate::masking`]; preview branches anonymise production data this way).
    pub masking: std::collections::BTreeMap<(String, String, String), MaskCall>,
}

/// A masking function call of the platform policy: a function of the column's value and
/// literal arguments, written as CQL (`mask_inner(1, 1)`, `mask_default()`, `mask_hash('MD5')`;
/// `system.` may be omitted). The functions are Cassandra 5's masking functions in both
/// dialects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskCall {
    /// Lower-case function name without the keyspace (`mask_inner`).
    pub function: String,
    /// The arguments after the column, as CQL literals (`1`, `'#'`, `null`).
    pub args: Vec<String>,
}

impl MaskCall {
    /// Parses `name(args…)` (or a bare `name`, or `DEFAULT` for `mask_default`). Names must be
    /// one of the masking functions; arguments must be literals.
    pub fn parse(text: &str) -> Result<MaskCall, String> {
        crate::masking::parse_call(text)
    }
}

impl std::fmt::Display for MaskCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({})", self.function, self.args.join(", "))
    }
}

/// A role created with `SUPERUSER = true AND LOGIN = true` when the database opens without it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superuser {
    pub name: String,
    pub password: String,
}

impl DbConfig {
    /// A configuration with the reference image's defaults for `dialect`.
    pub fn new(id: impl Into<String>, dialect: Dialect) -> Self {
        let scylla = dialect.is_scylla();
        DbConfig {
            id: id.into(),
            dialect,
            cluster_name: if scylla {
                String::new()
            } else {
                "Test Cluster".into()
            },
            datacenter: "datacenter1".into(),
            rack: "rack1".into(),
            num_tokens: if scylla { 256 } else { 16 },
            host_id: None,
            address: std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            native_port: 9042,
            settings: Settings::defaults(dialect),
            superuser: None,
            masking: std::collections::BTreeMap::new(),
        }
    }

    /// The presented node's host id.
    pub fn host_id(&self) -> [u8; 16] {
        self.host_id
            .unwrap_or_else(|| crate::uuid::v5(&crate::uuid::NAMESPACE_ROLL, self.id.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_per_dialect() {
        let s = DbConfig::new("db", Dialect::Scylla);
        assert_eq!(s.cluster_name, "");
        assert_eq!(s.num_tokens, 256);
        assert!(s.settings.materialized_views_enabled);
        let c = DbConfig::new("db", Dialect::Cassandra);
        assert_eq!(c.cluster_name, "Test Cluster");
        assert_eq!(c.settings.batch_size_warn_threshold, 5120);
        assert_eq!(
            c.host_id(),
            DbConfig::new("db", Dialect::Cassandra).host_id()
        );
        assert_ne!(
            c.host_id(),
            DbConfig::new("db2", Dialect::Cassandra).host_id()
        );
        assert_eq!(crate::uuid::version(&c.host_id()), 5);
    }
}
