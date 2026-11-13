//! Transforms: pivot and latest transforms over roll's indices (`_transform/...`), plus
//! `_cat/transforms`.
//!
//! **What is real:** the configurations (validation, defaults, update rules and errors of
//! Elasticsearch 9.5.5), previews, and running a transform: a checkpoint reads the source with
//! a paged `composite` aggregation through the core search API ([`internal`]) — group_by sources
//! as its sources, the pivot's aggregations below it (a `top_hits` of the newest document for
//! `latest`) — and bulk-indexes one document per bucket into the destination, which it first
//! creates with the mappings Elasticsearch deduces (`TransformAggregations`: `max`/`min` keep
//! the source field's type, `avg`/`sum`/`percentiles`/`stats` are `double`, counts `long`, a
//! `date_histogram` group `date`, ...). Document ids are a stable hash of the bucket key, so
//! re-running a checkpoint overwrites rather than duplicates. Stats count what the runs did.
//!
//! **How it runs:** a checkpoint runs inside the request that triggers it (`_start`,
//! `_schedule_now`). A batch transform then stops, as on Elasticsearch; a continuous one
//! (`sync`) stays `started` and runs another checkpoint on every `_schedule_now` or restart —
//! roll has no per-cluster background scheduler, so `frequency` isn't acted on.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::ml::{public, roll_state, valid_id, with_roll_state};
use crate::xpack::{enc, internal, store, unknown_field};

pub const TRANSFORMS: &str = "transform";
const CONFIG_VERSION: &str = "10.0.0";
const PAGE_SIZE: i64 = 500;
const PREVIEW_SIZE: i64 = 100;

const PUT_FIELDS: &[&str] = &["source", "dest", "pivot", "latest", "description", "frequency", "sync", "settings", "_meta", "retention_policy"];
const UPDATE_FIELDS: &[&str] = &["source", "dest", "description", "frequency", "sync", "settings", "_meta", "retention_policy"];

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("Transform with id [{id}] could not be found"))
}

fn conflict(reason: String) -> EsError {
    EsError::new(409, "status_exception", reason)
}

fn get_one(st: &crate::meta::ClusterState, id: &str) -> EsResult<Value> {
    st.metadata.xpack.get(TRANSFORMS, id).cloned().ok_or_else(|| not_found(id))
}

fn state_of(t: &Value) -> String {
    roll_state(t).get("state").and_then(Value::as_str).unwrap_or("stopped").to_string()
}

/// The source section with its defaults (`index` as a list, `query` match_all).
fn normalize_source(src: &Value) -> EsResult<Value> {
    let Value::Obj(m) = src else { return Err(EsError::illegal_argument("Required [index]")) };
    let mut out = Map::new();
    let index = match m.get("index") {
        Some(Value::Str(s)) => Value::Arr(s.split(',').map(|p| Value::from(p.trim())).collect()),
        Some(Value::Arr(a)) => Value::Arr(a.clone()),
        _ => return Err(EsError::illegal_argument("Required [index]")),
    };
    out.insert("index".into(), index);
    out.insert("query".into(), m.get("query").cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }));
    for (k, v) in m {
        if k != "index" && k != "query" {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Obj(out))
}

/// The pivot with `aggs` spelled `aggregations`, as Elasticsearch prints it.
fn normalize_pivot(p: &Value) -> Value {
    let Value::Obj(m) = p else { return p.clone() };
    let mut out = Map::new();
    for (k, v) in m {
        out.insert(if k == "aggs" { "aggregations".to_string() } else { k.clone() }, v.clone());
    }
    Value::Obj(out)
}

fn source_indices(t: &Value) -> Vec<String> {
    t.pointer(&["source", "index"]).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default()
}

/// Source indices that don't exist (wildcards must match something too).
fn missing_indices(st: &crate::meta::ClusterState, indices: &[String]) -> Vec<String> {
    indices
        .iter()
        .filter(|i| {
            let exists = |n: &str| st.metadata.indices.contains_key(n) || st.metadata.data_streams.contains_key(n) || st.metadata.indices.values().any(|x| x.aliases.contains_key(n));
            if i.contains('*') {
                !st.metadata.indices.keys().any(|n| crate::xcontent::filter::glob_match(i, n)) && !st.metadata.data_streams.keys().any(|n| crate::xcontent::filter::glob_match(i, n))
            } else {
                !exists(i)
            }
        })
        .cloned()
        .collect()
}

