//! Cluster health views beyond `_cluster/health`: the health report (`_health_report`), the
//! desired balance (`_internal/desired_balance`) and node removal prevalidation
//! (`_internal/prevalidate_node_removal`), shaped as Elasticsearch 9.5.5 reports them for a
//! one-node cluster.
//!
//! Shard availability comes from the same model as `_cluster/health`: every open index's
//! primaries are started unless [`Cluster::unassigned_reason`](crate::Cluster::unassigned_reason)
//! says otherwise, and replicas stay unassigned because there is no second node.

use crate::cluster::Unassigned;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use crate::rest::{Ctx, Reply};

/// Shard copies of the cluster, as the shards availability indicator counts them.
struct Availability<'a> {
    started_primaries: usize,
    unassigned_primaries: usize,
    unassigned_replicas: usize,
    /// Indices with unassigned primaries, and why.
    red: Vec<(&'a IndexMeta, Unassigned)>,
    /// Indices with unassigned replicas only, by data tier.
    replicas: Vec<(&'a IndexMeta, String)>,
}

fn availability<'a>(ctx: &Ctx, st: &'a ClusterState) -> Availability<'a> {
    let mut a = Availability { started_primaries: 0, unassigned_primaries: 0, unassigned_replicas: 0, red: Vec::new(), replicas: Vec::new() };
    for idx in st.metadata.indices.values() {
        let shards = idx.number_of_shards() as usize;
        let replicas = idx.number_of_replicas() as usize;
        match ctx.cluster.unassigned_reason(st, idx) {
            Some(reason) => {
                a.unassigned_primaries += shards;
                a.unassigned_replicas += shards * replicas;
                a.red.push((idx, reason));
            }
            None => {
                a.started_primaries += shards;
                a.unassigned_replicas += shards * replicas;
                if replicas > 0 {
                    a.replicas.push((idx, tier_of(idx)));
                }
            }
        }
    }
    a
}

/// The first data tier an index prefers (`_tier_preference`), `data_content` by default.
fn tier_of(idx: &IndexMeta) -> String {
    idx.settings
        .get("index.routing.allocation.include._tier_preference")
        .and_then(|t| t.split(',').map(str::trim).find(|t| !t.is_empty()))
        .unwrap_or("data_content")
        .to_string()
}

