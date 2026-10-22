//! The generated tables drivers, cqlsh and tools read beyond node identity: Cassandra's virtual
//! keyspaces (`system_views.*`, `system_virtual_schema.*`) and `system.prepared_statements`,
//! ScyllaDB's `system.config`, `clients`, `runtime_info`, `token_ring`, `protocol_servers`,
//! `cluster_status`, `load_per_node`, `tablets`, `topology` and the group 0 tables.
//!
//! Values are those of the single virtual node a database presents: its address, host id,
//! tokens and configuration ([`crate::DbConfig`], [`crate::Settings`]); the configuration tables
//! start from the reference image's rows ([`super::rows_data`]) with the database's own settings
//! substituted. Metrics a database doesn't keep (latencies, caches, thread pools) read as a fresh
//! node's zeros. Cassandra's virtual table classes are named per table (`ClientsTable`,
//! `SettingsTable`, `VirtualSchemaKeyspace`, …).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use nosql_types::Dialect;

use super::rows_data;
use super::{Registry, VRow, VirtualContext, VirtualTable, val};
use crate::error::Result;
use crate::schema::{KeyspaceKind, Table};

/// A generated table backed by a function.
struct F(fn(&Table, &VirtualContext) -> Result<Vec<VRow>>);

impl VirtualTable for F {
    fn rows(&self, t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
        (self.0)(t, ctx)
    }
}

pub fn register(r: &mut Registry, d: Dialect) {
    let mut add = |ks: &str, t: &str, f: fn(&Table, &VirtualContext) -> Result<Vec<VRow>>| {
        r.add(ks, t, Arc::new(F(f)));
    };
    match d {
        Dialect::Cassandra => {
            add("system_views", "clients", cassandra_clients);
            add("system_views", "settings", settings);
            add("system_views", "system_properties", system_properties);
            add("system_views", "thread_pools", thread_pools);
            add("system_views", "caches", caches);
            add("system_views", "batch_metrics", batch_metrics);
            add("system_views", "cql_metrics", cql_metrics);
            add("system_views", "cidr_filtering_metrics_counts", cidr_counts);
            add(
                "system_views",
                "cidr_filtering_metrics_latencies",
                cidr_latencies,
            );
            for t in [
                "coordinator_read_latency",
                "coordinator_scan_latency",
                "coordinator_write_latency",
                "local_read_latency",
                "local_scan_latency",
                "local_write_latency",
            ] {
                add("system_views", t, latency);
            }
            for t in ["disk_usage", "max_partition_size", "max_sstable_size"] {
                add("system_views", t, mebibytes);
            }
            add("system_views", "max_sstable_duration", max_sstable_duration);
            add("system_views", "rows_per_read", per_read);
            add("system_views", "tombstones_per_read", per_read);
            add("system_views", "gossip_info", gossip_info);
            add("system_virtual_schema", "keyspaces", virtual_keyspaces);
            add("system_virtual_schema", "tables", virtual_tables);
            add("system_virtual_schema", "columns", virtual_columns);
            add("system", "prepared_statements", prepared_statements);
        }
        Dialect::Scylla => {
            add("system", "clients", scylla_clients);
            add("system", "config", config);
            add("system", "runtime_info", runtime_info);
            add("system", "token_ring", token_ring);
            add("system", "protocol_servers", protocol_servers);
            add("system", "cluster_status", cluster_status);
            add("system", "load_per_node", load_per_node);
            add("system", "tablets", tablets);
            add("system", "topology", topology);
            add("system", "discovery", discovery);
            add("system", "raft_state", raft_state);
            add("system", "raft_snapshot_config", raft_state);
            add("system", "raft_snapshots", raft_snapshots);
        }
    }
}

fn text_map(items: &[(String, String)]) -> Option<Bytes> {
    Some(nosql_types::pack_map(
        items.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())),
    ))
}

// ---------------------------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------------------------

