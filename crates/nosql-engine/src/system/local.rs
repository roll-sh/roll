//! The node-identity tables drivers read on connect: `system.local`, `system.peers`,
//! `system.peers_v2` (Cassandra), ScyllaDB's `system.scylla_local` and `system.versions`, and the
//! size estimates (`system.size_estimates`, Cassandra's `system.table_estimates`).
//!
//! A database presents a single node, so `peers` and `peers_v2` are always empty and every token
//! range of the ring is local.

use std::sync::Arc;

use md5::{Digest, Md5};
use nosql_types::Dialect;

use super::{Registry, TableDef, VRow, VirtualContext, VirtualTable, c, def, val};
use crate::error::Result;
use crate::schema::{Table, TableKind};

/// ScyllaDB 2026.3.3's `supported_features` / `enabled_features`.
pub const SCYLLA_FEATURES: &str = "ADDRESS_NODES_BY_HOST_IDS,AGGREGATE_STORAGE_OPTIONS,ALTERNATOR_STREAMS,ALTERNATOR_TTL,ARBITRARY_TABLET_BOUNDARIES,BATCHLOG_V2,CDC,CDC_BLOCK_TABLET_MERGES_FOR_ALTERNATOR_STREAMS,CDC_GENERATIONS_V2,CDC_WITH_TABLETS,CLIENT_ROUTES,COLLECTION_INDEXING,COLOCATED_TABLETS,COMPACTION_HISTORY_UPGRADE,COMPRESSION_DICTS,COMPUTED_COLUMNS,CORRECT_COUNTER_ORDER,CORRECT_IDX_TOKEN_IN_SECONDARY_INDEX,CORRECT_NON_COMPOUND_RANGE_TOMBSTONES,CORRECT_STATIC_COMPACT_IN_MC,COUNTERS,COUNTERS_WITH_TABLETS,CQL_ROW_TTL,DIGEST_FOR_NULL_VALUES,DIGEST_INSENSITIVE_TO_EXPIRY,DIGEST_MULTIPARTITION_READ,DRIVER_SERVICE_LEVEL,EMPTY_REPLICA_MUTATION_PAGES,EMPTY_REPLICA_PAGES,ENFORCED_RAFT_RPC_SCHEDULING_GROUP,FETCH_COLUMN_MAPPINGS_ON_TABLET_MIGRATION,FILE_STREAM,FRAGMENTED_COMMITLOG_ENTRIES,GROUP0_LIMITED_VOTERS,GROUP0_SCHEMA_VERSIONING,HINTED_HANDOFF_SEPARATE_CONNECTION,HOST_ID_BASED_HINTED_HANDOFF,INDEXES,IN_MEMORY_TABLES,KEYSPACE_MULTI_RF_CHANGE,KEYSPACE_STORAGE_OPTIONS,LARGE_COLLECTION_DETECTION,LARGE_DATA_GUARDRAILS,LARGE_DATA_VIRTUAL_TABLES,LARGE_PARTITIONS,LA_SSTABLE_FORMAT,LOAD_AND_STREAM_ABORT_RPC_MESSAGE,LWT,LWT_WITH_TABLETS,MAINTENANCE_TENANT,MATERIALIZED_VIEWS,MC_SSTABLE_FORMAT,MD_SSTABLE_FORMAT,ME_SSTABLE_FORMAT,MS_SSTABLE_FORMAT,MT_SSTABLE_FORMAT,NATIVE_REVERSE_QUERIES,NONFROZEN_UDTS,PARALLELIZED_AGGREGATION,PARALLEL_TABLET_DRAINING,PER_TABLE_CACHING,PER_TABLE_PARTITIONERS,QUIESCE_TOPOLOGY_ENHANCED,RACK_LIST_RF,RANGE_SCAN_DATA_VARIANT,RANGE_TOMBSTONES,RANGE_TOMBSTONE_AND_DEAD_ROWS_DETECTION,REMOVENODE_WITH_LEFT_TOKEN_RING,REPAIR_BASED_TABLET_REBUILD,REPAIR_MSG_SPLIT,ROLES,ROW_LEVEL_REPAIR,SCHEMA_COMMITLOG,SCHEMA_TABLES_V3,SECONDARY_INDEXES_ON_STATIC_COLUMNS,SEPARATE_PAGE_SIZE_AND_SAFETY_LIMIT,SIZE_BASED_LOAD_BALANCING,SNAPSHOT_AS_TOPOLOGY_OPERATION,SSTABLE_COMPRESSION_DICTS,SSTABLE_REFERENCE_SHARING,STREAM_WITH_RPC_STREAM,SUPPORTS_CONSISTENT_TOPOLOGY_CHANGES,SUPPORTS_RAFT_CLUSTER_MANAGEMENT,TABLETS,TABLETS_INTERMEDIATE_FALLBACK_CLEANUP,TABLET_INCREMENTAL_REPAIR,TABLET_LOAD_STATS_V2,TABLET_MERGE,TABLET_MIGRATION_VIRTUAL_TASK,TABLET_OPTIONS,TABLET_POW2_CONVERGENCE,TABLET_RACK_AWARE_VIEW_PAIRING,TABLET_REPAIR_SCHEDULER,TABLET_REPAIR_TASKS_TABLE,TABLET_RESIZE_VIRTUAL_TASK,TABLE_DIGEST_INSENSITIVE_TO_EXPIRY,TOMBSTONE_GC_OPTIONS,TOPOLOGY_GLOBAL_REQUEST_QUEUE,TOPOLOGY_NOOP_REQUEST,TOPOLOGY_REQUESTS_TYPE_COLUMN,TRUNCATE_AS_TOPOLOGY_OPERATION,TRUNCATION_TABLE,TYPED_ERRORS_IN_READ_RPC,UDA,UDA_NATIVE_PARALLELIZED_AGGREGATION,UNBOUNDED_RANGE_TOMBSTONES,UUID_SSTABLE_IDENTIFIERS,VIEWS_WITH_TABLETS,VIEW_BUILDING_COORDINATOR,VIEW_BUILDING_TASKS_MIN_TASK_ID,VIEW_BUILD_STATUS_ON_GROUP0,VIEW_VIRTUAL_COLUMNS,VNODES_TO_TABLETS_MIGRATIONS,WORKLOAD_PRIORITIZATION,WRITETIME_TTL_INDIVIDUAL_ELEMENT,WRITE_FAILURE_REPLY,XXHASH,ZERO_TOKEN_NODES";