/// `getTruncatedIndices`: sorted by `index.priority` (highest first) then name, at most ten.
fn truncated(indices: &[&IndexMeta]) -> String {
    let mut v: Vec<&&IndexMeta> = indices.iter().collect();
    let prio = |i: &IndexMeta| i.settings.get("index.priority").and_then(|p| p.parse::<i64>().ok()).unwrap_or(1);
    v.sort_by(|a, b| prio(b).cmp(&prio(a)).then_with(|| a.name.cmp(&b.name)));
    let mut s: Vec<&str> = v.iter().take(10).map(|i| i.name.as_str()).collect();
    if v.len() > 10 {
        s.push("...");
    }
    s.join(", ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

fn shards_availability(ctx: &Ctx, st: &ClusterState, verbose: bool, size: usize) -> Value {
    let a = availability(ctx, st);
    let status = if a.unassigned_primaries > 0 {
        "red"
    } else if a.unassigned_replicas > 0 {
        "yellow"
    } else {
        "green"
    };
    let parts: Vec<String> = [
        (a.unassigned_primaries, "unavailable primary shard", "unavailable primary shards"),
        (a.unassigned_replicas, "unavailable replica shard", "unavailable replica shards"),
    ]
    .iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| plural(*n, one, many))
    .collect();
    let symptom = if parts.is_empty() { "This cluster has all shards available.".to_string() } else { format!("This cluster has {}.", parts.join(", ")) };
    let mut m = Map::new();
    m.insert("status".into(), status.into());
    m.insert("symptom".into(), symptom.into());
    if verbose {
        m.insert(
            "details".into(),
            obj! {
                "unassigned_primaries" => a.unassigned_primaries, "initializing_primaries" => 0, "creating_primaries" => 0,
                "restarting_primaries" => 0, "started_primaries" => a.started_primaries,
                "unassigned_replicas" => a.unassigned_replicas, "initializing_replicas" => 0, "creating_replicas" => 0,
                "restarting_replicas" => 0, "started_replicas" => 0,
            },
        );
    }
    let red: Vec<&IndexMeta> = a.red.iter().map(|(i, _)| *i).collect();
    let yellow: Vec<&IndexMeta> = a.replicas.iter().map(|(i, _)| *i).collect();
    let mut impacts = Vec::new();
    if !red.is_empty() {
        impacts.push(obj! {
            "id" => "elasticsearch:health:shards_availability:impact:primary_unassigned",
            "severity" => 1,
            "description" => format!("Cannot add data to {} {} [{}]. Searches might return incomplete results.",
                red.len(), if red.len() == 1 { "index" } else { "indices" }, truncated(&red)),
            "impact_areas" => vec![Value::from("ingest"), Value::from("search")],
        });
    }
    if !yellow.is_empty() {
        impacts.push(obj! {
            "id" => "elasticsearch:health:shards_availability:impact:replica_unassigned",
            "severity" => 2,
            "description" => format!("Searches might be slower than usual. Fewer redundant copies of the data exist on {} {} [{}].",
                yellow.len(), if yellow.len() == 1 { "index" } else { "indices" }, truncated(&yellow)),
            "impact_areas" => vec![Value::from("search")],
        });
    }
    if !impacts.is_empty() {
        m.insert("impacts".into(), Value::Arr(impacts));
    }
    if verbose && size > 0 {
        let mut diagnosis = Vec::new();
        let names = |list: Vec<&IndexMeta>| -> Value {
            let mut n: Vec<&str> = list.iter().map(|i| i.name.as_str()).collect();
            n.sort_unstable();
            Value::Arr(n.into_iter().take(size).map(Value::from).collect())
        };
        let mut tiers: Vec<&str> = a.replicas.iter().map(|(_, t)| t.as_str()).collect();
        tiers.sort_unstable();
        tiers.dedup();
        for tier in tiers {
            let list: Vec<&IndexMeta> = a.replicas.iter().filter(|(_, t)| t == tier).map(|(i, _)| *i).collect();
            diagnosis.push(obj! {
                "id" => format!("elasticsearch:health:shards_availability:diagnosis:increase_tier_capacity_for_allocations:tier:{tier}"),
                "cause" => format!("Elasticsearch isn't allowed to allocate some shards from these indices to any of the nodes in the desired data tier because there are not enough nodes in the [{tier}] tier to allocate each shard copy on a different node."),
                "action" => "Increase the number of nodes in this tier or decrease the number of replica shards in the affected indices.",
                "help_url" => "https://ela.st/tier-capacity",
                "affected_resources" => obj! { "indices" => names(list) },
            });
        }
        for (reason, id, cause, action, url) in [
            (
                Unassigned::IndexAllocationDisabled,
                "enable_index_allocations",
                "Elasticsearch isn't allowed to allocate some shards from these indices because allocation for those shards has been disabled at the index level.",
                "Check that the [index.routing.allocation.enable] index settings are set to [all].",
                "https://ela.st/fix-index-allocation",
            ),
            (
                Unassigned::ClusterAllocationDisabled,
                "enable_cluster_allocations",
                "Elasticsearch isn't allowed to allocate some shards from these indices because allocation for those shards has been disabled at the cluster level.",
                "Check that the [cluster.routing.allocation.enable] cluster setting is set to [all].",
                "https://ela.st/fix-cluster-allocation",
            ),
            (
                Unassigned::AllocationFailed,
                "explain_allocations",
                "Elasticsearch isn't allowed to allocate some shards from these indices to any data nodes in the cluster.",
                "Diagnose the issue by calling the allocation explain API for an index [GET _cluster/allocation/explain]. Choose a node to which you expect a shard to be allocated, find this node in the node-by-node explanation, and address the reasons which prevent Elasticsearch from allocating the shard.",
                "https://ela.st/diagnose-shards",
            ),
        ] {
            let list: Vec<&IndexMeta> = a.red.iter().filter(|(_, r)| *r == reason).map(|(i, _)| *i).collect();
            if !list.is_empty() {
                diagnosis.push(obj! {
                    "id" => format!("elasticsearch:health:shards_availability:diagnosis:{id}"),
                    "cause" => cause, "action" => action, "help_url" => url,
                    "affected_resources" => obj! { "indices" => names(list) },
                });
            }
        }
        if !diagnosis.is_empty() {
            m.insert("diagnosis".into(), Value::Arr(diagnosis));
        }
    }
    Value::Obj(m)
}

/// The indicators in Elasticsearch's order.
const INDICATORS: &[&str] = &[
    "master_is_stable",
    "repository_integrity",
    "disk",
    "shards_capacity",
    "file_settings",
    "cluster_state_encryption",
    "data_stream_lifecycle",
    "slm",
    "shards_availability",
    "ilm",
];

fn indicator(ctx: &Ctx, st: &ClusterState, name: &str, verbose: bool, size: usize) -> Value {
    let node = &ctx.cluster.node;
    let simple = |status: &str, symptom: &str, details: Value| -> Value {
        let mut m = Map::new();
        m.insert("status".into(), status.into());
        m.insert("symptom".into(), symptom.into());
        if verbose && !matches!(&details, Value::Obj(d) if d.is_empty()) {
            m.insert("details".into(), details);
        }
        Value::Obj(m)
    };
    match name {
        "master_is_stable" => {
            let master = obj! { "node_id" => node.id.clone(), "name" => node.name.clone() };
            simple("green", "The cluster has a stable master node", obj! { "current_master" => master.clone(), "recent_masters" => vec![master] })
        }
        "repository_integrity" => {
            let repos = super::snapshots::repository_count(st);
            if repos == 0 {
                simple("green", "No snapshot repositories configured.", obj! {})
            } else {
                simple("green", "No corrupted snapshot repositories.", obj! { "total_repositories" => repos })
            }
        }
        "disk" => {
            let blocked = st.metadata.indices.values().filter(|i| i.settings.get("index.blocks.read_only_allow_delete") == Some("true")).count();
            simple(
                "green",
                "The cluster has enough available disk space.",
                obj! {
                    "indices_with_readonly_block" => blocked, "nodes_with_enough_disk_space" => 1, "nodes_with_unknown_disk_status" => 0,
                    "nodes_over_high_watermark" => 0, "nodes_over_flood_stage_watermark" => 0,
                },
            )
        }
        "shards_capacity" => {
            let setting = |k: &str, d: i64| st.metadata.cluster_setting(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(d);
            let per_node = setting("cluster.max_shards_per_node", 1000);
            let frozen = setting("cluster.max_shards_per_node.frozen", 3000);
            simple(
                "green",
                "The cluster has enough room to add new shards.",
                obj! { "data" => obj! { "max_shards_in_cluster" => per_node }, "frozen" => obj! { "max_shards_in_cluster" => frozen } },
            )
        }
        "file_settings" => simple("green", "No file-based setting changes have occurred", obj! {}),
        "cluster_state_encryption" => {
            // Reported with its details even when not verbose.
            obj! {
                "status" => "green", "symptom" => "Cluster state encryption is not configured.",
                "details" => obj! { "state" => "disabled", "active_password_id" => "(unset)", "encryption_required" => true },
            }
        }
        "data_stream_lifecycle" => simple(
            "green",
            "Data streams are executing their lifecycles without issues",
            obj! { "stagnating_backing_indices_count" => 0, "total_backing_indices_in_error" => 0 },
        ),
        "slm" => simple("green", "No Snapshot Lifecycle Management policies configured", obj! { "slm_status" => "RUNNING", "policies" => 0 }),
        "shards_availability" => shards_availability(ctx, st, verbose, size),
        "ilm" => simple("green", "Index Lifecycle Management is running", obj! { "policies" => 0, "stagnating_indices" => 0, "ilm_status" => "RUNNING" }),
        _ => Value::Null,
    }
}

/// `GET _health_report[/{feature}]`.
pub fn health_report(ctx: &Ctx) -> EsResult<Reply> {
    let verbose = ctx.bool_param("verbose", true)?;
    let size = ctx.int_param("size", 1000)?;
    if size < 0 {
        return Err(EsError::action_request_validation(&["The size parameter must be a positive integer".into()]));
    }
    let st = ctx.state();
    let feature = ctx.path_param("feature");
    if let Some(f) = feature
        && !INDICATORS.contains(&f)
    {
        return Err(EsError::resource_not_found(format!("Did not find indicator {f}")));
    }
    let mut indicators = Map::new();
    let mut worst = 0;
    for name in INDICATORS.iter().filter(|n| feature.is_none_or(|f| f == **n)) {
        let v = indicator(ctx, &st, name, verbose, size as usize);
        let rank = match v.get("status").and_then(Value::as_str) {
            Some("red") => 3,
            Some("yellow") => 2,
            Some("unknown") => 1,
            _ => 0,
        };
        worst = worst.max(rank);
        indicators.insert(name.to_string(), v);
    }
    let mut m = Map::new();
    // A single indicator's report has no overall status.
    if feature.is_none() {
        m.insert("status".into(), ["green", "unknown", "yellow", "red"][worst].into());
    }
    m.insert("cluster_name".into(), ctx.cluster.name.clone().into());
    m.insert("indicators".into(), Value::Obj(indicators));
    Reply::ok(Value::Obj(m))
}

fn stat(v: f64) -> Value {
    obj! { "total" => v, "min" => v, "max" => v, "average" => v, "std_dev" => 0.0 }
}

/// `GET _internal/desired_balance`: every started primary is where it should be; replicas
/// can't be placed (`ignored`) on a single node.
pub fn desired_balance(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let node = &ctx.cluster.node;
    let version = st.version as i64;
    let (mut assigned, mut unassigned) = (0usize, 0usize);
    let mut routing = Map::new();
    let mut shard_sizes = Map::new();
    let mut data_set_sizes = Map::new();
    let mut paths = Map::new();
    let mut reserved_shards = Vec::new();
    let names: Vec<&String> = st.metadata.indices.keys().collect();
    for i in crate::json::java_hash_order(&names, 16) {
        let idx = &st.metadata.indices[names[i]];
        let red = ctx.cluster.unassigned_reason(&st, idx).is_some();
        let replicas = idx.number_of_replicas() as usize;
        let tier = Value::Arr(
            idx.settings
                .get("index.routing.allocation.include._tier_preference")
                .unwrap_or("data_content")
                .split(',')
                .map(|t| Value::from(t.trim().to_string()))
                .collect(),
        );
        let mut shards = Map::new();
        for s in 0..idx.number_of_shards() {
            let copy = |primary: bool, started: bool| {
                obj! {
                    "index" => idx.name.clone(), "shard_id" => s, "state" => if started { "STARTED" } else { "UNASSIGNED" },
                    "primary" => primary, "node" => if started { Value::from(node.id.clone()) } else { Value::Null },
                    "node_is_desired" => started, "relocating_node" => Value::Null, "relocating_node_is_desired" => Value::Null,
                    "forecast_write_load" => Value::Null, "forecast_shard_size_in_bytes" => Value::Null, "tier_preference" => tier.clone(),
                }
            };
            let mut current = vec![copy(true, !red)];
            for _ in 0..replicas {
                current.push(copy(false, false));
            }
            if red {
                unassigned += 1 + replicas;
            } else {
                assigned += 1;
                unassigned += replicas;
                let key = format!("[{}][{s}]", idx.name);
                shard_sizes.insert(format!("{key}[p]_bytes"), 0.into());
                data_set_sizes.insert(format!("{key}_bytes"), 0.into());
                paths.insert(format!("NodeAndShard[nodeId={}, shardId={key}]", node.id), "/data".into());
                reserved_shards.push(Value::from(key));
            }
            let desired_nodes: Vec<Value> = if red { Vec::new() } else { vec![Value::from(node.id.clone())] };
            shards.insert(
                s.to_string(),
                obj! {
                    "current" => Value::Arr(current),
                    "desired" => obj! { "node_ids" => Value::Arr(desired_nodes), "total" => 1 + replicas, "unassigned" => replicas + usize::from(red), "ignored" => replicas + usize::from(red) },
                },
            );
        }
        routing.insert(idx.name.clone(), Value::Obj(shards));
    }
    let shards = assigned as f64;
    let mut tiers = Map::new();
    for role in super::nodes::ROLES.iter().filter(|r| r.starts_with("data")) {
        tiers.insert(
            role.to_string(),
            obj! {
                "shard_count" => stat(shards), "undesired_shard_allocation_count" => stat(0.0), "forecast_write_load" => stat(0.0),
                "forecast_disk_usage" => stat(0.0), "actual_disk_usage" => stat(0.0),
            },
        );
    }
    let mut nodes = Map::new();
    nodes.insert(
        node.name.clone(),
        obj! {
            "node_id" => node.id.clone(), "roles" => Value::from(super::nodes::roles(ctx.cluster.dialect).to_vec()), "shard_count" => assigned,
            "undesired_shard_allocation_count" => 0, "forecast_write_load" => 0.0, "forecast_disk_usage_bytes" => 0,
            "actual_disk_usage_bytes" => 0, "node_weight" => 0.0,
        },
    );
    let m = super::nodes::NodeMetrics::sample(ctx.cluster);
    let used = (m.disk_total - m.disk_avail).max(0);
    let pct = |part: i64| if m.disk_total > 0 { (part as f64 * 1000.0 / m.disk_total as f64).round() / 10.0 } else { 0.0 };
    let disk = obj! {
        "path" => "/data", "total_bytes" => m.disk_total, "used_bytes" => used, "free_bytes" => m.disk_avail,
        "free_disk_percent" => pct(m.disk_avail), "used_disk_percent" => pct(used),
    };
    let reserved = if reserved_shards.is_empty() {
        Vec::new()
    } else {
        vec![obj! { "node_id" => node.id.clone(), "path" => "/data", "total" => 0, "shards" => Value::Arr(reserved_shards) }]
    };
    Reply::ok(obj! {
        "stats" => obj! {
            "computation_converged_index" => version, "computation_active" => false, "computation_submitted" => version,
            "computation_executed" => version, "computation_converged" => version, "computation_iterations" => version,
            "computed_shard_movements" => 0, "computation_time_in_millis" => 0, "reconciliation_time_in_millis" => 0,
            "unassigned_shards" => unassigned, "total_allocations" => assigned, "undesired_allocations" => 0, "undesired_allocations_ratio" => 0.0,
        },
        "cluster_balance_stats" => obj! {
            "shard_count" => assigned, "undesired_shard_allocation_count" => 0,
            "tiers" => Value::Obj(crate::json::java_hash_map_expected(tiers)),
            "nodes" => Value::Obj(nodes),
        },
        "routing_table" => Value::Obj(routing),
        "cluster_info" => obj! {
            "nodes" => obj! { node.id.clone() => obj! { "node_name" => node.name.clone(), "least_available" => disk.clone(), "most_available" => disk } },
            "shard_write_loads" => obj! {},
            "shard_sizes" => Value::Obj(shard_sizes),
            "shard_data_set_sizes" => Value::Obj(data_set_sizes),
            "shard_paths" => Value::Obj(paths),
            "reserved_sizes" => Value::Arr(reserved),
        },
    })
}

/// `DELETE _internal/desired_balance`: restarts the balance computation; an empty
/// `text/plain` answer.
pub fn reset_desired_balance(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Ok(Reply::Raw(200, "text/plain; charset=UTF-8".into(), bytes::Bytes::new()))
}

/// `POST _internal/prevalidate_node_removal`: removing the node is safe unless it holds a copy
/// of a red shard, which a node of roll never does (red shards are unassigned).
pub fn prevalidate_node_removal(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let lists: Vec<(&str, Vec<String>)> = ["names", "ids", "external_ids"].iter().map(|k| (*k, ctx.list_param(k).unwrap_or_default())).collect();
    let given: Vec<&(&str, Vec<String>)> = lists.iter().filter(|(_, l)| !l.is_empty()).collect();
    if given.is_empty() {
        return Err(EsError::action_request_validation(&["request must contain one of the parameters 'names', 'ids', or 'external_ids'".into()]));
    }
    if given.len() > 1 {
        return Err(EsError::action_request_validation(&["request must contain only one of the parameters 'names', 'ids', or 'external_ids'".into()]));
    }
    let (kind, values) = given[0];
    let node = &ctx.cluster.node;
    let own = if *kind == "ids" { &node.id } else { &node.name };
    let missing: Vec<&str> = values.iter().filter(|v| *v != own).map(String::as_str).collect();
    if !missing.is_empty() {
        let what = match *kind {
            "names" => "node names",
            "ids" => "node IDs",
            _ => "node external IDs",
        };
        // Reported from a `HashSet`.
        let order = crate::json::java_hash_order(&missing, 16);
        let listed: Vec<&str> = order.into_iter().map(|i| missing[i]).collect();
        return Err(EsError::resource_not_found(format!("could not resolve {what} [{}]", listed.join(", "))));
    }
    let st = ctx.state();
    let red = st.metadata.indices.values().any(|i| ctx.cluster.unassigned_reason(&st, i).is_some());
    let (message, reason) = if red { ("", "no_red_shards_on_node") } else { ("cluster status is not RED", "no_problems") };
    Reply::ok(obj! {
        "is_safe" => true,
        "message" => message,
        "nodes" => vec![obj! {
            "name" => node.name.clone(), "id" => node.id.clone(), "external_id" => node.name.clone(),
            "result" => obj! { "is_safe" => true, "reason" => reason, "message" => "" },
        }],
    })
}