/// Cassandra `ClientsTable`.
fn cassandra_clients(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let mut out = Vec::new();
    for c in ctx.db.clients() {
        let Some(addr) = c.info.address else {
            continue;
        };
        let tls = c.tls.lock().clone();
        let user = c.user.lock().clone();
        out.push(vec![
            ("address", val::inet(&addr.ip())),
            ("port", val::int(addr.port() as i32)),
            ("client_options", text_map(&c.info.options)),
            (
                "connection_stage",
                val::text(if c.ready.load(Ordering::Relaxed) {
                    "ready"
                } else {
                    "authenticating"
                }),
            ),
            (
                "driver_name",
                c.info.driver_name.as_deref().and_then(val::text),
            ),
            (
                "driver_version",
                c.info.driver_version.as_deref().and_then(val::text),
            ),
            ("hostname", val::text(&addr.ip().to_string())),
            (
                "keyspace_name",
                c.keyspace.lock().as_deref().and_then(val::text),
            ),
            ("protocol_version", val::int(c.info.protocol_version as i32)),
            (
                "request_count",
                val::bigint(c.requests.load(Ordering::Relaxed) as i64),
            ),
            (
                "ssl_cipher_suite",
                tls.as_ref().and_then(|t| val::text(&t.cipher_suite)),
            ),
            ("ssl_enabled", val::boolean(tls.is_some())),
            (
                "ssl_protocol",
                tls.as_ref().and_then(|t| val::text(&t.protocol)),
            ),
            (
                "username",
                val::text(user.as_deref().unwrap_or("anonymous")),
            ),
        ]);
    }
    Ok(out)
}

/// ScyllaDB `system.clients`.
fn scylla_clients(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let mut out = Vec::new();
    for c in ctx.db.clients() {
        let Some(addr) = c.info.address else {
            continue;
        };
        let tls = c.tls.lock().clone();
        let user = c.user.lock().clone();
        out.push(vec![
            ("address", val::inet(&addr.ip())),
            ("port", val::int(addr.port() as i32)),
            (
                "client_type",
                val::text(c.info.client_type.as_deref().unwrap_or("cql")),
            ),
            ("client_options", text_map(&c.info.options)),
            (
                "connection_stage",
                val::text(if c.ready.load(Ordering::Relaxed) {
                    "READY"
                } else {
                    "AUTHENTICATING"
                }),
            ),
            (
                "driver_name",
                c.info.driver_name.as_deref().and_then(val::text),
            ),
            (
                "driver_version",
                c.info.driver_version.as_deref().and_then(val::text),
            ),
            ("protocol_version", val::int(c.info.protocol_version as i32)),
            ("scheduling_group", val::text("sl:default")),
            ("shard_id", val::int(c.shard.load(Ordering::Relaxed) as i32)),
            (
                "ssl_cipher_suite",
                tls.as_ref().and_then(|t| val::text(&t.cipher_suite)),
            ),
            ("ssl_enabled", val::boolean(tls.is_some())),
            (
                "ssl_protocol",
                tls.as_ref().and_then(|t| val::text(&t.protocol)),
            ),
            (
                "username",
                val::text(user.as_deref().unwrap_or("anonymous")),
            ),
        ]);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// `DataStorageSpec` as `SettingsTable` prints it (`5KiB`).
fn storage_size(bytes: u64) -> String {
    if bytes.is_multiple_of(1024) {
        format!("{}KiB", bytes >> 10)
    } else {
        format!("{bytes}B")
    }
}

fn short_class(c: &str) -> &str {
    c.rsplit('.').next().unwrap_or(c)
}

/// The settings of this database that differ from the image: name → value.
fn cassandra_overrides(ctx: &VirtualContext) -> BTreeMap<&'static str, Option<String>> {
    let cfg = ctx.cfg;
    let live = ctx.db.settings();
    let s = &*live;
    let addr = cfg.address.to_string();
    let mut m: BTreeMap<&'static str, Option<String>> = BTreeMap::new();
    let mut set = |k: &'static str, v: String| {
        m.insert(k, Some(v));
    };
    set("cluster_name", cfg.cluster_name.clone());
    for k in [
        "listen_address",
        "rpc_address",
        "broadcast_address",
        "broadcast_rpc_address",
    ] {
        set(k, addr.clone());
    }
    set("seed_provider.parameters", format!("{{seeds={addr}}}"));
    // The database's memtable cap (its plan's share of the node's memtable budget).
    let memtable = ctx.db.store().memory().cap() >> 20;
    set("memtable_heap_space", format!("{memtable}MiB"));
    set("memtable_heap_space_in_mb", memtable.to_string());
    set("native_transport_port", cfg.native_port.to_string());
    set("num_tokens", cfg.num_tokens.to_string());
    for (k, v) in [
        ("authenticator", &s.authenticator),
        ("authorizer", &s.authorizer),
        ("network_authorizer", &s.network_authorizer),
    ] {
        set(k, short_class(v).to_string());
    }
    set(
        "authenticator.class_name",
        short_class(&s.authenticator).to_string(),
    );
    set(
        "authorizer.class_name",
        short_class(&s.authorizer).to_string(),
    );
    set(
        "network_authorizer.class_name",
        short_class(&s.network_authorizer).to_string(),
    );
    set(
        "batch_size_warn_threshold",
        storage_size(s.batch_size_warn_threshold),
    );
    set(
        "batch_size_warn_threshold_in_kb",
        (s.batch_size_warn_threshold / 1024).to_string(),
    );
    set(
        "batch_size_fail_threshold",
        storage_size(s.batch_size_fail_threshold),
    );
    set(
        "batch_size_fail_threshold_in_kb",
        (s.batch_size_fail_threshold / 1024).to_string(),
    );
    set(
        "unlogged_batch_across_partitions_warn_threshold",
        s.unlogged_batch_across_partitions_warn_threshold
            .to_string(),
    );
    set(
        "tombstone_warn_threshold",
        s.tombstone_warn_threshold.to_string(),
    );
    set(
        "tombstone_failure_threshold",
        s.tombstone_failure_threshold.to_string(),
    );
    set(
        "materialized_views_enabled",
        s.materialized_views_enabled.to_string(),
    );
    set(
        "enable_materialized_views",
        s.materialized_views_enabled.to_string(),
    );
    set("sasi_indexes_enabled", s.sasi_indexes_enabled.to_string());
    set("enable_sasi_indexes", s.sasi_indexes_enabled.to_string());
    set(
        "user_defined_functions_enabled",
        s.user_defined_functions_enabled.to_string(),
    );
    set(
        "enable_user_defined_functions",
        s.user_defined_functions_enabled.to_string(),
    );
    set(
        "dynamic_data_masking_enabled",
        s.dynamic_data_masking_enabled.to_string(),
    );
    set(
        "drop_compact_storage_enabled",
        s.drop_compact_storage_enabled.to_string(),
    );
    set(
        "enable_drop_compact_storage",
        s.drop_compact_storage_enabled.to_string(),
    );
    set(
        "allow_filtering_enabled",
        s.allow_filtering_enabled.to_string(),
    );
    for (k, ms) in [
        ("permissions_validity", s.permissions_validity_ms),
        ("roles_validity", s.roles_validity_ms),
        ("credentials_validity", s.credentials_validity_ms),
    ] {
        set(k, format!("{ms}ms"));
    }
    set(
        "permissions_validity_in_ms",
        s.permissions_validity_ms.to_string(),
    );
    set("roles_validity_in_ms", s.roles_validity_ms.to_string());
    set(
        "credentials_validity_in_ms",
        s.credentials_validity_ms.to_string(),
    );
    m
}