pub const SCYLLA_VERSION: &str = "2026.3.3-0.20261004.d80b3d6ca26b";
pub const SCYLLA_BUILD_ID: &str = "d0d29ed4522f3714b887a0841c08d59f92b273fc";

/// `release_version`, `cql_version` and `native_protocol_version` of `system.local`.
pub fn versions(d: Dialect) -> (&'static str, &'static str, &'static str) {
    match d {
        Dialect::Scylla => ("3.0.8", "3.3.1", "4"),
        Dialect::Cassandra => ("5.0.9", "3.4.7", "5"),
    }
}

pub fn tables(d: Dialect) -> Vec<TableDef> {
    let ks = "system";
    let mut v = Vec::new();
    let mut local = vec![
        c("key", "text", 'p', 0),
        c("bootstrapped", "text", 'r', -1),
        c("broadcast_address", "inet", 'r', -1),
        c("cluster_name", "text", 'r', -1),
        c("cql_version", "text", 'r', -1),
        c("data_center", "text", 'r', -1),
        c("gossip_generation", "int", 'r', -1),
        c("host_id", "uuid", 'r', -1),
        c("listen_address", "inet", 'r', -1),
        c("native_protocol_version", "text", 'r', -1),
        c("partitioner", "text", 'r', -1),
        c("rack", "text", 'r', -1),
        c("release_version", "text", 'r', -1),
        c("rpc_address", "inet", 'r', -1),
        c("schema_version", "uuid", 'r', -1),
        c("tokens", "set<text>", 'r', -1),
        c("truncated_at", "map<uuid, blob>", 'r', -1),
    ];
    let mut peers = vec![
        c("peer", "inet", 'p', 0),
        c("data_center", "text", 'r', -1),
        c("host_id", "uuid", 'r', -1),
        c("preferred_ip", "inet", 'r', -1),
        c("rack", "text", 'r', -1),
        c("release_version", "text", 'r', -1),
        c("rpc_address", "inet", 'r', -1),
        c("schema_version", "uuid", 'r', -1),
        c("tokens", "set<text>", 'r', -1),
    ];
    match d {
        Dialect::Scylla => {
            local.push(c("supported_features", "text", 'r', -1));
            peers.push(c("supported_features", "text", 'r', -1));
        }
        Dialect::Cassandra => {
            local.push(c("broadcast_port", "int", 'r', -1));
            local.push(c("listen_port", "int", 'r', -1));
            local.push(c("rpc_port", "int", 'r', -1));
        }
    }
    let gc_local = 0;
    let mut add = |t: TableDef| v.push(t);
    add(def(
        ks,
        "local",
        "information about the local node",
        gc_local,
        TableKind::Virtual,
        local,
    ));
    add(def(
        ks,
        "peers",
        "information about known peers in the cluster",
        0,
        TableKind::Virtual,
        peers,
    ));
    add(def(
        ks,
        "size_estimates",
        if d.is_scylla() {
            "per-table primary range size estimates"
        } else {
            "per-table primary range size estimates, table is deprecated in favor of table_estimates"
        },
        0,
        TableKind::Virtual,
        vec![
            c("keyspace_name", "text", 'p', 0),
            c("table_name", "text", 'c', 0),
            c("range_start", "text", 'c', 1),
            c("range_end", "text", 'c', 2),
            c("mean_partition_size", "bigint", 'r', -1),
            c("partitions_count", "bigint", 'r', -1),
        ],
    ));
    match d {
        Dialect::Cassandra => {
            add(def(
                ks,
                "peers_v2",
                "information about known peers in the cluster",
                0,
                TableKind::Virtual,
                vec![
                    c("peer", "inet", 'p', 0),
                    c("peer_port", "int", 'c', 0),
                    c("data_center", "text", 'r', -1),
                    c("host_id", "uuid", 'r', -1),
                    c("native_address", "inet", 'r', -1),
                    c("native_port", "int", 'r', -1),
                    c("preferred_ip", "inet", 'r', -1),
                    c("preferred_port", "int", 'r', -1),
                    c("rack", "text", 'r', -1),
                    c("release_version", "text", 'r', -1),
                    c("schema_version", "uuid", 'r', -1),
                    c("tokens", "set<text>", 'r', -1),
                ],
            ));
            add(def(
                ks,
                "table_estimates",
                "per-table range size estimates",
                0,
                TableKind::Virtual,
                vec![
                    c("keyspace_name", "text", 'p', 0),
                    c("table_name", "text", 'c', 0),
                    c("range_type", "text", 'c', 1),
                    c("range_start", "text", 'c', 2),
                    c("range_end", "text", 'c', 3),
                    c("mean_partition_size", "bigint", 'r', -1),
                    c("partitions_count", "bigint", 'r', -1),
                ],
            ));
        }
        Dialect::Scylla => {
            add(def(
                ks,
                "scylla_local",
                "Scylla specific information about the local node",
                864_000,
                TableKind::Virtual,
                vec![c("key", "text", 'p', 0), c("value", "text", 'r', -1)],
            ));
            add(def(
                ks,
                "versions",
                "Version information.",
                864_000,
                TableKind::Virtual,
                vec![
                    c("key", "text", 'p', 0),
                    c("build_id", "text", 'r', -1),
                    c("build_mode", "text", 'r', -1),
                    c("version", "text", 'r', -1),
                ],
            ));
        }
    }
    if !d.is_scylla() {
        for t in &mut v {
            t.memtable_flush_period_in_ms = 3_600_000;
        }
    }
    v
}