fn build_config(ctx: &Ctx, id: &str, body: &Map, now: i64) -> EsResult<Value> {
    for k in body.keys() {
        if !PUT_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, "data_frame_transform_config", k));
        }
    }
    let missing: Vec<&str> = ["source", "dest"].into_iter().filter(|k| !body.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    if body.contains_key("pivot") == body.contains_key("latest") {
        return Err(EsError::x_content_parse_bare("Failed to build [data_frame_transform_config] after last required field arrived")
            .caused_by(EsError::illegal_argument("Transform configuration must specify exactly 1 function")));
    }
    let dest = body.get("dest").cloned().unwrap_or(Value::Null);
    if dest.get("index").and_then(Value::as_str).is_none() {
        return Err(EsError::illegal_argument("Required [index]"));
    }
    let roles: Vec<Value> = crate::xpack::security::principal(ctx).roles.iter().map(|r| Value::from(r.as_str())).collect();
    let mut out = Map::new();
    out.insert("id".into(), id.into());
    out.insert("authorization".into(), obj! { "roles" => Value::Arr(roles) });
    out.insert("version".into(), CONFIG_VERSION.into());
    out.insert("create_time".into(), now.into());
    out.insert("source".into(), normalize_source(body.get("source").unwrap_or(&Value::Null))?);
    out.insert("dest".into(), dest);
    for k in ["frequency", "sync"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(p) = body.get("pivot") {
        validate_pivot(p)?;
        out.insert("pivot".into(), normalize_pivot(p));
    }
    if let Some(l) = body.get("latest") {
        validate_latest(l)?;
        out.insert("latest".into(), l.clone());
    }
    for k in ["description", "_meta", "retention_policy"] {
        if let Some(v) = body.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    out.insert("settings".into(), body.get("settings").cloned().unwrap_or_else(Value::object));
    Ok(Value::Obj(out))
}

fn validate_pivot(p: &Value) -> EsResult<()> {
    let groups = p.get("group_by").and_then(Value::as_object);
    if groups.is_none_or(|g| g.is_empty()) {
        return Err(EsError::illegal_argument("Required [group_by]"));
    }
    for (name, g) in groups.into_iter().flatten() {
        let kind = g.as_object().and_then(|m| m.keys().next()).map(String::as_str).unwrap_or("");
        if !matches!(kind, "terms" | "histogram" | "date_histogram" | "geotile_grid") {
            return Err(EsError::parse(format!("Unknown group_by type [{kind}] for [{name}]")));
        }
    }
    Ok(())
}

fn validate_latest(l: &Value) -> EsResult<()> {
    let keys = l.get("unique_key").and_then(Value::as_array);
    if keys.is_none_or(|k| k.is_empty()) {
        return Err(EsError::action_request_validation(&["latest.unique_key must be non-empty".into()]));
    }
    if l.get("sort").and_then(Value::as_str).is_none_or(str::is_empty) {
        return Err(EsError::action_request_validation(&["latest.sort must be non-empty".into()]));
    }
    Ok(())
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(EsError::action_request_validation(&[format!(
            "Invalid id; '{id}' can contain lowercase alphanumeric (a-z and 0-9), hyphens or underscores; must start and end with alphanumeric"
        )]));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let defer = ctx.bool_param("defer_validation", false)?;
    let cfg = build_config(ctx, &id, &body, ctx.now_ms)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(TRANSFORMS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("Transform with id [{id}] already exists")));
            }
            if !defer {
                let missing = missing_indices(st, &source_indices(&cfg));
                if !missing.is_empty() {
                    let msgs: Vec<String> = missing.iter().map(|m| format!("no such index [{m}]")).collect();
                    return Err(EsError::new(400, "validation_exception", format!("Validation Failed: {}", numbered(&msgs))));
                }
            }
            store::put(st, TRANSFORMS, &id, cfg, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

fn numbered(msgs: &[String]) -> String {
    msgs.iter().enumerate().map(|(i, m)| format!("{}: {m};", i + 1)).collect()
}

pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !UPDATE_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, "data_frame_transform_config_update", k));
        }
    }
    let source = match body.get("source") {
        Some(s) => Some(normalize_source(s)?),
        None => None,
    };
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            let cur = get_one(st, &id)?;
            let Value::Obj(mut m) = cur else { return Err(not_found(&id)) };
            if let Some(s) = source {
                m.insert("source".into(), s);
            }
            for k in ["dest", "frequency", "sync", "description", "_meta", "retention_policy"] {
                if let Some(v) = body.get(k) {
                    m.insert(k.into(), v.clone());
                }
            }
            if let Some(Value::Obj(s)) = body.get("settings") {
                let mut cur = m.get("settings").and_then(Value::as_object).cloned().unwrap_or_default();
                for (k, v) in s {
                    if v.is_null() {
                        cur.shift_remove(k);
                    } else {
                        cur.insert(k.clone(), v.clone());
                    }
                }
                m.insert("settings".into(), Value::Obj(cur));
            }
            let v = Value::Obj(reorder(m));
            store::put(st, TRANSFORMS, &id, v.clone(), now)?;
            Ok(v)
        })
        .await?;
    Reply::ok(public(&out))
}

/// Elasticsearch's field order of a transform configuration.
fn reorder(mut m: Map) -> Map {
    const ORDER: &[&str] =
        &["id", "authorization", "version", "create_time", "source", "dest", "frequency", "sync", "pivot", "latest", "description", "settings", "_meta", "retention_policy", "_roll"];
    let mut out = Map::new();
    for k in ORDER {
        if let Some(v) = m.shift_remove(*k) {
            out.insert(k.to_string(), v);
        }
    }
    out.extend(m);
    out
}

fn expand(ctx: &Ctx, st: &crate::meta::ClusterState) -> EsResult<Vec<(String, Value)>> {
    let allow = ctx.bool_param("allow_no_match", true)?;
    crate::xpack::ml::expand(st, TRANSFORMS, ctx.path_param("transform_id"), allow, not_found)
}

fn paged(ctx: &Ctx, items: Vec<Value>) -> EsResult<(usize, Vec<Value>)> {
    let from = ctx.int_param("from", 0)?.max(0) as usize;
    let size = ctx.int_param("size", 100)?.max(0) as usize;
    let count = items.len();
    Ok((count, items.into_iter().skip(from).take(size).collect()))
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let exclude = ctx.bool_param("exclude_generated", false)?;
    let items: Vec<Value> = expand(ctx, &st)?
        .into_iter()
        .map(|(_, t)| {
            let mut v = public(&t);
            if exclude && let Value::Obj(m) = &mut v {
                for k in ["authorization", "version", "create_time"] {
                    m.shift_remove(k);
                }
            }
            v
        })
        .collect();
    let (count, page) = paged(ctx, items)?;
    Reply::ok(obj! { "count" => count, "transforms" => Value::Arr(page) })
}

const STAT_KEYS: &[&str] = &[
    "pages_processed",
    "documents_processed",
    "documents_indexed",
    "documents_deleted",
    "trigger_count",
    "index_time_in_ms",
    "index_total",
    "index_failures",
    "search_time_in_ms",
    "search_total",
    "search_failures",
    "processing_time_in_ms",
    "processing_total",
    "delete_time_in_ms",
];

fn stats_value(r: &Map) -> Value {
    let s = r.get("stats").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut m = Map::new();
    for k in STAT_KEYS {
        m.insert(k.to_string(), s.get(*k).cloned().unwrap_or(Value::from(0)));
    }
    for k in ["exponential_avg_checkpoint_duration_ms", "exponential_avg_documents_indexed", "exponential_avg_documents_processed"] {
        m.insert(k.to_string(), Value::from(s.get(k).and_then(Value::as_f64).unwrap_or(0.0)));
    }
    Value::Obj(m)
}