/// Cassandra `SettingsTable`.
fn settings(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let o = cassandra_overrides(ctx);
    Ok(rows_data::CASSANDRA_SETTINGS
        .iter()
        .map(|(name, v)| {
            let v = match o.get(name) {
                Some(v) => v.clone(),
                None => v.map(str::to_string),
            };
            vec![
                ("name", val::text(name)),
                ("value", v.as_deref().and_then(val::text)),
            ]
        })
        .collect())
}

/// Cassandra `SystemPropertiesTable`.
fn system_properties(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(rows_data::CASSANDRA_PROPERTIES
        .iter()
        .map(|(n, v)| vec![("name", val::text(n)), ("value", val::text(v))])
        .collect())
}

/// ScyllaDB `system.config`: the image's rows with this database's values (`source` becomes
/// `config` where it differs from the default).
fn config(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    let live = ctx.db.settings();
    let s = &*live;
    let json = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
    let class = |c: &str| {
        if c.contains('.') {
            json(c)
        } else {
            json(&format!("org.apache.cassandra.auth.{c}"))
        }
    };
    let addr = json(&cfg.address.to_string());
    let mut o: BTreeMap<&str, String> = BTreeMap::new();
    o.insert("cluster_name", json(&cfg.cluster_name));
    for k in [
        "listen_address",
        "rpc_address",
        "broadcast_address",
        "broadcast_rpc_address",
        "alternator_address",
    ] {
        o.insert(k, addr.clone());
    }
    o.insert("native_transport_port", cfg.native_port.to_string());
    o.insert("num_tokens", cfg.num_tokens.to_string());
    o.insert("authenticator", class(&s.authenticator));
    o.insert("authorizer", class(&s.authorizer));
    o.insert(
        "batch_size_warn_threshold_in_kb",
        (s.batch_size_warn_threshold / 1024).to_string(),
    );
    o.insert(
        "batch_size_fail_threshold_in_kb",
        (s.batch_size_fail_threshold / 1024).to_string(),
    );
    o.insert(
        "tombstone_warn_threshold",
        s.tombstone_warn_threshold.to_string(),
    );
    o.insert(
        "tombstone_failure_threshold",
        s.tombstone_failure_threshold.to_string(),
    );
    o.insert(
        "enable_user_defined_functions",
        s.user_defined_functions_enabled.to_string(),
    );
    if s.user_defined_functions_enabled {
        o.insert("experimental_features", "[\"udf\"]".into());
    }
    o.insert(
        "enable_create_table_with_compact_storage",
        s.compact_storage_enabled.to_string(),
    );
    o.insert(
        "max_partition_key_restrictions_per_query",
        s.max_partition_key_restrictions_per_query.to_string(),
    );
    o.insert(
        "max_clustering_key_restrictions_per_query",
        s.max_clustering_key_restrictions_per_query.to_string(),
    );
    o.insert(
        "minimum_replication_factor_warn_threshold",
        s.minimum_replication_factor_warn_threshold.to_string(),
    );
    o.insert(
        "tablets_mode_for_new_keyspaces",
        json(if s.tablets_by_default {
            "enabled"
        } else {
            "disabled"
        }),
    );
    crate::guardrails::config_table::values(s, &mut o);
    Ok(rows_data::SCYLLA_CONFIG
        .iter()
        .map(|(name, source, ty, v)| {
            let source = &crate::guardrails::config_table::source(s, name, source);
            let (source, v) = match o.get(name) {
                Some(ov) if ov != v => (
                    if *source == "default" {
                        "config"
                    } else {
                        source
                    },
                    ov.clone(),
                ),
                _ => (*source, v.to_string()),
            };
            vec![
                ("name", val::text(name)),
                ("source", val::text(source)),
                ("type", val::text(ty)),
                ("value", val::text(&v)),
            ]
        })
        .collect())
}

