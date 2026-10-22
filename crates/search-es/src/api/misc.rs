//! Small operational APIs whose answers follow from a one-node cluster without snapshots,
//! remote clusters or system features: cache clearing, recoveries, reroute, remote info,
//! feature states, voting exclusions, hot threads, cluster resolution and snapshot repositories.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::meta::resolve::IndicesOptions;
use crate::obj;
use crate::rest::{Ctx, Reply};

/// `POST {index}/_cache/clear`: nothing is cached between requests yet; reports the shards.
pub fn clear_cache(ctx: &Ctx) -> EsResult<Reply> {
    for p in ["fielddata", "query", "request"] {
        ctx.bool_param(p, false)?;
    }
    super::indices::broadcast(ctx)
}

/// `GET {index}/_recovery`: every primary recovered from an empty store (or its existing store
/// once closed), done.
pub fn recovery(ctx: &Ctx) -> EsResult<Reply> {
    let active_only = ctx.bool_param("active_only", false)?;
    let detailed = ctx.bool_param("detailed", false)?;
    let names = super::resolve_indices(ctx, IndicesOptions::STRICT_EXPAND_OPEN)?;
    let st = ctx.state();
    let node = &ctx.cluster.node;
    let mut out = Map::new();
    if active_only {
        return Reply::ok(Value::Obj(out));
    }
    for n in names {
        let idx = &st.metadata.indices[&n];
        let created = idx.creation_date();
        // A restored index recovered its shards (one small file each) from the snapshot.
        let restored = idx.recovery_source.as_ref().filter(|_| !idx.is_closed());
        let (files, bytes, pct) = if restored.is_some() { (1, 225, "100.0%") } else { (0, 0, "0.0%") };
        let shards: Vec<Value> = (0..idx.number_of_shards())
            .map(|s| {
                obj! {
                    "id" => s,
                    "type" => if restored.is_some() { "SNAPSHOT" } else if idx.is_closed() { "EXISTING_STORE" } else { "EMPTY_STORE" },
                    "stage" => "DONE",
                    "primary" => true,
                    "start_time_in_millis" => created,
                    "stop_time_in_millis" => created,
                    "total_time_in_millis" => 0,
                    "source" => restored.map(|r| Value::Obj((**r).clone())).unwrap_or_else(|| obj! {}),
                    "target" => obj! {
                        "id" => node.id.clone(), "host" => node.host.clone(), "transport_address" => node.transport_address.clone(),
                        "ip" => node.ip.clone(), "name" => node.name.clone(),
                    },
                    "index" => obj! {
                        "size" => obj! { "total_in_bytes" => bytes, "reused_in_bytes" => 0, "recovered_in_bytes" => bytes, "recovered_from_snapshot_in_bytes" => 0, "percent" => pct },
                        "files" => if detailed {
                            obj! { "total" => files, "reused" => 0, "recovered" => files, "percent" => pct, "details" => Value::Arr(Vec::new()) }
                        } else {
                            obj! { "total" => files, "reused" => 0, "recovered" => files, "percent" => pct }
                        },
                        "total_time_in_millis" => 0,
                        "source_throttle_time_in_millis" => 0,
                        "target_throttle_time_in_millis" => 0,
                    },
                    "translog" => obj! { "recovered" => 0, "total" => 0, "percent" => "100.0%", "total_on_start" => 0, "total_time_in_millis" => 0 },
                    "verify_index" => obj! { "check_index_time_in_millis" => 0, "total_time_in_millis" => 0 },
                }
            })
            .collect();
        out.insert(n.clone(), obj! { "shards" => Value::Arr(shards) });
    }
    // `RecoveryResponse` keys indices in a `HashMap`.
    let mut v = Value::Obj(crate::json::java_hash_map(out, 16));
    if ctx.human {
        crate::units::humanize(&mut v);
        // Unthrottled recoveries print their throttle time as `-1`.
        if let Value::Obj(indices) = &mut v {
            for shard in indices.values_mut().filter_map(|i| i.as_object_mut()).filter_map(|i| i.get_mut("shards")).filter_map(|s| match s {
                Value::Arr(a) => Some(a),
                _ => None,
            }) {
                for s in shard.iter_mut().filter_map(|s| s.as_object_mut()).filter_map(|s| s.get_mut("index")).filter_map(|i| i.as_object_mut()) {
                    for k in ["source_throttle_time", "target_throttle_time"] {
                        if s.contains_key(k) {
                            s.insert(k.into(), Value::from("-1"));
                        }
                    }
                }
            }
        }
    }
    Reply::ok(v)
}

/// `GET _remote/info`: no remote clusters.
pub fn remote_info(_ctx: &Ctx) -> EsResult<Reply> {
    Reply::ok(obj! {})
}

/// The feature states roll clusters have (none of the system features of a full Elasticsearch
/// distribution apply).
const FEATURES: &[(&str, &str)] = &[("synonyms", "Manages synonyms"), ("tasks", "Manages task results")];

pub fn get_features(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let list: Vec<Value> = FEATURES.iter().map(|(n, d)| obj! { "name" => *n, "description" => *d }).collect();
    Reply::ok(obj! { "features" => Value::Arr(list) })
}

/// `POST _features/_reset`: drops the state of every system feature (synonym sets; task
/// results are not kept).
pub async fn reset_features(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    if !st.metadata.synonyms.is_empty() || st.metadata.indices.values().any(|i| i.system) {
        ctx.cluster
            .update(|st| {
                let system: Vec<String> = st.metadata.indices.values().filter(|i| i.system).map(|i| i.name.clone()).collect();
                super::indices::delete_indices(st, &system)?;
                st.metadata.synonyms = Default::default();
                st.metadata.synonyms_version += 1;
                Ok(())
            })
            .await?;
    }
    let list: Vec<Value> = FEATURES.iter().map(|(n, _)| obj! { "feature_name" => *n, "status" => "SUCCESS" }).collect();
    Reply::ok(obj! { "features" => Value::Arr(list) })
}