pub async fn stats(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let st = ctx.state();
    let mut items = Vec::new();
    for (id, t) in expand(ctx, &st)? {
        let r = roll_state(&t);
        let state = state_of(&t);
        let mut m = Map::new();
        m.insert("id".into(), id.clone().into());
        m.insert("state".into(), state.clone().into());
        if state == "started" || state == "indexing" {
            m.insert("node".into(), crate::xpack::ml::node_value(ctx, true));
        }
        m.insert("stats".into(), stats_value(&r));
        let checkpoint = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0);
        let mut cp = Map::new();
        let mut last = Map::new();
        last.insert("checkpoint".into(), checkpoint.into());
        if let Some(ts) = r.get("checkpoint_time") {
            last.insert("timestamp_millis".into(), ts.clone());
        }
        if let Some(ub) = r.get("time_upper_bound") {
            last.insert("time_upper_bound_millis".into(), ub.clone());
        }
        cp.insert("last".into(), Value::Obj(last));
        if checkpoint == 0 {
            cp.insert("operations_behind".into(), source_doc_count(ctx, &source_indices(&t)).await.into());
        }
        if let Some(ts) = r.get("changes_last_detected_at") {
            cp.insert("changes_last_detected_at".into(), ts.clone());
            if state == "started" {
                cp.insert("last_search_time".into(), ts.clone());
            }
        }
        m.insert("checkpointing".into(), Value::Obj(cp));
        m.insert("health".into(), obj! { "status" => "green" });
        items.push(Value::Obj(m));
    }
    let (count, page) = paged(ctx, items)?;
    Reply::ok(obj! { "count" => count, "transforms" => Value::Arr(page) })
}