/// ScyllaDB `system.runtime_info`.
fn runtime_info(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let uptime = (crate::database::now_secs() - ctx.started_at).max(0);
    let p = ctx.db.trace_probability();
    let probability = if p == 0.0 {
        "0".to_string()
    } else {
        p.to_string()
    };
    let rows: Vec<(&str, &str, String)> = vec![
        ("cache", "entries", "0".into()),
        ("cache", "hit_rate_recent", "0".into()),
        ("cache", "hit_rate_total", "0".into()),
        ("cache", "hits", "0".into()),
        ("cache", "memory_free", "0".into()),
        ("cache", "memory_total", "0".into()),
        ("cache", "memory_used", "0".into()),
        ("cache", "misses", "0".into()),
        ("cache", "requests_recent", "0".into()),
        ("cache", "requests_total", "0".into()),
        ("generic", "gossip_active", "true".into()),
        ("generic", "incremental_backup_enabled", "false".into()),
        ("generic", "load", "0".into()),
        ("generic", "trace_probability", probability),
        ("generic", "uptime", format!("{uptime} seconds")),
        ("memory", "free", "0".into()),
        ("memory", "total", "0".into()),
        ("memory", "used", "0".into()),
        ("memtable", "entries", "0".into()),
        ("memtable", "memory_free", "0".into()),
        ("memtable", "memory_total", "0".into()),
        ("memtable", "memory_used", "0".into()),
    ];
    Ok(rows
        .into_iter()
        .map(|(g, i, v)| {
            vec![
                ("group", val::text(g)),
                ("item", val::text(i)),
                ("value", val::text(&v)),
            ]
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Metrics (a fresh node's zeros)
// ---------------------------------------------------------------------------------------------

fn thread_pools(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(rows_data::CASSANDRA_THREAD_POOLS
        .iter()
        .map(|(n, limit)| {
            vec![
                ("name", val::text(n)),
                ("active_tasks", val::int(0)),
                ("active_tasks_limit", val::int(*limit)),
                ("blocked_tasks", val::bigint(0)),
                ("blocked_tasks_all_time", val::bigint(0)),
                ("completed_tasks", val::bigint(0)),
                ("pending_tasks", val::int(0)),
            ]
        })
        .collect())
}

fn caches(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok([
        ("counters", 16_777_216i64),
        ("keys", 33_554_432),
        ("rows", 0),
    ]
    .iter()
    .map(|(n, cap)| {
        vec![
            ("name", val::text(n)),
            ("capacity_bytes", val::bigint(*cap)),
            ("entry_count", val::int(0)),
            ("hit_count", val::bigint(0)),
            ("hit_ratio", val::double(f64::NAN)),
            ("recent_hit_rate_per_second", val::bigint(0)),
            ("recent_request_rate_per_second", val::bigint(0)),
            ("request_count", val::bigint(0)),
            ("size_bytes", val::bigint(0)),
        ]
    })
    .collect())
}

fn batch_metrics(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok([
        "partitions_per_counter_batch",
        "partitions_per_logged_batch",
        "partitions_per_unlogged_batch",
    ]
    .iter()
    .map(|n| {
        vec![
            ("name", val::text(n)),
            ("max", val::bigint(0)),
            ("p50th", val::double(0.0)),
            ("p999th", val::double(0.0)),
            ("p99th", val::double(0.0)),
        ]
    })
    .collect())
}

fn cql_metrics(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let prepared = ctx.db.prepared_statements().len() as f64;
    Ok([
        ("prepared_statements_count", prepared),
        ("prepared_statements_evicted", 0.0),
        ("prepared_statements_executed", 0.0),
        ("prepared_statements_ratio", 0.0),
        ("regular_statements_executed", 0.0),
    ]
    .iter()
    .map(|(n, v)| vec![("name", val::text(n)), ("value", val::double(*v))])
    .collect())
}

fn cidr_counts(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(vec![vec![
        ("name", val::text("CIDR groups cache reload count")),
        ("value", val::bigint(0)),
    ]])
}

fn cidr_latencies(_t: &Table, _ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok([
        "CIDR checks latency (ns)",
        "CIDR groups cache reload latency (ns)",
        "Lookup IP in CIDR groups cache latency (ns)",
    ]
    .iter()
    .map(|n| {
        vec![
            ("name", val::text(n)),
            ("max", val::double(0.0)),
            ("p50th", val::double(0.0)),
            ("p95th", val::double(0.0)),
            ("p999th", val::double(0.0)),
            ("p99th", val::double(0.0)),
        ]
    })
    .collect())
}

/// Every table of the non-virtual keyspaces (the per-table metric tables' rows).
fn stored_tables<'c>(ctx: &'c VirtualContext) -> Vec<(&'c str, &'c str)> {
    let mut out = Vec::new();
    for k in ctx.schema.keyspaces.values() {
        if k.kind == KeyspaceKind::Virtual {
            continue;
        }
        for t in k.tables.values() {
            out.push((k.name.as_str(), t.name.as_str()));
        }
    }
    out
}

fn latency(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(stored_tables(ctx)
        .into_iter()
        .map(|(k, t)| {
            vec![
                ("keyspace_name", val::text(k)),
                ("table_name", val::text(t)),
                ("count", val::bigint(0)),
                ("max_ms", val::double(0.0)),
                ("p50th_ms", val::double(0.0)),
                ("p99th_ms", val::double(0.0)),
                ("per_second", val::double(0.0)),
            ]
        })
        .collect())
}

fn mebibytes(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(stored_tables(ctx)
        .into_iter()
        .map(|(k, t)| {
            vec![
                ("keyspace_name", val::text(k)),
                ("table_name", val::text(t)),
                ("mebibytes", val::bigint(0)),
            ]
        })
        .collect())
}

fn max_sstable_duration(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(stored_tables(ctx)
        .into_iter()
        .map(|(k, t)| {
            vec![
                ("keyspace_name", val::text(k)),
                ("table_name", val::text(t)),
                ("max_sstable_duration", val::bigint(0)),
            ]
        })
        .collect())
}

fn per_read(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(stored_tables(ctx)
        .into_iter()
        .map(|(k, t)| {
            vec![
                ("keyspace_name", val::text(k)),
                ("table_name", val::text(t)),
                ("count", val::bigint(0)),
                ("max", val::double(0.0)),
                ("p50th", val::double(0.0)),
                ("p99th", val::double(0.0)),
            ]
        })
        .collect())
}

/// Cassandra `GossipInfoTable`: the node's application states.
fn gossip_info(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    let addr = cfg.address.to_string();
    let host = crate::uuid::format(&cfg.host_id());
    let toks = super::local::tokens(&cfg.host_id(), cfg.num_tokens);
    let status = format!("NORMAL,{}", toks.first().copied().unwrap_or_default());
    let (release, _, _) = super::local::versions(cfg.dialect);
    Ok(vec![vec![
        ("address", val::inet(&cfg.address)),
        ("port", val::int(7000)),
        ("dc", val::text(&cfg.datacenter)),
        ("dc_version", val::int(8)),
        ("generation", val::int(ctx.started_at as i32)),
        ("heartbeat", val::int(1)),
        ("host_id", val::text(&host)),
        ("host_id_version", val::int(2)),
        ("hostname", val::text(&addr)),
        ("load", val::text("0.0")),
        ("load_version", val::int(1)),
        (
            "native_address_and_port",
            val::text(&format!("{addr}:{}", cfg.native_port)),
        ),
        ("native_address_and_port_version", val::int(3)),
        ("net_version", val::text("12")),
        ("net_version_version", val::int(1)),
        ("rack", val::text(&cfg.rack)),
        ("rack_version", val::int(10)),
        ("release_version", val::text(release)),
        ("release_version_version", val::int(5)),
        ("rpc_address", val::text(&addr)),
        ("rpc_address_version", val::int(4)),
        ("rpc_ready", val::text("true")),
        ("rpc_ready_version", val::int(1)),
        (
            "schema",
            val::text(&crate::uuid::format(&ctx.schema.version)),
        ),
        ("schema_version", val::int(1)),
        ("sstable_versions", val::text("big-nb")),
        ("sstable_versions_version", val::int(6)),
        ("status", val::text(&status)),
        ("status_version", val::int(1)),
        ("status_with_port", val::text(&status)),
        ("status_with_port_version", val::int(1)),
        ("tokens_version", val::int(1)),
    ]])
}

/// Cassandra `system.prepared_statements`: the prepared statements of the database.
fn prepared_statements(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(ctx
        .db
        .prepared_statements()
        .iter()
        .map(|p| {
            vec![
                ("prepared_id", Some(Bytes::copy_from_slice(&p.id))),
                ("logged_keyspace", p.keyspace.as_deref().and_then(val::text)),
                ("query_string", val::text(&p.text)),
            ]
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// system_virtual_schema
// ---------------------------------------------------------------------------------------------

fn virtual_keyspaces(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(ctx
        .schema
        .keyspaces
        .values()
        .filter(|k| k.kind == KeyspaceKind::Virtual)
        .map(|k| vec![("keyspace_name", val::text(&k.name))])
        .collect())
}

fn virtual_tables(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let mut out = Vec::new();
    for k in ctx
        .schema
        .keyspaces
        .values()
        .filter(|k| k.kind == KeyspaceKind::Virtual)
    {
        for t in k.tables.values() {
            out.push(vec![
                ("keyspace_name", val::text(&k.name)),
                ("table_name", val::text(&t.name)),
                ("comment", val::text(&t.params.comment)),
            ]);
        }
    }
    Ok(out)
}

fn virtual_columns(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let mut out = Vec::new();
    for k in ctx
        .schema
        .keyspaces
        .values()
        .filter(|k| k.kind == KeyspaceKind::Virtual)
    {
        for t in k.tables.values() {
            for c in &t.columns {
                out.push(vec![
                    ("keyspace_name", val::text(&k.name)),
                    ("table_name", val::text(&t.name)),
                    ("column_name", val::text(&c.name)),
                    (
                        "clustering_order",
                        val::text(c.clustering_order(Dialect::Cassandra)),
                    ),
                    ("column_name_bytes", Some(Bytes::from(c.name.clone()))),
                    ("kind", val::text(c.kind.name())),
                    ("position", val::int(c.position)),
                    ("type", val::text(&nosql_types::cql_name(&c.ty))),
                ]);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// ScyllaDB topology
// ---------------------------------------------------------------------------------------------

/// `system.token_ring`: every token range of every keyspace (vnode keyspaces as `<ALL>`,
/// tablets keyspaces per table and tablet).
fn token_ring(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    let toks = super::local::tokens(&cfg.host_id(), cfg.num_tokens);
    let mut ring: Vec<(String, String)> = Vec::new();
    if let (Some(first), Some(last)) = (toks.first(), toks.last()) {
        ring.push((last.to_string(), first.to_string()));
        for w in toks.windows(2) {
            ring.push((w[0].to_string(), w[1].to_string()));
        }
    }
    let mut out = Vec::new();
    let row = |ks: &str, table: &str, s: &str, e: &str| -> VRow {
        vec![
            ("keyspace_name", val::text(ks)),
            ("table_name", val::text(table)),
            ("start_token", val::text(s)),
            ("endpoint", val::inet(&cfg.address)),
            ("dc", val::text(&cfg.datacenter)),
            ("end_token", val::text(e)),
            ("rack", val::text(&cfg.rack)),
        ]
    };
    for k in ctx.schema.keyspaces.values() {
        if k.replication.short_class() == "LocalStrategy" || k.kind == KeyspaceKind::Virtual {
            continue;
        }
        match k.tablets {
            None => {
                for (s, e) in &ring {
                    out.push(row(&k.name, "<ALL>", s, e));
                }
            }
            Some(n) => {
                for t in k.tables.values() {
                    let bounds = tablet_bounds(tablet_count(n));
                    let mut prev = i64::MIN.to_string();
                    for b in bounds {
                        let e = b.to_string();
                        out.push(row(&k.name, &t.name, &prev, &e));
                        prev = e;
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Tablets of a new table: the keyspace's `initial` (rounded up to a power of two), else
/// ScyllaDB's default for a single-shard node.
fn tablet_count(initial: i32) -> u32 {
    if initial > 0 {
        (initial as u32).next_power_of_two()
    } else {
        16
    }
}

/// The last token of each of `n` equal tablets.
fn tablet_bounds(n: u32) -> Vec<i64> {
    let n = n.max(1) as i128;
    let span = (u64::MAX as i128) + 1;
    (1..=n)
        .map(|i| (i64::MIN as i128 + span * i / n - 1) as i64)
        .collect()
}

/// `system.tablets`: the tablet map of tablets keyspaces' tables.
fn tablets(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let host = ctx.cfg.host_id();
    let replicas = {
        let pair = nosql_types::pack_collection([&host[..], &0i32.to_be_bytes()[..]]);
        // A tuple<uuid, int> serialises its fields like collection elements without a count.
        let tuple = pair.slice(4..);
        Some(nosql_types::pack_collection([&tuple[..]]))
    };
    let mut out = Vec::new();
    for k in ctx.schema.user_keyspaces() {
        let Some(initial) = k.tablets else {
            continue;
        };
        for t in k.tables.values() {
            // A CDC table and its log are one tablet group (the log names its base).
            let base = t
                .name
                .strip_suffix("_scylla_cdc_log")
                .and_then(|b| k.tables.get(b))
                .filter(|_| crate::cdc::is_log(k, t));
            let group = match base {
                Some(b) => Some((b.as_ref(), t.as_ref())),
                None => crate::cdc::log_of(k, &t.name).map(|l| (t.as_ref(), l.as_ref())),
            };
            let n = match group {
                Some((b, l)) => crate::cdc::group_tablet_count(k, &b.params, &l.params) as u32,
                None => tablet_count(initial),
            };
            for last in tablet_bounds(n) {
                out.push(vec![
                    ("table_id", val::uuid(&t.id)),
                    ("base_table", base.and_then(|b| val::uuid(&b.id))),
                    ("last_token", val::bigint(last)),
                    ("keyspace_name", val::text(&k.name)),
                    ("table_name", val::text(&t.name)),
                    ("tablet_count", val::int(n as i32)),
                    ("resize_seq_number", val::bigint(0)),
                    ("resize_type", val::text("none")),
                    ("replicas", replicas.clone()),
                    ("sstables_repaired_at", val::bigint(0)),
                ]);
            }
        }
    }
    Ok(out)
}

fn protocol_servers(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let a = ctx.cfg.address;
    let cql = [format!("{a}:{}", ctx.cfg.native_port), format!("{a}:19042")];
    let alternator = [format!("{a}:8000")];
    Ok(vec![
        vec![
            ("name", val::text("native transport")),
            (
                "listen_addresses",
                val::text_list(cql.iter().map(String::as_str)),
            ),
            ("protocol", val::text("cql")),
            ("protocol_version", val::text("3.3.1")),
        ],
        vec![
            ("name", val::text("alternator")),
            (
                "listen_addresses",
                val::text_list(alternator.iter().map(String::as_str)),
            ),
            ("protocol", val::text("dynamodb")),
            ("protocol_version", val::text("2012-08-10")),
        ],
    ])
}

fn cluster_status(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    Ok(vec![vec![
        ("peer", val::inet(&cfg.address)),
        ("dc", val::text(&cfg.datacenter)),
        ("draining", val::boolean(false)),
        ("excluded", val::boolean(false)),
        ("host_id", val::uuid(&cfg.host_id())),
        ("load", val::text("0")),
        ("owns", Some(Bytes::copy_from_slice(&1.0f32.to_be_bytes()))),
        ("rack", val::text(&cfg.rack)),
        ("status", val::text("NORMAL")),
        ("tokens", val::int(cfg.num_tokens as i32)),
        ("up", val::boolean(true)),
    ]])
}

fn load_per_node(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    let tablets: i64 = ctx
        .schema
        .user_keyspaces()
        .filter_map(|k| k.tablets.map(|n| (k, n)))
        .map(|(k, n)| k.tables.len() as i64 * tablet_count(n) as i64)
        .sum();
    Ok(vec![vec![
        ("node", val::uuid(&cfg.host_id())),
        ("dc", val::text(&cfg.datacenter)),
        ("draining", val::boolean(false)),
        ("effective_capacity", val::bigint(0)),
        ("excluded", val::boolean(false)),
        ("ip", val::inet(&cfg.address)),
        ("rack", val::text(&cfg.rack)),
        ("storage_allocated_load", val::bigint(0)),
        ("storage_allocated_utilization", val::double(0.0)),
        ("storage_capacity", val::bigint(0)),
        ("storage_load", val::bigint(0)),
        ("storage_utilization", val::double(0.0)),
        ("tablets_allocated", val::bigint(tablets)),
        (
            "tablets_allocated_per_shard",
            val::double(tablets as f64 / cfg.settings.shards.max(1) as f64),
        ),
        ("up", val::boolean(true)),
    ]])
}

/// `system.topology`: the one normal node.
fn topology(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    let cfg = ctx.cfg;
    let features: Vec<&str> = super::local::SCYLLA_FEATURES.split(',').collect();
    let toks: Vec<String> = super::local::tokens(&cfg.host_id(), cfg.num_tokens)
        .iter()
        .map(|t| t.to_string())
        .collect();
    let (release, _, _) = super::local::versions(cfg.dialect);
    Ok(vec![vec![
        ("key", val::text("topology")),
        ("host_id", val::uuid(&cfg.host_id())),
        ("enabled_features", val::text_set(features.iter().copied())),
        ("fence_version", val::bigint(1)),
        ("tablet_balancing_enabled", val::boolean(true)),
        ("upgrade_state", val::text("done")),
        ("version", val::bigint(1)),
        ("cleanup_status", val::text("clean")),
        ("datacenter", val::text(&cfg.datacenter)),
        ("ignore_msb", val::int(12)),
        ("node_state", val::text("normal")),
        ("num_tokens", val::int(cfg.num_tokens as i32)),
        ("rack", val::text(&cfg.rack)),
        ("release_version", val::text(release)),
        ("shard_count", val::int(cfg.settings.shards as i32)),
        (
            "supported_features",
            val::text_set(features.iter().copied()),
        ),
        ("tokens", val::text_set(toks.iter().map(String::as_str))),
        ("tokens_string", val::text("")),
    ]])
}

fn group0(cfg: &crate::DbConfig) -> [u8; 16] {
    crate::uuid::v5(
        &crate::uuid::NAMESPACE_ROLL,
        format!("{}/group0", cfg.id).as_bytes(),
    )
}

fn discovery(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(vec![vec![
        ("key", val::text("peers")),
        ("ip_addr", val::inet(&ctx.cfg.address)),
        ("raft_server_id", val::uuid(&ctx.cfg.host_id())),
    ]])
}

fn raft_state(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(vec![vec![
        ("group_id", val::uuid(&group0(ctx.cfg))),
        ("disposition", val::text("CURRENT")),
        ("server_id", val::uuid(&ctx.cfg.host_id())),
        ("can_vote", val::boolean(true)),
    ]])
}

fn raft_snapshots(_t: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>> {
    Ok(vec![vec![
        ("group_id", val::uuid(&group0(ctx.cfg))),
        ("idx", val::bigint(1)),
        (
            "snapshot_id",
            val::uuid(&crate::uuid::v5(&group0(ctx.cfg), b"snapshot")),
        ),
        ("term", val::bigint(0)),
    ]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_sizes_print_like_cassandra() {
        assert_eq!(storage_size(5 * 1024), "5KiB");
        assert_eq!(storage_size(50 * 1024), "50KiB");
        assert_eq!(storage_size(100), "100B");
    }

    #[test]
    fn tablets_split_the_ring_evenly() {
        let b = tablet_bounds(16);
        assert_eq!(b.len(), 16);
        assert_eq!(b[0], -8070450532247928833);
        assert_eq!(b[1], -6917529027641081857);
        assert_eq!(*b.last().unwrap(), i64::MAX);
        assert_eq!(tablet_count(0), 16);
        assert_eq!(tablet_count(3), 4);
    }
}