/// `POST _cluster/voting_config_exclusions`: records exclusions (unknown nodes as `_absent_`);
/// the only master-eligible node can't be excluded.
pub async fn post_voting_exclusions(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let ids = ctx.list_param("node_ids").unwrap_or_default();
    let names = ctx.list_param("node_names").unwrap_or_default();
    if ids.is_empty() == names.is_empty() {
        if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
            return Err(EsError::illegal_argument(
                "Please set node identifiers correctly. One and only one of [node_name], [node_names] and [node_ids] has to be set",
            ));
        }
        return Err(EsError::illegal_argument("You must set [node_names] or [node_ids] but not both"));
    }
    let node = &ctx.cluster.node;
    if ids.contains(&node.id) || names.contains(&node.name) {
        return Err(EsError::illegal_argument(format!(
            "add voting config exclusions request for nodes {:?} would add [1] exclusions to the existing [0] which would exceed the maximum of [0] eligible masters",
            if ids.is_empty() { &names } else { &ids }
        )));
    }
    let entries: Vec<(String, String)> = if ids.is_empty() {
        names.into_iter().map(|n| ("_absent_".to_string(), n)).collect()
    } else {
        ids.into_iter().map(|i| (i, "_absent_".to_string())).collect()
    };
    ctx.cluster
        .update(move |st| {
            for e in entries {
                if !st.metadata.voting_exclusions.contains(&e) {
                    st.metadata.voting_exclusions.push(e);
                }
            }
            Ok(())
        })
        .await?;
    Ok(Reply::Empty(200))
}

pub async fn delete_voting_exclusions(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    ctx.bool_param("wait_for_removal", true)?;
    ctx.cluster
        .update(|st| {
            st.metadata.voting_exclusions.clear();
            Ok(())
        })
        .await?;
    Ok(Reply::Empty(200))
}

/// `GET _nodes/hot_threads`: the node's header line; roll's threads are shared by every cluster
/// on the node, so none is reported as hot for this cluster.
pub fn hot_threads(ctx: &Ctx) -> EsResult<Reply> {
    let node = &ctx.cluster.node;
    let interval = ctx.query("interval").unwrap_or("500ms");
    let threads = ctx.int_param("threads", 3)?;
    let ignore_idle = ctx.bool_param("ignore_idle_threads", true)?;
    if let Some(t) = ctx.query("type")
        && !matches!(t, "cpu" | "wait" | "block" | "mem")
    {
        return Err(EsError::illegal_argument(format!("type not supported [{t}]")));
    }
    let now = crate::time::DateFormatter::new("strict_date_time").expect("built-in").format_millis(ctx.now_ms);
    let text = format!(
        "::: {{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}{{{}}}\n   Hot threads at {now}, interval={interval}, busiestThreads={threads}, ignoreIdleThreads={ignore_idle}:\n\n",
        node.name,
        node.id,
        node.ephemeral_id,
        node.name,
        node.host,
        node.transport_address,
        super::nodes::role_abbreviations(ctx.cluster.dialect),
        crate::version::ES_VERSION
    );
    Ok(Reply::Raw(200, super::cat::TEXT_PLAIN.to_string(), bytes::Bytes::from(text)))
}

/// `GET _resolve/cluster/{name}`: only the local cluster.
pub fn resolve_cluster(ctx: &Ctx) -> EsResult<Reply> {
    let Some(exprs) = ctx.list_param("name") else {
        return Reply::ok(obj! {});
    };
    crate::meta::resolve::reject_remote(&exprs)?;
    let opts = ctx.indices_options(IndicesOptions::STRICT_EXPAND_OPEN)?;
    let st = ctx.state();
    let mut local = Map::new();
    local.insert("connected".into(), true.into());
    local.insert("skip_unavailable".into(), false.into());
    match crate::meta::resolve::resolve(&st, &exprs, &opts, ctx.now_ms) {
        Ok(names) => {
            // A search naming a closed index would fail, so it doesn't count as a match.
            let named_closed = exprs.iter().any(|e| !e.contains('*') && st.metadata.indices.get(e).is_some_and(|i| i.is_closed()));
            // Closed indices a wildcard expands to can't be searched either; aliases and data
            // streams matching the expressions count whatever state their indices are in.
            let any_open = names.iter().any(|n| st.metadata.indices.get(n).is_none_or(|i| !i.is_closed()));
            let matches = |name: &str| exprs.iter().any(|e| e == "_all" || crate::xcontent::filter::glob_match(e, name));
            let any_alias = st.aliases().keys().any(|a| matches(a))
                || st.metadata.data_streams.keys().any(|d| matches(d))
                || st.metadata.data_stream_aliases.keys().any(|a| matches(a));
            local.insert("matching_indices".into(), ((any_open || any_alias) && !named_closed).into());
            local.insert(
                "version".into(),
                obj! {
                    "number" => crate::version::ES_VERSION,
                    "build_flavor" => "default",
                    "minimum_wire_compatibility_version" => crate::version::MIN_WIRE_COMPAT,
                    "minimum_index_compatibility_version" => crate::version::MIN_INDEX_COMPAT,
                },
            );
        }
        Err(e) => {
            local.insert("error".into(), e.reason().into());
        }
    }
    Reply::ok(obj! { "(local)" => Value::Obj(local) })
}

