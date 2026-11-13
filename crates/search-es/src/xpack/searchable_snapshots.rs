//! Searchable snapshots: `POST _snapshot/<repo>/<snapshot>/_mount`, `GET [<index>/]_searchable_snapshots/stats`,
//! `GET _searchable_snapshots/cache/stats` and `POST [<index>/]_searchable_snapshots/cache/clear`.
//!
//! Elasticsearch mounts a snapshotted index by restoring its metadata with extra settings
//! (`index.store.type: snapshot`, the `index.store.snapshot.*` coordinates, a write block, the
//! searchable-snapshot allocator and tier preference) and then reads the shard files lazily
//! from the repository. roll mounts the same way, through the snapshot restore API (so the
//! index gets exactly a restored index's metadata and errors) plus those settings; the data
//! comes back however the restore brings it back. What is emulated: roll has no Lucene files
//! read through a blob cache, so the per-file statistics arrays are empty, the node's shared
//! cache reports zeros, and a `shared_cache` (frozen tier) mount succeeds like a `full_copy`
//! one (Elasticsearch fails its shards on a node without a shared cache configured).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};

/// The mount body fields (`MountSearchableSnapshotRequest.PARSER`).
const FIELDS: [&str; 4] = ["index", "renamed_index", "index_settings", "ignore_index_settings"];

fn none_mounted() -> EsError {
    EsError::new(404, "resource_not_found_exception", "No searchable snapshots indices found")
}

pub async fn mount(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let repo = ctx.path_param("repository").unwrap_or_default().to_string();
    let snap = ctx.path_param("snapshot").unwrap_or_default().to_string();
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    if let Some(k) = body.keys().find(|k| !FIELDS.contains(&k.as_str())) {
        return Err(crate::xpack::unknown_field(ctx, "mount_searchable_snapshot", k));
    }
    let Some(index) = body.get("index").map(Value::to_es_string) else {
        return Err(EsError::illegal_argument("Required [index]"));
    };
    let partial = match ctx.query("storage").unwrap_or("full_copy") {
        s if s.eq_ignore_ascii_case("full_copy") => false,
        s if s.eq_ignore_ascii_case("shared_cache") => true,
        other => {
            return Err(EsError::x_content_parse_bare("Failed to build [mount_searchable_snapshot] after last required field arrived").caused_by(
                EsError::illegal_argument(format!(
                    "No enum constant org.elasticsearch.xpack.core.searchablesnapshots.MountSearchableSnapshotRequest.Storage.{}",
                    other.to_ascii_uppercase()
                )),
            ));
        }
    };
    let renamed = body.get("renamed_index").map(Value::to_es_string).unwrap_or_else(|| index.clone());

    // The snapshot's coordinates, with the errors Elasticsearch's mount gives before restoring.
    let (repo_uuid, snap_uuid, index_uuid) = {
        let st = ctx.state();
        let Some(r) = st.metadata.repositories.get(&repo) else {
            return Err(EsError::new(404, "repository_missing_exception", format!("[{repo}] missing")));
        };
        let data = st.metadata.repository_data.get(&r.location_key()).cloned().unwrap_or_default();
        let Some(s) = data.find(&snap) else {
            return Err(EsError::new(500, "exception", format!("snapshot [{snap}] not found in repository [{repo}]")));
        };
        let Some(meta) = s.index_meta.get(&index) else {
            return Err(EsError::index_not_found_bare(&format!("index [{index}] not found in repository [{repo}]")));
        };
        let uuid = meta.get("uuid").map(Value::to_es_string).unwrap_or_default();
        (data.uuid.clone(), s.uuid.clone(), uuid)
    };

    let mut settings = match body.get("index_settings") {
        Some(Value::Obj(m)) => m.clone(),
        _ => Map::new(),
    };
    let mut set = |k: &str, v: &str| {
        settings.insert(k.to_string(), Value::from(v));
    };
    set("index.store.type", "snapshot");
    set("index.store.snapshot.repository_name", &repo);
    set("index.store.snapshot.repository_uuid", &repo_uuid);
    set("index.store.snapshot.snapshot_name", &snap);
    set("index.store.snapshot.snapshot_uuid", &snap_uuid);
    set("index.store.snapshot.index_name", &index);
    set("index.store.snapshot.index_uuid", &index_uuid);
    set("index.recovery.type", "snapshot_prewarm");
    set("index.blocks.write", "true");
    set("index.allocation.existing_shards_allocator", "searchable_snapshot_allocator");
    set("index.auto_expand_replicas", "false");
    set("index.shard.check_on_startup", "false");
    if partial {
        set("index.store.snapshot.partial", "true");
        set("index.routing.allocation.include._tier_preference", "data_frozen");
        set("index.routing.allocation.disk.watermark.ignore", "true");
        set("index.shard_limit.group", "frozen");
        set("index.number_of_replicas", "0");
    } else {
        set("index.routing.allocation.include._tier_preference", "data_cold,data_warm,data_hot");
    }
    let mut restore = obj! {
        "indices" => Value::from(vec![index.clone()]),
        "include_aliases" => false,
        "include_global_state" => false,
        "index_settings" => Value::Obj(settings),
        // The whole name, `$` escaped for the regex replacement.
        "rename_pattern" => "^.*$",
        "rename_replacement" => renamed.replace('$', "$$"),
    };
    if let Some(ignore) = body.get("ignore_index_settings")
        && let Value::Obj(m) = &mut restore
    {
        m.insert("ignore_index_settings".into(), ignore.clone());
    }
    let uri = format!("/_snapshot/{}/{}/_restore?wait_for_completion={wait}", enc(&repo), enc(&snap));
    let r = internal::call(ctx.cluster, "POST", &uri, Some(&restore)).await?;
    Reply::ok(r)
}

