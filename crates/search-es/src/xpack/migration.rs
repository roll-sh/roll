//! Upgrade assistance: `_migration/system_features`, `_migration/deprecations`, data stream
//! upgrades (`_migration/reindex`) and `_create_from`.
//!
//! roll writes every index in the current format, so no system feature, index or data stream
//! ever needs upgrading: feature upgrade status reports `NO_MIGRATION_NEEDED` for each of
//! Elasticsearch's system features (with the system indices that exist), the deprecation
//! report is empty, and a data stream reindex completes at once with nothing to reindex while
//! keeping its status (a persistent task on Elasticsearch) until cancelled. `_create_from` is
//! real: it creates the destination with the source's settings and mappings (minus the
//! per-index private settings and, by default, the write blocks) plus the overrides, through
//! the create index API.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal, store, unknown_field};

/// Elasticsearch 9.5.5's system features and the index patterns each owns.
const FEATURES: &[(&str, &[&str])] = &[
    ("async_search", &[".async-search*"]),
    ("enrich", &[".enrich-*"]),
    ("ent_search", &[".query-rules-*", ".search-app-*", ".elastic-connectors*"]),
    ("fleet", &[".fleet-*"]),
    ("geoip", &[".geoip_databases*"]),
    ("inference_plugin", &[".inference*", ".secrets-inference*"]),
    ("kibana", &[".kibana*"]),
    ("logstash_management", &[".logstash*"]),
    ("machine_learning", &[".ml-config*", ".ml-inference-*", ".ml-meta*"]),
    ("searchable_snapshots", &[".snapshot-blob-cache*"]),
    ("security", &[".security-*"]),
    ("synonyms", &[".synonyms*"]),
    ("tasks", &[".tasks*"]),
    ("transform", &[".transform-internal-*"]),
    ("watcher", &[".watches*", ".triggered_watches*"]),
];

pub fn feature_upgrade_status(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let st = ctx.state();
    let v = crate::version::INDEX_VERSION_STRING;
    let features: Vec<Value> = FEATURES
        .iter()
        .map(|(name, patterns)| {
            let indices: Vec<Value> = st
                .metadata
                .indices
                .keys()
                .filter(|i| !i.starts_with(".ds-") && patterns.iter().any(|p| crate::xcontent::filter::glob_match(p, i)))
                .map(|i| obj! { "index" => i.clone(), "version" => v })
                .collect();
            obj! { "feature_name" => *name, "minimum_index_version" => v, "migration_status" => "NO_MIGRATION_NEEDED", "indices" => Value::Arr(indices) }
        })
        .collect();
    Reply::ok(obj! { "features" => Value::Arr(features), "migration_status" => "NO_MIGRATION_NEEDED" })
}

pub fn post_feature_upgrade(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    Reply::ok(obj! { "accepted" => false, "reason" => "No system indices require migration" })
}

pub fn deprecations(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    if let Some(idx) = ctx.path_param("index") {
        // Unknown concrete indices fail like any index resolution.
        let st = ctx.state();
        for name in idx.split(',').filter(|n| !n.contains('*') && !n.is_empty() && *n != "_all") {
            if !st.metadata.indices.contains_key(name) && !st.metadata.data_streams.contains_key(name) && !st.metadata.indices.values().any(|i| i.aliases.contains_key(name)) {
                return Err(EsError::index_not_found(name));
            }
        }
    }
    Reply::ok(obj! {
        "cluster_settings" => Value::Arr(vec![]),
        "node_settings" => Value::Arr(vec![]),
        "ml_settings" => Value::Arr(vec![]),
        "ilm_policies" => Value::object(),
        "data_streams" => Value::object(),
        "index_settings" => Value::object(),
        "templates" => Value::object(),
    })
}

const REINDEX: &str = "migrate.reindex";