/// The documents of the source indices (what a first checkpoint is behind by).
async fn source_doc_count(ctx: &Ctx<'_>, indices: &[String]) -> i64 {
    let uri = format!("/{}/_count?ignore_unavailable=true&allow_no_indices=true", enc(&indices.join(",")));
    internal::call(ctx.cluster, "GET", &uri, None).await.ok().and_then(|r| r.get("count").and_then(Value::as_i64)).unwrap_or(0)
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let force = ctx.bool_param("force", false)?;
    let delete_dest = ctx.bool_param("delete_dest_index", false)?;
    let st = ctx.state();
    let t = get_one(&st, &id)?;
    if !force && state_of(&t) != "stopped" {
        return Err(conflict(format!("Cannot delete transform [{id}] as the task is running. Stop the task first")));
    }
    drop(st);
    if delete_dest && let Some(dest) = t.pointer(&["dest", "index"]).and_then(Value::as_str) {
        let _ = internal::call(ctx.cluster, "DELETE", &format!("/{}", enc(dest)), None).await;
    }
    ctx.cluster
        .update(move |st| {
            store::remove(st, TRANSFORMS, &id);
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn reset(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let force = ctx.bool_param("force", false)?;
    let t = get_one(&ctx.state(), &id)?;
    if !force && state_of(&t) != "stopped" {
        return Err(conflict(format!("Cannot reset transform [{id}] as the task is running. Stop the task first")));
    }
    // The destination index goes if the transform created it.
    if roll_state(&t).get("created_dest").and_then(Value::as_bool) == Some(true)
        && let Some(dest) = t.pointer(&["dest", "index"]).and_then(Value::as_str)
    {
        let _ = internal::call(ctx.cluster, "DELETE", &format!("/{}", enc(dest)), None).await;
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let t = get_one(st, &id)?;
            let mut v = t.clone();
            if let Value::Obj(m) = &mut v {
                m.shift_remove("_roll");
            }
            store::put(st, TRANSFORMS, &id, v, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn start(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let t = get_one(&ctx.state(), &id)?;
    if state_of(&t) != "stopped" {
        return Err(conflict(format!("Cannot start transform [{id}] as it is already started.")));
    }
    let missing = missing_indices(&ctx.state(), &source_indices(&t));
    if !missing.is_empty() {
        let msgs: Vec<String> = missing.iter().map(|m| format!("no such index [{m}]")).collect();
        return Err(EsError::new(400, "validation_exception", format!("Validation Failed: {}", numbered(&msgs))));
    }
    run_checkpoint(ctx, &id).await?;
    Reply::acknowledged()
}

pub async fn schedule_now(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let t = get_one(&ctx.state(), &id)?;
    if state_of(&t) == "started" {
        run_checkpoint(ctx, &id).await?;
    }
    Reply::acknowledged()
}

pub async fn stop(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    ctx.bool_param("wait_for_completion", false)?;
    ctx.bool_param("force", false)?;
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let ids: Vec<String> = crate::xpack::ml::expand(&st, TRANSFORMS, ctx.path_param("transform_id"), allow, not_found)?.into_iter().map(|(k, _)| k).collect();
    drop(st);
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            for id in ids {
                if let Some(t) = st.metadata.xpack.get(TRANSFORMS, &id).cloned()
                    && state_of(&t) != "stopped"
                {
                    let v = with_roll_state(t, |r| {
                        r.insert("state".into(), "stopped".into());
                    });
                    store::put(st, TRANSFORMS, &id, v, now)?;
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn node_stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let running = st.metadata.xpack.iter(TRANSFORMS).filter(|(_, t)| state_of(t) == "started").count();
    let mut out = Map::new();
    out.insert(ctx.cluster.node.id.clone(), obj! { "scheduler" => obj! { "registered_transform_count" => running, "peek_transform" => Value::Null } });
    out.insert("total".into(), obj! { "scheduler" => obj! { "registered_transform_count" => running } });
    Reply::ok(Value::Obj(out))
}

pub fn upgrade(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    ctx.bool_param("dry_run", false)?;
    let n = ctx.state().metadata.xpack.len(TRANSFORMS);
    Reply::ok(obj! { "updated" => 0, "no_action" => n, "needs_update" => 0 })
}

pub fn set_upgrade_mode(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    ctx.bool_param("enabled", false)?;
    Reply::acknowledged()
}

// --- running -----------------------------------------------------------------------------------

/// The field types of the source indices (`field_caps`), by field name.
async fn field_types(ctx: &Ctx<'_>, indices: &[String], fields: &[String]) -> Map {
    if fields.is_empty() {
        return Map::new();
    }
    let uri = format!("/{}/_field_caps?fields={}&ignore_unavailable=true&allow_no_indices=true", enc(&indices.join(",")), enc(&fields.join(",")));
    let Ok(caps) = internal::call(ctx.cluster, "GET", &uri, None).await else { return Map::new() };
    let mut out = Map::new();
    if let Some(Value::Obj(f)) = caps.get("fields") {
        for (name, types) in f {
            if let Some(t) = types.as_object().and_then(|t| t.keys().next()) {
                out.insert(name.clone(), Value::from(t.as_str()));
            }
        }
    }
    out
}

/// The single field an aggregation reads (`field` of its body).
fn agg_field(agg: &Value) -> Option<(String, String)> {
    let (kind, body) = agg.as_object()?.iter().find(|(k, _)| k.as_str() != "aggs" && k.as_str() != "aggregations" && k.as_str() != "meta")?;
    let field = match kind.as_str() {
        "top_metrics" => body.pointer(&["metrics", "field"]).or_else(|| body.get("metrics").and_then(Value::as_array).and_then(|a| a.first()).and_then(|m| m.get("field")))?,
        _ => body.get("field")?,
    };
    Some((kind.clone(), field.to_es_string()))
}

/// Elasticsearch's target mapping of an aggregation's output (`TransformAggregations`).
fn agg_target(kind: &str, source_type: Option<&str>) -> Option<&'static str> {
    Some(match kind {
        "avg" | "median_absolute_deviation" | "sum" | "weighted_avg" | "percentiles" | "stats" | "boxplot" | "extended_stats" => "double",
        "cardinality" | "value_count" | "range" | "filter" | "missing" => "long",
        "geo_bounds" | "geo_line" => "geo_shape",
        "geo_centroid" => "geo_point",
        "terms" | "rare_terms" => "flattened",
        "max" | "min" | "top_metrics" => match source_type? {
            "scaled_float" => "float",
            "aggregate_metric_double" => "double",
            t => leak(t),
        },
        _ => return None,
    })
}

/// Field types are a small closed set; interned to `'static`.
fn leak(t: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "long", "integer", "short", "byte", "double", "float", "half_float", "unsigned_long", "date", "date_nanos", "keyword", "constant_keyword", "wildcard", "ip", "boolean",
        "version", "text", "match_only_text",
    ];
    KNOWN.iter().find(|k| **k == t).copied().unwrap_or("double")
}

/// Mappings of the destination index Elasticsearch deduces for a pivot.
async fn deduced_mappings(ctx: &Ctx<'_>, t: &Value) -> Map {
    let mut props = Map::new();
    let Some(pivot) = t.get("pivot") else { return props };
    let indices = source_indices(t);
    let mut fields = Vec::new();
    if let Some(Value::Obj(groups)) = pivot.get("group_by") {
        for g in groups.values() {
            if let Some(Value::Obj(inner)) = g.as_object().and_then(|m| m.values().next())
                && let Some(f) = inner.get("field")
            {
                fields.push(f.to_es_string());
            }
        }
    }
    let aggs = pivot.get("aggregations").or_else(|| pivot.get("aggs")).and_then(Value::as_object).cloned().unwrap_or_default();
    for a in aggs.values() {
        if let Some((_, f)) = agg_field(a) {
            fields.push(f);
        }
    }
    let types = field_types(ctx, &indices, &fields).await;
    if let Some(Value::Obj(groups)) = pivot.get("group_by") {
        for (name, g) in groups {
            let Some((kind, inner)) = g.as_object().and_then(|m| m.iter().next()) else { continue };
            let ty = match kind.as_str() {
                "date_histogram" => Some("date".to_string()),
                "histogram" => Some("double".to_string()),
                "geotile_grid" => Some("geo_shape".to_string()),
                _ => inner.get("field").and_then(|f| types.get(&f.to_es_string())).map(Value::to_es_string),
            };
            if let Some(ty) = ty {
                props.insert(name.clone(), obj! { "type" => ty });
            }
        }
    }
    for (name, a) in &aggs {
        let (kind, ty) = match agg_field(a) {
            Some((kind, f)) => {
                let st = types.get(&f).map(Value::to_es_string);
                (kind.clone(), agg_target(&kind, st.as_deref()))
            }
            None => {
                let kind = a.as_object().and_then(|m| m.keys().find(|k| k.as_str() != "aggs" && k.as_str() != "aggregations")).cloned().unwrap_or_default();
                let ty = agg_target(&kind, None);
                (kind, ty)
            }
        };
        // Object outputs map as an object plus a double per leaf, named like the output keys
        // (`TransformAggregations.getAggregationOutputTypes`).
        let leaves: Vec<String> = match kind.as_str() {
            "percentiles" => percents_of(a).into_iter().map(percent_key).collect(),
            "stats" => STATS_LEAVES.iter().map(|l| l.to_string()).collect(),
            "extended_stats" => STATS_LEAVES.iter().chain(EXTENDED_STATS_LEAVES).map(|l| l.to_string()).collect(),
            "boxplot" => BOXPLOT_LEAVES.iter().map(|l| l.to_string()).collect(),
            _ => Vec::new(),
        };
        if !leaves.is_empty() {
            props.insert(name.clone(), obj! { "type" => "object" });
            if kind == "extended_stats" {
                props.insert(format!("{name}.std_deviation_bounds"), obj! { "type" => "object" });
            }
            for l in leaves {
                props.insert(format!("{name}.{l}"), obj! { "type" => "double" });
            }
            continue;
        }
        if let Some(ty) = ty {
            props.insert(name.clone(), obj! { "type" => ty });
        }
    }
    // `SchemaUtil.deduceMappings` collects the field types in a `HashMap`.
    crate::ingest::doc::java_hash_order(&mut props, false);
    props
}

fn generated_dest(id: &str, now: i64, props: Map) -> Value {
    obj! {
        "mappings" => obj! {
            "_meta" => obj! {
                "_transform" => obj! { "transform" => id, "version" => obj! { "created" => CONFIG_VERSION }, "creation_date_in_millis" => now },
                "created_by" => "transform",
            },
            "properties" => Value::Obj(props),
        },
        "settings" => obj! { "index" => obj! { "number_of_shards" => "1", "auto_expand_replicas" => "0-1" } },
        "aliases" => obj! {},
    }
}

/// The composite aggregation a checkpoint pages through.
fn composite_request(t: &Value, size: i64, after: Option<&Value>) -> Value {
    let mut sources = Vec::new();
    let mut sub = Map::new();
    if let Some(pivot) = t.get("pivot") {
        if let Some(Value::Obj(groups)) = pivot.get("group_by") {
            for (name, g) in groups {
                let mut g = g.clone();
                if let Some(Value::Obj(inner)) = g.as_object_mut().and_then(|m| m.values_mut().next()).map(|v| v as &mut Value) {
                    inner.entry("missing_bucket".to_string()).or_insert(Value::from(true));
                }
                let mut m = Map::new();
                m.insert(name.clone(), g);
                sources.push(Value::Obj(m));
            }
        }
        if let Some(Value::Obj(a)) = pivot.get("aggregations").or_else(|| pivot.get("aggs")) {
            sub = a.clone();
        }
    } else if let Some(latest) = t.get("latest") {
        for k in latest.get("unique_key").and_then(Value::as_array).into_iter().flatten() {
            let f = k.to_es_string();
            let mut m = Map::new();
            m.insert(f.clone(), obj! { "terms" => obj! { "field" => f, "missing_bucket" => true } });
            sources.push(Value::Obj(m));
        }
        let sort = latest.get("sort").map(Value::to_es_string).unwrap_or_default();
        let mut so = Map::new();
        so.insert(sort, obj! { "order" => "desc" });
        sub.insert("_transform_latest".into(), obj! { "top_hits" => obj! { "size" => 1, "sort" => crate::arr![Value::Obj(so)] } });
    }
    let mut comp = Map::new();
    comp.insert("size".into(), size.into());
    comp.insert("sources".into(), Value::Arr(sources));
    if let Some(a) = after {
        comp.insert("after".into(), a.clone());
    }
    let mut agg = Map::new();
    agg.insert("composite".into(), Value::Obj(comp));
    if !sub.is_empty() {
        agg.insert("aggregations".into(), Value::Obj(sub));
    }
    obj! {
        "size" => 0,
        "query" => t.pointer(&["source", "query"]).cloned().unwrap_or_else(|| obj! { "match_all" => obj! {} }),
        "runtime_mappings" => t.pointer(&["source", "runtime_mappings"]).cloned().unwrap_or_else(Value::object),
        "aggregations" => obj! { "_transform" => Value::Obj(agg) },
    }
}

/// Sets `a.b.c` in a document as nested objects, as transforms write dotted names.
fn set_path(doc: &mut Map, name: &str, v: Value) {
    match name.split_once('.') {
        Some((head, rest)) => {
            let e = doc.entry(head.to_string()).or_insert_with(Value::object);
            if !matches!(e, Value::Obj(_)) {
                *e = Value::object();
            }
            if let Value::Obj(m) = e {
                set_path(m, rest, v);
            }
        }
        None => {
            doc.insert(name.to_string(), v);
        }
    }
}

/// `String.hashCode()` spread as `HashMap.hash` does.
fn java_string_hash(s: &str) -> u32 {
    let h = s.encode_utf16().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32));
    h ^ (h >> 16)
}

const STATS_LEAVES: [&str; 5] = ["count", "min", "max", "avg", "sum"];
const EXTENDED_STATS_LEAVES: &[&str] = &[
    "sum_of_squares",
    "variance",
    "variance_population",
    "variance_sampling",
    "std_deviation",
    "std_deviation_population",
    "std_deviation_sampling",
    "std_deviation_bounds.upper",
    "std_deviation_bounds.lower",
    "std_deviation_bounds.upper_population",
    "std_deviation_bounds.lower_population",
    "std_deviation_bounds.upper_sampling",
    "std_deviation_bounds.lower_sampling",
];
const BOXPLOT_LEAVES: [&str; 7] = ["min", "max", "q1", "q2", "q3", "lower", "upper"];

/// A `percentiles` aggregation's percents (Elasticsearch's defaults when not given).
fn percents_of(a: &Value) -> Vec<f64> {
    match a.pointer(&["percentiles", "percents"]) {
        Some(Value::Arr(ps)) => ps.iter().filter_map(Value::as_f64).collect(),
        _ => vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0],
    }
}

/// `OutputFieldNameConverter.fromDouble`: whole percents without a fraction (`50`), others
/// with `_` for the decimal point (`99_5`), so they are no dotted paths.
fn percent_key(p: f64) -> String {
    if p == (p as i64) as f64 { (p as i64).to_string() } else { crate::json::write::java_double(p).replace('.', "_") }
}

/// The value a pivot aggregation writes for its response `r`.
fn agg_output(kind: &str, r: &Value) -> Value {
    match kind {
        "percentiles" => {
            // Ascending percents; a NaN (no data) is written as null.
            let mut out: Vec<(f64, Value)> = match r.get("values") {
                Some(Value::Obj(m)) => m.iter().filter_map(|(k, v)| Some((k.parse::<f64>().ok()?, v.clone()))).collect(),
                Some(Value::Arr(a)) => a.iter().filter_map(|e| Some((e.get("key")?.as_f64()?, e.get("value")?.clone()))).collect(),
                _ => return Value::Null,
            };
            out.sort_by(|a, b| a.0.total_cmp(&b.0));
            Value::Obj(out.into_iter().map(|(p, v)| (percent_key(p), if v.as_f64().is_some_and(f64::is_finite) { v } else { Value::Null })).collect())
        }
        "stats" | "extended_stats" | "boxplot" | "geo_bounds" | "geo_line" => match r {
            Value::Obj(m) => Value::Obj(
                m.iter()
                    .filter(|(k, _)| !k.ends_with("_as_string") && k.as_str() != "meta")
                    // `stats` writes its count as a double, like its other leaves.
                    .map(|(k, v)| (k.clone(), if kind == "stats" && k == "count" { v.as_f64().map_or(v.clone(), Value::from) } else { v.clone() }))
                    .collect(),
            ),
            other => other.clone(),
        },
        "geo_centroid" => r.get("location").cloned().unwrap_or(Value::Null),
        "filter" | "missing" => r.get("doc_count").cloned().unwrap_or(Value::from(0)),
        "range" => match r.get("buckets") {
            Some(Value::Arr(a)) => Value::Obj(a.iter().filter_map(|b| Some((b.get("key")?.to_es_string(), b.get("doc_count")?.clone()))).collect()),
            _ => Value::Null,
        },
        "terms" | "rare_terms" => match r.get("buckets") {
            Some(Value::Arr(a)) => Value::Obj(a.iter().filter_map(|b| Some((b.get("key")?.to_es_string(), b.get("doc_count")?.clone()))).collect()),
            _ => Value::Null,
        },
        "top_metrics" => r.pointer(&["top"]).and_then(Value::as_array).and_then(|a| a.first()).and_then(|t| t.get("metrics")).cloned().unwrap_or(Value::Null),
        "scripted_metric" => r.get("value").cloned().unwrap_or(Value::Null),
        _ => match r.get("value_as_string") {
            // Dates (min/max over date fields) are written formatted.
            Some(s) if r.get("value").is_some_and(|v| !v.is_null()) => s.clone(),
            _ => r.get("value").cloned().unwrap_or(Value::Null),
        },
    }
}

/// One output document per composite bucket. `types` are the deduced destination mappings:
/// numbers written to an integral field drop their fraction
/// (`dropFloatingPointComponentIfTypeRequiresIt`).
fn bucket_docs(t: &Value, buckets: &[Value], types: &Map) -> Vec<(String, Value)> {
    let aggs = t.get("pivot").and_then(|p| p.get("aggregations").or_else(|| p.get("aggs"))).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(buckets.len());
    for b in buckets {
        let key = b.get("key").cloned().unwrap_or(Value::Null);
        let id = doc_id(&key);
        if t.get("latest").is_some() {
            let src = b.pointer(&["_transform_latest", "hits", "hits"]).and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.get("_source")).cloned();
            if let Some(src) = src {
                out.push((id, src));
            }
            continue;
        }
        let mut doc = Map::new();
        if let Value::Obj(k) = &key {
            for (name, v) in k {
                set_path(&mut doc, name, date_key(t, name, v, b));
            }
        }
        for (name, a) in &aggs {
            let kind = a.as_object().and_then(|m| m.keys().find(|k| !matches!(k.as_str(), "aggs" | "aggregations" | "meta"))).cloned().unwrap_or_default();
            let r = b.get(name).cloned().unwrap_or(Value::Null);
            let mut v = agg_output(&kind, &r);
            let integral = types.get(name).and_then(|m| m.get("type")).and_then(Value::as_str).is_some_and(|t| matches!(t, "long" | "integer" | "short" | "byte" | "unsigned_long"));
            if integral && let Some(d) = v.as_f64().filter(|d| d.is_finite() && matches!(v, Value::Num(_))) {
                v = Value::from(d.floor() as i64);
            }
            set_path(&mut doc, name, v);
        }
        // Elasticsearch builds the documents as `HashMap`s (`AggregationResultUtils`), but a
        // percentiles object is a `LinkedHashMap` in ascending percent order.
        crate::ingest::doc::java_hash_order(&mut doc, true);
        for (name, a) in &aggs {
            if a.get("percentiles").is_some() {
                let r = b.get(name).cloned().unwrap_or(Value::Null);
                set_path(&mut doc, name, agg_output("percentiles", &r));
            }
            // `std_deviation_bounds` is a `HashMap` sized for its six entries (8 buckets).
            if a.get("extended_stats").is_some()
                && let Some(Value::Obj(bounds)) = doc.get_mut(name.as_str()).and_then(Value::as_object_mut).and_then(|m| m.get_mut("std_deviation_bounds"))
            {
                let mut e: Vec<(usize, String, Value)> = std::mem::take(bounds).into_iter().enumerate().map(|(i, (k, v))| (i, k, v)).collect();
                e.sort_by_key(|(i, k, _)| (java_string_hash(k) & 7, *i));
                bounds.extend(e.into_iter().map(|(_, k, v)| (k, v)));
            }
        }
        out.push((id, Value::Obj(doc)));
    }
    out
}

/// `date_histogram` group keys are written as dates (`yyyy-MM-ddTHH:mm:ss.SSSZ`).
fn date_key(t: &Value, name: &str, v: &Value, _bucket: &Value) -> Value {
    let is_date = t.pointer(&["pivot", "group_by", name, "date_histogram"]).is_some();
    match (is_date, v.as_i64()) {
        (true, Some(ms)) => Value::from(crate::time::java_instant_string(ms)),
        _ => v.clone(),
    }
}

/// A stable id for a bucket key: URL-safe base64 of a 24-byte digest, the length of
/// Elasticsearch's transform document ids.
fn doc_id(key: &Value) -> String {
    use base64::Engine;
    let d = crate::xpack::security::digest(&[b"transform", key.to_json().as_bytes()]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&d[..24])
}

/// Previews a transform: the first page of output documents and the destination it would create.
async fn preview_of(ctx: &Ctx<'_>, t: &Value, id: &str) -> EsResult<Value> {
    let indices = source_indices(t);
    let req = composite_request(t, PREVIEW_SIZE, None);
    let resp = internal::call(ctx.cluster, "POST", &format!("/{}/_search", enc(&indices.join(","))), Some(&req)).await?;
    let buckets = resp.pointer(&["aggregations", "_transform", "buckets"]).and_then(Value::as_array).cloned().unwrap_or_default();
    let props = deduced_mappings(ctx, t).await;
    let docs: Vec<Value> = bucket_docs(t, &buckets, &props).into_iter().map(|(_, d)| d).collect();
    Ok(obj! { "preview" => Value::Arr(docs), "generated_dest_index" => generated_dest(id, ctx.now_ms, props) })
}

pub async fn preview(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let (t, id) = match ctx.path_param("transform_id") {
        Some(id) => (public(&get_one(&ctx.state(), id)?), id.to_string()),
        None => {
            let body = ctx.body_map()?.unwrap_or_default();
            let mut body = body;
            if !body.contains_key("dest") {
                body.insert("dest".into(), obj! { "index" => "unused-transform-preview-index" });
            }
            let cfg = build_config(ctx, "transform-preview", &body, ctx.now_ms)?;
            let missing = missing_indices(&ctx.state(), &source_indices(&cfg));
            if !missing.is_empty() {
                let msgs: Vec<String> = missing.iter().map(|m| format!("no such index [{m}]")).collect();
                return Err(EsError::new(400, "validation_exception", format!("Validation Failed: {}", numbered(&msgs))));
            }
            (cfg, "transform-preview".to_string())
        }
    };
    Reply::ok(preview_of(ctx, &t, &id).await?)
}

/// Runs one checkpoint of a transform and records its stats and state.
async fn run_checkpoint(ctx: &Ctx<'_>, id: &str) -> EsResult<()> {
    let t = get_one(&ctx.state(), id)?;
    let started = std::time::Instant::now();
    let dest = t.pointer(&["dest", "index"]).and_then(Value::as_str).unwrap_or_default().to_string();
    let pipeline = t.pointer(&["dest", "pipeline"]).and_then(Value::as_str).map(str::to_string);
    let mut created_dest = roll_state(&t).get("created_dest").and_then(Value::as_bool).unwrap_or(false);
    let types = deduced_mappings(ctx, &t).await;
    if !ctx.state().metadata.indices.contains_key(&dest) && !ctx.state().metadata.data_streams.contains_key(&dest) && !ctx.state().metadata.indices.values().any(|i| i.aliases.contains_key(&dest)) {
        let mut body = generated_dest(id, ctx.now_ms, types.clone());
        if let (Value::Obj(m), Some(Value::Arr(aliases))) = (&mut body, t.pointer(&["dest", "aliases"])) {
            let mut am = Map::new();
            for a in aliases {
                if let Some(name) = a.get("alias").and_then(Value::as_str) {
                    am.insert(name.to_string(), obj! {});
                }
            }
            m.insert("aliases".into(), Value::Obj(am));
        }
        internal::call(ctx.cluster, "PUT", &format!("/{}", enc(&dest)), Some(&body)).await?;
        created_dest = true;
    }
    let indices = source_indices(&t);
    let search_uri = format!("/{}/_search", enc(&indices.join(",")));
    let page_size = t.pointer(&["settings", "max_page_search_size"]).and_then(Value::as_i64).unwrap_or(PAGE_SIZE);
    let (mut pages, mut processed, mut indexed, mut searches, mut index_ops) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut search_ms = 0i64;
    let mut index_ms = 0i64;
    let mut after: Option<Value> = None;
    loop {
        let req = composite_request(&t, page_size, after.as_ref());
        let s0 = std::time::Instant::now();
        let resp = internal::call(ctx.cluster, "POST", &search_uri, Some(&req)).await?;
        search_ms += s0.elapsed().as_millis() as i64;
        searches += 1;
        pages += 1;
        let buckets = resp.pointer(&["aggregations", "_transform", "buckets"]).and_then(Value::as_array).cloned().unwrap_or_default();
        if buckets.is_empty() {
            break;
        }
        processed += buckets.iter().filter_map(|b| b.get("doc_count").and_then(Value::as_i64)).sum::<i64>();
        let docs = bucket_docs(&t, &buckets, &types);
        let mut nd = String::new();
        for (doc_id, d) in &docs {
            nd.push_str(&obj! { "index" => obj! { "_index" => dest.clone(), "_id" => doc_id.clone() } }.to_json());
            nd.push('\n');
            nd.push_str(&d.to_json());
            nd.push('\n');
        }
        let uri = match &pipeline {
            Some(p) => format!("/_bulk?pipeline={}", enc(p)),
            None => "/_bulk".to_string(),
        };
        let i0 = std::time::Instant::now();
        let r = internal::call_ndjson(ctx.cluster, "POST", &uri, nd).await?;
        index_ms += i0.elapsed().as_millis() as i64;
        index_ops += 1;
        indexed += r.get("items").and_then(Value::as_array).map_or(0, |a| a.iter().filter(|i| i.pointer(&["index", "status"]).and_then(Value::as_i64).is_some_and(|s| s < 300)).count() as i64);
        after = resp.pointer(&["aggregations", "_transform", "after_key"]).cloned();
        if after.is_none() || (buckets.len() as i64) < page_size {
            // One more (empty) page confirms the end, as Elasticsearch's indexer does.
            searches += 1;
            pages += 1;
            break;
        }
    }
    let _ = internal::call(ctx.cluster, "POST", &format!("/{}/_refresh", enc(&dest)), None).await;
    let elapsed = started.elapsed().as_millis() as i64;
    let continuous = t.get("sync").is_some();
    let now = ctx.now_ms;
    let id = id.to_string();
    let delay_ms = t.pointer(&["sync", "time", "delay"]).map(|d| crate::units::TimeValue::parse(&d.to_es_string(), "delay").map(|v| v.as_millis()).unwrap_or(60_000));
    ctx.cluster
        .update(move |st| {
            let Some(cur) = st.metadata.xpack.get(TRANSFORMS, &id).cloned() else { return Ok(()) };
            let v = with_roll_state(cur, |r| {
                let mut s = r.get("stats").and_then(Value::as_object).cloned().unwrap_or_default();
                let mut add = |k: &str, n: i64| {
                    let cur = s.get(k).and_then(Value::as_i64).unwrap_or(0);
                    s.insert(k.into(), (cur + n).into());
                };
                add("pages_processed", pages);
                add("documents_processed", processed);
                add("documents_indexed", indexed);
                add("trigger_count", 1);
                add("index_time_in_ms", index_ms);
                add("index_total", index_ops);
                add("search_time_in_ms", search_ms);
                add("search_total", searches);
                add("processing_total", pages);
                s.insert("exponential_avg_checkpoint_duration_ms".into(), Value::from(elapsed as f64));
                s.insert("exponential_avg_documents_indexed".into(), Value::from(indexed as f64));
                s.insert("exponential_avg_documents_processed".into(), Value::from(processed as f64));
                r.insert("stats".into(), Value::Obj(s));
                let cp = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0) + 1;
                r.insert("checkpoint".into(), cp.into());
                r.insert("checkpoint_time".into(), now.into());
                r.insert("changes_last_detected_at".into(), now.into());
                if let Some(d) = delay_ms {
                    // Continuous checkpoints only see data older than the sync delay, rounded
                    // down to the date_histogram interval Elasticsearch aligns it to.
                    r.insert("time_upper_bound".into(), ((now - d) / 3_600_000 * 3_600_000).into());
                }
                r.insert("created_dest".into(), created_dest.into());
                r.insert("state".into(), if continuous { "started" } else { "stopped" }.into());
            });
            store::put(st, TRANSFORMS, &id, v, now).map(|_| ())
        })
        .await
}

// --- _cat/transforms ---------------------------------------------------------------------------

macro_rules! cols {
    ($($n:literal),*) => { &[$(crate::api::cat::Column { name: $n, aliases: &[], desc: None, default: true, right: false, sibling: None, cond: crate::api::cat::Cond::Always }),*] };
}

static CAT_COLUMNS: &[crate::api::cat::Column] = cols!("id", "state", "checkpoint", "documents_processed", "checkpoint_progress", "last_search_time", "changes_last_detection_time");

pub fn cat(ctx: &Ctx) -> EsResult<Reply> {
    use crate::api::cat::{Cell, Table};
    let st = ctx.state();
    let mut t = Table::new(CAT_COLUMNS);
    let ids = crate::xpack::ml::expand(&st, TRANSFORMS, ctx.path_param("transform_id"), ctx.bool_param("allow_no_match", true)?, not_found)?;
    let fmt = |ms: i64| crate::time::DateFormatter::new("EEE MMM dd HH:mm:ss 'GMT' yyyy").map(|f| f.format_millis(ms)).unwrap_or_default();
    for (id, tr) in ids {
        let r = roll_state(&tr);
        let cp = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0);
        let docs = r.get("stats").and_then(|s| s.get("documents_processed")).and_then(Value::as_i64).unwrap_or(0);
        let state = state_of(&tr);
        let started = state == "started";
        let last = r.get("changes_last_detected_at").and_then(Value::as_i64);
        t.rows.push(vec![
            Cell::Str(id),
            Cell::Str(state),
            Cell::Str(cp.to_string()),
            Cell::Str(docs.to_string()),
            if started && cp > 0 { Cell::Str("100.00".into()) } else { Cell::Null },
            match (started, last) {
                (true, Some(ms)) => Cell::Str(fmt(ms)),
                _ => Cell::Null,
            },
            match (started, last) {
                (true, Some(ms)) => Cell::Str(fmt(ms)),
                _ => Cell::Null,
            },
        ]);
    }
    Ok(Reply::Table(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_types() {
        assert_eq!(agg_target("max", Some("float")), Some("float"));
        assert_eq!(agg_target("max", Some("scaled_float")), Some("float"));
        assert_eq!(agg_target("avg", Some("integer")), Some("double"));
        assert_eq!(agg_target("value_count", None), Some("long"));
        assert_eq!(agg_target("scripted_metric", None), None);
    }

    #[test]
    fn dotted_output() {
        let mut d = Map::new();
        set_path(&mut d, "a.b", Value::from(1));
        set_path(&mut d, "a.c", Value::from(2));
        assert_eq!(Value::Obj(d).to_json(), r#"{"a":{"b":1,"c":2}}"#);
    }

    #[test]
    fn stable_ids() {
        let k = obj! { "pop" => 13.0 };
        assert_eq!(doc_id(&k), doc_id(&k.clone()));
        assert_eq!(doc_id(&k).len(), 32);
        assert_ne!(doc_id(&k), doc_id(&obj! { "pop" => 14.0 }));
    }

    #[test]
    fn outputs() {
        assert_eq!(agg_output("max", &obj! { "value" => 1.5 }), Value::from(1.5));
        assert_eq!(agg_output("max", &obj! { "value" => 1.0e12, "value_as_string" => "2001-09-09T01:46:40.000Z" }), Value::from("2001-09-09T01:46:40.000Z"));
        assert_eq!(agg_output("filter", &obj! { "doc_count" => 3 }), Value::from(3));
        assert_eq!(percent_key(50.0), "50");
        assert_eq!(percent_key(99.5), "99_5");
        assert_eq!(percent_key(0.001), "0_001");
        let p = agg_output("percentiles", &obj! { "values" => obj! { "99.5" => 98.51, "1.0" => 1.98, "50.0" => "NaN" } });
        assert_eq!(p, obj! { "1" => 1.98, "50" => Value::Null, "99_5" => 98.51 });
        assert_eq!(p.as_object().unwrap().keys().collect::<Vec<_>>(), ["1", "50", "99_5"]);
        assert_eq!(agg_output("stats", &obj! { "count" => 50, "min" => 1.0 }), obj! { "count" => 50.0, "min" => 1.0 });
    }
}