/// The mounted indices among `expressions` (all of them when empty), with their shard counts.
fn mounted(ctx: &Ctx) -> EsResult<Vec<(String, std::sync::Arc<crate::meta::IndexMeta>)>> {
    let st = ctx.state();
    let names = match ctx.list_param("index") {
        Some(exprs) if !exprs.is_empty() => {
            let opts = ctx.indices_options(crate::meta::resolve::IndicesOptions::STRICT_EXPAND_OPEN)?;
            crate::meta::resolve::resolve(&st, &exprs, &opts, ctx.now_ms)?
        }
        _ => st.metadata.indices.keys().cloned().collect(),
    };
    let out: Vec<_> = names
        .into_iter()
        .filter_map(|n| st.metadata.indices.get(&n).map(|i| (n, i.clone())))
        .filter(|(_, i)| i.settings.get("index.store.type") == Some("snapshot") && !i.is_closed())
        .collect();
    if out.is_empty() {
        return Err(none_mounted());
    }
    Ok(out)
}

pub fn stats(ctx: &Ctx) -> EsResult<Reply> {
    let level = ctx.query("level").unwrap_or("indices");
    if !matches!(level, "cluster" | "indices" | "shards") {
        return Err(EsError::illegal_argument(format!("level parameter must be one of [cluster] or [indices] or [shards] but was [{level}]")));
    }
    let indices = mounted(ctx)?;
    let total: i64 = indices.iter().map(|(_, i)| i.number_of_shards() as i64).sum();
    let mut out = obj! {
        "_shards" => obj! { "total" => total, "successful" => total, "failed" => 0 },
        "total" => Value::Arr(Vec::new()),
    };
    if level != "cluster" {
        let mut per = Map::new();
        for (name, idx) in &indices {
            let mut entry = obj! { "total" => Value::Arr(Vec::new()) };
            if level == "shards"
                && let Value::Obj(m) = &mut entry
            {
                let mut shards = Map::new();
                for s in 0..idx.number_of_shards() {
                    shards.insert(
                        s.to_string(),
                        Value::Arr(vec![obj! {
                            "snapshot_uuid" => idx.settings.get("index.store.snapshot.snapshot_uuid").unwrap_or_default(),
                            "index_uuid" => idx.settings.get("index.store.snapshot.index_uuid").unwrap_or_default(),
                            "shard" => obj! {
                                "id" => format!("[{name}][{s}]"),
                                "state" => "STARTED",
                                "primary" => true,
                                "node" => ctx.cluster.node.id.clone(),
                            },
                            "files" => Value::Arr(Vec::new()),
                        }]),
                    );
                }
                m.insert("shards".into(), Value::Obj(shards));
            }
            per.insert(name.clone(), entry);
        }
        if let Value::Obj(m) = &mut out {
            m.insert("indices".into(), Value::Obj(per));
        }
    }
    Reply::ok(out)
}

pub fn cache_stats(ctx: &Ctx) -> EsResult<Reply> {
    let mut nodes = Map::new();
    nodes.insert(
        ctx.cluster.node.id.clone(),
        obj! {
            "shared_cache" => obj! {
                "reads" => 0, "bytes_read_in_bytes" => 0, "writes" => 0, "bytes_written_in_bytes" => 0,
                "evictions" => 0, "num_regions" => 0, "size_in_bytes" => 0, "region_size_in_bytes" => 16_777_216,
            }
        },
    );
    Reply::ok(obj! { "nodes" => Value::Obj(nodes) })
}

pub fn clear_cache(ctx: &Ctx) -> EsResult<Reply> {
    let indices = mounted(ctx)?;
    let total: i64 = indices.iter().map(|(_, i)| i.number_of_shards() as i64).sum();
    Reply::ok(obj! { "_shards" => obj! { "total" => total, "successful" => total, "failed" => 0 } })
}