pub async fn migrate_reindex(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if k != "mode" && k != "source" {
            return Err(unknown_field(ctx, "migration_reindex", k));
        }
    }
    let Some(source) = body.get("source") else {
        let mut missing = vec![];
        if !body.contains_key("mode") {
            missing.push("mode");
        }
        missing.push("source");
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    };
    let Some(mode) = body.get("mode") else { return Err(EsError::illegal_argument("Required [mode]")) };
    let (l, c) = crate::xpack::key_loc(ctx, "source");
    let index = match source.get("index") {
        Some(Value::Str(s)) => s.clone(),
        _ => {
            return Err(EsError::x_content_parse(l, c, "[migration_reindex] failed to parse field [source]").caused_by(EsError::illegal_argument("Required [index]")));
        }
    };
    let mode = mode.to_es_string();
    if mode != "upgrade" {
        return Err(EsError::x_content_parse(l, c, "[migration_reindex] failed to parse field [source]").caused_by(
            EsError::x_content_parse_bare("Failed to build [migration_reindex] after last required field arrived").caused_by(EsError::illegal_argument(format!(
                "No enum constant org.elasticsearch.xpack.migrate.action.ReindexDataStreamAction.Mode.{}",
                mode.to_uppercase()
            ))),
        ));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(ds) = st.metadata.data_streams.get(&index) else {
                return Err(EsError::resource_not_found(format!("Data stream named [{index}] does not exist")));
            };
            if st.metadata.xpack.get(REINDEX, &index).is_some() {
                return Err(EsError::resource_already_exists(format!("task with id {{reindex-data-stream-{index}}} already exist")));
            }
            let total = ds.indices.len();
            store::put(st, REINDEX, &index, obj! { "start_time_millis" => now, "total_indices_in_data_stream" => total }, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub fn migrate_reindex_status(ctx: &Ctx) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default();
    let st = ctx.state();
    let Some(t) = st.metadata.xpack.get(REINDEX, index) else {
        return Err(EsError::resource_not_found(format!("No migration reindex status found for [{index}]")));
    };
    Reply::ok(obj! {
        "start_time_millis" => t.get("start_time_millis").cloned().unwrap_or(Value::from(0)),
        "complete" => true,
        "total_indices_in_data_stream" => t.get("total_indices_in_data_stream").cloned().unwrap_or(Value::from(0)),
        "total_indices_requiring_upgrade" => 0,
        "successes" => 0,
        "in_progress" => Value::Arr(vec![]),
        "pending" => 0,
        "errors" => Value::Arr(vec![]),
    })
}

pub async fn cancel_migrate_reindex(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let index = ctx.path_param("index").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| match store::remove(st, REINDEX, &index) {
            Some(_) => Ok(()),
            None => Err(EsError::resource_not_found(format!("the project [default] task with id reindex-data-stream-{index} doesn't exist"))),
        })
        .await?;
    Reply::acknowledged()
}

/// Settings `_create_from` never copies: they identify the source index.
fn private_setting(k: &str) -> bool {
    matches!(k, "index.uuid" | "index.creation_date" | "index.provided_name" | "index.history.uuid" | "index.routing.allocation.initial_recovery._id")
        || k.starts_with("index.version.")
        || k.starts_with("index.resize.")
        || k.starts_with("index.shrink.")
        || k.starts_with("index.store.snapshot.")
        || k.starts_with("index.verified_before_close")
        || k.starts_with("index.downsample.")
}

pub async fn create_from(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let source = ctx.path_param("source").unwrap_or_default().to_string();
    let dest = ctx.path_param("dest").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !matches!(k.as_str(), "settings_override" | "mappings_override" | "remove_index_blocks") {
            return Err(unknown_field(ctx, "create_index_from_source_request", k));
        }
    }
    let remove_blocks = body.get("remove_index_blocks").and_then(Value::as_bool).unwrap_or(true);
    if !ctx.state().metadata.indices.contains_key(&source) {
        return Err(EsError::index_not_found_bare(&source));
    }
    let got = internal::call(ctx.cluster, "GET", &format!("/{}?flat_settings=true", enc(&source)), None).await?;
    let src = got.get(&source).cloned().unwrap_or(Value::Null);
    let mut settings = Map::new();
    if let Some(Value::Obj(flat)) = src.get("settings") {
        for (k, v) in flat {
            if private_setting(k) || (remove_blocks && k.starts_with("index.blocks.")) {
                continue;
            }
            settings.insert(k.clone(), v.clone());
        }
    }
    if let Some(over @ Value::Obj(_)) = body.get("settings_override") {
        let over = crate::meta::settings::Settings::from_json(over)?;
        if let Value::Obj(flat) = over.to_flat_json() {
            settings.extend(flat);
        }
    }
    let mut req = Map::new();
    req.insert("settings".into(), Value::Obj(settings));
    if let Some(m @ Value::Obj(mm)) = src.get("mappings")
        && !mm.is_empty()
    {
        req.insert("mappings".into(), m.clone());
    }
    let resp = internal::call(ctx.cluster, "PUT", &format!("/{}", enc(&dest)), Some(&Value::Obj(req))).await?;
    if let Some(over @ Value::Obj(m)) = body.get("mappings_override")
        && !m.is_empty()
    {
        internal::call(ctx.cluster, "PUT", &format!("/{}/_mapping", enc(&dest)), Some(over)).await?;
    }
    Reply::ok(resp)
}