pub fn register(r: &mut Registry, d: Dialect) {
    r.add("system", "local", Arc::new(Local));
    r.add("system", "peers", Arc::new(Empty));
    r.add(
        "system",
        "size_estimates",
        Arc::new(SizeEstimates { range_types: false }),
    );
    match d {
        Dialect::Cassandra => {
            r.add("system", "peers_v2", Arc::new(Empty));
            r.add(
                "system",
                "table_estimates",
                Arc::new(SizeEstimates { range_types: true }),
            );
        }
        Dialect::Scylla => {
            r.add("system", "scylla_local", Arc::new(ScyllaLocal));
            r.add("system", "versions", Arc::new(Versions));
        }
    }
}

/// The node's tokens: `num_tokens` values derived from the host id, so they are stable for a
/// database and spread over the ring.
pub fn tokens(host_id: &[u8; 16], n: u32) -> Vec<i64> {
    let mut out: Vec<i64> = (0..n)
        .map(|i| {
            let mut h = Md5::new();
            h.update(host_id);
            h.update(i.to_be_bytes());
            let d = h.finalize();
            let mut b = [0u8; 8];
            b.copy_from_slice(&d[..8]);
            let t = i64::from_be_bytes(b);
            if t == i64::MIN { i64::MAX } else { t }
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

struct Local;

impl VirtualTable for Local {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let cfg = ctx.cfg;
        let d = cfg.dialect;
        let (release, cql, proto) = versions(d);
        let host = cfg.host_id();
        let toks: Vec<String> = tokens(&host, cfg.num_tokens)
            .iter()
            .map(|t| t.to_string())
            .collect();
        let mut row: VRow = vec![
            ("key", val::text("local")),
            ("bootstrapped", val::text("COMPLETED")),
            ("broadcast_address", val::inet(&cfg.address)),
            ("cluster_name", val::text(&cfg.cluster_name)),
            ("cql_version", val::text(cql)),
            ("data_center", val::text(&cfg.datacenter)),
            ("gossip_generation", val::int(ctx.started_at as i32)),
            ("host_id", val::uuid(&host)),
            ("listen_address", val::inet(&cfg.address)),
            ("native_protocol_version", val::text(proto)),
            (
                "partitioner",
                val::text("org.apache.cassandra.dht.Murmur3Partitioner"),
            ),
            ("rack", val::text(&cfg.rack)),
            ("release_version", val::text(release)),
            ("rpc_address", val::inet(&cfg.address)),
            ("schema_version", val::uuid(&ctx.schema.version)),
            ("tokens", val::text_set(toks.iter().map(String::as_str))),
        ];
        match d {
            Dialect::Scylla => row.push(("supported_features", val::text(SCYLLA_FEATURES))),
            Dialect::Cassandra => {
                row.push(("broadcast_port", val::int(7000)));
                row.push(("listen_port", val::int(7000)));
                row.push(("rpc_port", val::int(cfg.native_port as i32)));
            }
        }
        Ok(vec![row])
    }
}

struct Empty;

impl VirtualTable for Empty {
    fn rows(&self, _t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
        Ok(Vec::new())
    }
}

struct ScyllaLocal;

impl VirtualTable for ScyllaLocal {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let group0 = crate::uuid::format(&crate::uuid::v5(
            &crate::uuid::NAMESPACE_ROLL,
            format!("{}/group0", ctx.cfg.id).as_bytes(),
        ));
        let schema = crate::uuid::format(&ctx.schema.version);
        let kv = |k: &'static str, v: &str| -> VRow {
            vec![("key", val::text(k)), ("value", val::text(v))]
        };
        Ok(vec![
            kv("enabled_features", SCYLLA_FEATURES),
            kv("auth_version", "2"),
            kv("raft_group0_id", &group0),
            kv("service_level_version", "2"),
            kv("service_level_driver_created", "true"),
            kv("group0_schema_version", &schema),
            kv("group0_upgrade_state", "use_post_raft_procedures"),
            kv("view_builder_version", "20"),
        ])
    }
}

struct Versions;

impl VirtualTable for Versions {
    fn rows(&self, _t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
        Ok(vec![vec![
            ("key", val::text("local")),
            ("build_id", val::text(SCYLLA_BUILD_ID)),
            ("build_mode", val::text("release")),
            ("version", val::text(SCYLLA_VERSION)),
        ]])
    }
}

/// Size estimates per table and token range. Estimates are zero: a database's data lives in
/// object storage and the counts are not tracked per range.
struct SizeEstimates {
    /// `table_estimates` (Cassandra): one set of rows per range type.
    range_types: bool,
}

impl VirtualTable for SizeEstimates {
    fn rows(&self, _t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        let toks = tokens(&ctx.cfg.host_id(), ctx.cfg.num_tokens);
        let mut bounds: Vec<(i64, i64)> = toks.windows(2).map(|w| (w[0], w[1])).collect();
        if let (Some(first), Some(last)) = (toks.first(), toks.last()) {
            bounds.push((*last, i64::MIN));
            bounds.push((i64::MIN, *first));
        }
        let ranges: Vec<(String, String)> = bounds
            .iter()
            .map(|(s, e)| (s.to_string(), e.to_string()))
            .collect();
        // Each range's share of the ring ((start, end], `MIN` as an end is the ring's end), so
        // a table's estimate splits by token width and the shares add up to it exactly.
        let widths: Vec<u128> = bounds
            .iter()
            .map(|&(s, e)| {
                let end = if e == i64::MIN {
                    i64::MAX as i128 + 1
                } else {
                    e as i128
                };
                (end - s as i128).max(0) as u128
            })
            .collect();
        let ring: u128 = widths.iter().sum::<u128>().max(1);
        let split = |total: u64| -> Vec<i64> {
            let mut acc: u128 = 0;
            let mut prev: u128 = 0;
            widths
                .iter()
                .map(|w| {
                    acc += w;
                    let upto = total as u128 * acc / ring;
                    let n = upto - prev;
                    prev = upto;
                    n as i64
                })
                .collect()
        };
        let mut out = Vec::new();
        let scylla = ctx.cfg.dialect.is_scylla();
        for ks in ctx.schema.keyspaces.values() {
            let include = ks.kind == crate::schema::KeyspaceKind::User
                || (scylla
                    && ks.kind == crate::schema::KeyspaceKind::System
                    && ks.replication.short_class() != "LocalStrategy");
            if !include {
                continue;
            }
            for t in ks.tables.values() {
                let types: &[&str] = if self.range_types {
                    &["local_primary", "primary"]
                } else {
                    &[""]
                };
                // Estimates from storage: partitions of SSTables and memtables.
                let stats = ctx
                    .db
                    .store()
                    .get_table(t.table_id())
                    .map(|st| st.stats())
                    .unwrap_or_default();
                let total = stats.partitions_estimate;
                let mean = (stats.uncompressed_bytes + stats.memtable_bytes)
                    .checked_div(total)
                    .unwrap_or(0) as i64;
                let counts = split(total);
                for rt in types {
                    for (i, (s, e)) in ranges.iter().enumerate() {
                        let count = counts.get(i).copied().unwrap_or(0);
                        let mut row: VRow = vec![
                            ("keyspace_name", val::text(&ks.name)),
                            ("table_name", val::text(&t.name)),
                            ("range_start", val::text(s)),
                            ("range_end", val::text(e)),
                            (
                                "mean_partition_size",
                                val::bigint(if count > 0 { mean } else { 0 }),
                            ),
                            ("partitions_count", val::bigint(count)),
                        ];
                        if self.range_types {
                            row.push(("range_type", val::text(rt)));
                        }
                        out.push(row);
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_stable_sorted_and_unique() {
        let a = tokens(&[7; 16], 256);
        assert_eq!(a.len(), 256);
        assert!(a.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(a, tokens(&[7; 16], 256));
        assert_ne!(a, tokens(&[8; 16], 256));
    }
}
