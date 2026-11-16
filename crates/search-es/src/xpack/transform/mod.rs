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
//! **How it runs** ([`run`]): in the background, like Elasticsearch. `_start` checks the
//! configuration against the cluster, creates the destination and marks the transform
//! `started` with its first trigger due; the cluster's job scheduler runs it. A batch transform
//! stops after its checkpoint. A continuous one (`sync.time`) is triggered every `frequency`:
//! it looks for source documents whose sync time is past the last checkpoint's bound (`now -
//! delay`) and, when there are some, writes a new checkpoint, which recomputes every bucket up
//! to the new bound. `_start` of a continuous transform answers once its first checkpoint is
//! done; `_schedule_now` makes the next trigger due at once. The schedule and the stats live in
//! the stored transform, so a started transform survives the cluster sleeping: the triggers
//! that came due meanwhile run when it wakes. Failures are retried with a backoff
//! (`num_failure_retries`, forever when `unattended`), then the transform is `failed`.
//!
//! Configuration parsing and validation are in [`config`], the composite search and the
//! documents and mappings it produces in [`output`].

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::ml::{public, roll_state, with_roll_state};
use crate::xpack::{enc, internal, store, unknown_field};

mod config;
mod output;
mod run;

use config::*;
use output::*;
use run::*;
pub use run::{Runs, next_due, run_due};

pub const TRANSFORMS: &str = "transform";

const CONFIG_VERSION: &str = "10.0.0";

const PAGE_SIZE: i64 = 500;

const PREVIEW_SIZE: i64 = 100;

/// `PageParams.MAX_SIZE` of the get APIs.
const MAX_GET_SIZE: i64 = 1_000;

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
    t.pointer(&["_roll", "state"]).and_then(Value::as_str).unwrap_or("stopped").to_string()
}

/// The state the APIs show: a started transform running a trigger is `indexing`.
fn shown_state(cluster: &Cluster, id: &str, t: &Value) -> String {
    let state = state_of(t);
    if state == "started" && cluster.xpack.transforms.is_running(id) { "indexing".to_string() } else { state }
}

fn source_indices(t: &Value) -> Vec<String> {
    t.pointer(&["source", "index"]).and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default()
}

fn dest_index(t: &Value) -> &str {
    t.pointer(&["dest", "index"]).and_then(Value::as_str).unwrap_or_default()
}

fn setting<'a>(t: &'a Value, key: &str) -> Option<&'a Value> {
    t.pointer(&["settings", key]).filter(|v| !v.is_null())
}

fn deduces_mappings(t: &Value) -> bool {
    setting(t, "deduce_mappings").and_then(Value::as_bool) != Some(false)
}

fn unattended(t: &Value) -> bool {
    setting(t, "unattended").and_then(Value::as_bool) == Some(true)
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let defer = ctx.bool_param("defer_validation", false)?;
    let cfg = build_config(ctx, &id, &body, ctx.now_ms)?;
    if ctx.state().metadata.xpack.get(TRANSFORMS, &id).is_some() {
        return Err(EsError::resource_already_exists(format!("Transform with id [{id}] already exists")));
    }
    if !defer && !unattended(&cfg) {
        validate_against_cluster(ctx.cluster, &cfg, ctx.now_ms).await?;
    }
    let now = ctx.now_ms;
    let mut cfg = cfg;
    set_user(&mut cfg, ctx);
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(TRANSFORMS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("Transform with id [{id}] already exists")));
            }
            store::put(st, TRANSFORMS, &id, cfg, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

/// Keeps the requesting user with a transform, to run it as ([`run::trigger`]); a superuser's
/// runs with the cluster's authority.
fn set_user(cfg: &mut Value, ctx: &Ctx) {
    if let (Some(u), Value::Obj(m)) = (internal::stored_user(crate::xpack::security::principal(ctx)), cfg) {
        m.insert("_roll".into(), obj! { "user" => u });
    }
}

pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    let defer = ctx.bool_param("defer_validation", false)?;
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !UPDATE_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, "data_frame_transform_config_update", k));
        }
    }
    if body.get("sync").is_some_and(Value::is_null) {
        let (l, c) = crate::xpack::value_loc(ctx, "sync");
        return Err(EsError::x_content_parse(l, c, "[data_frame_transform_config_update] sync doesn't support values of type: VALUE_NULL"));
    }
    request_invalid(config_errors(None, &body))?;
    let source = match body.get("source") {
        Some(s) => Some(normalize_source(s)?),
        None => None,
    };
    // The updated configuration, checked against the cluster before it is stored.
    let cur = get_one(&ctx.state(), &id)?;
    let Value::Obj(mut m) = cur else { return Err(not_found(&id)) };
    if let Some(sync) = body.get("sync") {
        let from = m.get("sync").and_then(Value::as_object).and_then(|s| s.keys().next().cloned());
        let to = sync.as_object().and_then(|s| s.keys().next().cloned()).unwrap_or_default();
        if from.as_deref() != Some(to.as_str()) {
            return Err(EsError::new(
                400,
                "status_exception",
                format!("Cannot change the current sync configuration of transform [{id}] from [{}] to [{to}]", from.as_deref().unwrap_or("null")),
            ));
        }
    }
    if let Some(s) = source {
        m.insert("source".into(), s);
    }
    for k in ["dest", "frequency", "description", "_meta"] {
        if let Some(v) = body.get(k) {
            m.insert(k.into(), v.clone());
        }
    }
    if let Some(v) = body.get("sync") {
        m.insert("sync".into(), normalize_sync(v));
    }
    match body.get("retention_policy") {
        Some(Value::Null) => {
            m.shift_remove("retention_policy");
        }
        Some(v) => {
            m.insert("retention_policy".into(), v.clone());
        }
        None => {}
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
    if !defer {
        let candidate = Value::Obj(m.clone());
        if !unattended(&candidate) {
            let errs = source_dest_errors(&ctx.state(), &candidate, ctx.now_ms);
            if !errs.is_empty() {
                return Err(validation_failed(&errs));
            }
        }
    }
    let user = internal::stored_user(crate::xpack::security::principal(ctx));
    let now = ctx.now_ms;
    let out = ctx
        .cluster
        .update(move |st| {
            // Only the configuration changes: keep the run state stored meanwhile, with the
            // updater as the user it runs as (Elasticsearch replaces the stored headers).
            let mut roll = st.metadata.xpack.get(TRANSFORMS, &id).ok_or_else(|| not_found(&id))?.get("_roll").and_then(Value::as_object).cloned().unwrap_or_default();
            roll.shift_remove("user");
            if let Some(u) = user.clone() {
                roll.insert("user".into(), u);
            }
            if !roll.is_empty() {
                m.insert("_roll".into(), Value::Obj(roll));
            }
            let v = Value::Obj(reorder(m));
            store::put(st, TRANSFORMS, &id, v.clone(), now)?;
            Ok(v)
        })
        .await?;
    Reply::ok(public(&out))
}

fn expand(ctx: &Ctx, st: &crate::meta::ClusterState) -> EsResult<Vec<(String, Value)>> {
    let allow = ctx.bool_param("allow_no_match", true)?;
    let expr = ctx.path_param("transform_id");
    let out = crate::xpack::ml::expand(st, TRANSFORMS, expr, allow, not_found)?;
    // `_all` and `*` match nothing on an empty cluster: that is a miss too.
    if out.is_empty() && !allow {
        return Err(not_found(expr.unwrap_or("_all")));
    }
    Ok(out)
}

fn paged(ctx: &Ctx, items: Vec<Value>) -> EsResult<(usize, Vec<Value>)> {
    let from = ctx.int_param("from", 0)?.max(0) as usize;
    let size = ctx.int_param("size", 100)?;
    if size > MAX_GET_SIZE {
        return Err(EsError::action_request_validation(&[format!("Param [size] has a max acceptable value of [{MAX_GET_SIZE}]")]));
    }
    let size = size.max(0) as usize;
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
    // Stats of transforms that don't exist are an empty list, never a 404.
    ctx.bool_param("allow_no_match", true)?;
    let found = crate::xpack::ml::expand(&st, TRANSFORMS, ctx.path_param("transform_id"), true, not_found).unwrap_or_default();
    for (id, t) in found {
        let r = roll_state(&t);
        let state = shown_state(ctx.cluster, &id, &t);
        let mut m = Map::new();
        m.insert("id".into(), id.clone().into());
        m.insert("state".into(), state.clone().into());
        if let Some(reason) = r.get("reason").filter(|_| state == "failed") {
            m.insert("reason".into(), reason.clone());
        }
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
        m.insert("health".into(), obj! { "status" => if state == "failed" { "red" } else { "green" } });
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
    let from = match ctx.param("from") {
        Some(text) => {
            let fmt = crate::time::DateFormatter::new("strict_date_optional_time||epoch_millis").map_err(EsError::illegal_argument)?;
            match crate::time::parse_date_math(text, ctx.now_ms, false, None, &fmt) {
                Ok(i) => Some(i.millis()),
                Err(e) => return Err(EsError::parse("Failed to parse date for [from]").caused_by(e.to_es())),
            }
        }
        None => None,
    };
    let t = get_one(&ctx.state(), &id)?;
    if from.is_some() && t.get("sync").is_none() {
        return Err(EsError::new(
            400,
            "status_exception",
            format!("Transform configuration [{id}] has invalid elements: [Validation Failed: 1: [from] parameter is currently not supported for batch (non-continuous) transforms;]"),
        ));
    }
    match state_of(&t).as_str() {
        "stopped" => {}
        "failed" => {
            return Err(conflict(format!(
                "Unable to start transform [{id}] as it is in a failed state. Use force stop and then restart the transform once error is resolved. More details: [{}]",
                roll_state(&t).get("reason").map(Value::to_es_string).unwrap_or_default()
            )));
        }
        _ => return Err(conflict(format!("Cannot start transform [{id}] as it is already started."))),
    }
    let config = public(&t);
    if !unattended(&t) {
        validate_against_cluster(ctx.cluster, &config, ctx.now_ms).await?;
    }
    // Elasticsearch creates the destination when it starts the transform, before its first
    // checkpoint runs.
    let created = ensure_dest(ctx.cluster, &config, ctx.now_ms).await?;
    let now = ctx.now_ms;
    let key = id.clone();
    ctx.cluster
        .update(move |st| {
            let t = get_one(st, &key)?;
            if state_of(&t) != "stopped" {
                return Err(conflict(format!("Cannot start transform [{key}] as it is already started.")));
            }
            let v = with_roll_state(t, |r| {
                r.insert("state".into(), "started".into());
                r.insert("next_run".into(), now.into());
                r.shift_remove("failure_count");
                r.shift_remove("reason");
                match from {
                    Some(f) => r.insert("from".into(), f.into()),
                    None => r.shift_remove("from"),
                };
                if created {
                    r.insert("created_dest".into(), true.into());
                }
            });
            store::put(st, TRANSFORMS, &key, v, now).map(|_| ())
        })
        .await?;
    if ctx.cluster.handle().is_none() {
        // No background work on this cluster: the first trigger runs before the answer.
        trigger(ctx.cluster, &id).await;
    } else if config.get("sync").is_some() {
        // A continuous transform answers once its first checkpoint is done (a batch one
        // answers at once, and runs in the background). The scheduler, woken by the state
        // change, runs it.
        let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
        let cluster = ctx.cluster;
        cluster.xpack.transforms.wait_until(wait_limit(timeout), || first_trigger_done(cluster, &id, now)).await;
    }
    Reply::acknowledged()
}

fn wait_limit(t: crate::units::TimeValue) -> std::time::Duration {
    std::time::Duration::from_millis(t.as_millis().max(0) as u64)
}

/// Whether the trigger due at `since` has run (or the transform stopped or went away).
fn first_trigger_done(cluster: &Cluster, id: &str, since: i64) -> bool {
    match cluster.state().metadata.xpack.get(TRANSFORMS, id) {
        Some(t) => state_of(t) != "started" || next_run(t).is_none_or(|n| n > since),
        None => true,
    }
}

pub async fn schedule_now(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("transform_id").unwrap_or_default().to_string();
    if id == "_all" || id == "*" {
        return Err(EsError::action_request_validation(&["_schedule_now API does not support _all wildcard".into()]));
    }
    let t = get_one(&ctx.state(), &id)?;
    if state_of(&t) == "started" {
        let now = ctx.now_ms;
        let key = id.clone();
        ctx.cluster
            .update(move |st| {
                let Some(t) = st.metadata.xpack.get(TRANSFORMS, &key).cloned() else { return Ok(()) };
                let v = with_roll_state(t, |r| {
                    r.insert("next_run".into(), now.into());
                });
                store::put(st, TRANSFORMS, &key, v, now).map(|_| ())
            })
            .await?;
        if ctx.cluster.handle().is_none() {
            trigger(ctx.cluster, &id).await;
        }
    }
    Reply::acknowledged()
}

pub async fn stop(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let force = ctx.bool_param("force", false)?;
    if force && ctx.bool_param("wait_for_checkpoint", false)? {
        return Err(EsError::action_request_validation(&["cannot set both [force] and [wait_for_checkpoint] to true".into()]));
    }
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let found = crate::xpack::ml::expand(&st, TRANSFORMS, ctx.path_param("transform_id"), allow, not_found)?;
    drop(st);
    if !force && let Some((id, t)) = found.iter().find(|(_, t)| state_of(t) == "failed") {
        return Err(conflict(format!(
            "Unable to stop transform [{id}] as it is in a failed state. Use force stop to stop the transform. More details: [{}]",
            roll_state(t).get("reason").map(Value::to_es_string).unwrap_or_default()
        )));
    }
    let ids: Vec<String> = found.into_iter().map(|(k, _)| k).collect();
    let now = ctx.now_ms;
    let stopping = ids.clone();
    ctx.cluster
        .update(move |st| {
            for id in stopping {
                if let Some(t) = st.metadata.xpack.get(TRANSFORMS, &id).cloned()
                    && state_of(&t) != "stopped"
                {
                    let v = with_roll_state(t, |r| {
                        r.insert("state".into(), "stopped".into());
                        r.shift_remove("next_run");
                    });
                    store::put(st, TRANSFORMS, &id, v, now)?;
                }
            }
            Ok(())
        })
        .await?;
    // A trigger running now finishes its checkpoint; waiting for completion waits for it.
    if wait {
        let runs = &ctx.cluster.xpack.transforms;
        runs.wait_until(wait_limit(timeout), || ids.iter().all(|id| !runs.is_running(id))).await;
    }
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

/// Previews a transform: the first page of output documents and the destination it would create.
async fn preview_of(ctx: &Ctx<'_>, t: &Value, id: &str) -> EsResult<Value> {
    let req = composite_request(t, PREVIEW_SIZE, None);
    let resp = internal::call(ctx.cluster, "POST", &search_uri(t), Some(&req)).await?;
    let buckets = resp.pointer(&["aggregations", "_transform", "buckets"]).and_then(Value::as_array).cloned().unwrap_or_default();
    let props = if deduces_mappings(t) { deduced_mappings(ctx.cluster, t).await } else { Map::new() };
    let mut docs: Vec<Value> = bucket_docs(t, &buckets, &props).into_iter().map(|(_, d)| d).collect();
    // The destination's pipeline runs over the preview, as it would over the real documents.
    if let Some(p) = t.pointer(&["dest", "pipeline"]).and_then(Value::as_str)
        && !docs.is_empty()
    {
        let sim = obj! { "docs" => Value::Arr(docs.iter().map(|d| obj! { "_source" => d.clone() }).collect()) };
        let out = internal::call(ctx.cluster, "POST", &format!("/_ingest/pipeline/{}/_simulate", enc(p)), Some(&sim)).await?;
        docs = out.get("docs").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.pointer(&["doc", "_source"]).cloned()).collect();
    }
    Ok(obj! { "preview" => Value::Arr(docs), "generated_dest_index" => generated_dest(id, ctx.now_ms, props) })
}

/// `TransformConfigLinter`: what makes a transform slow, as the preview warns.
fn lint(t: &Value) -> Vec<String> {
    let runtime = t.pointer(&["source", "runtime_mappings"]).and_then(Value::as_object);
    let is_runtime = |f: &str| runtime.is_some_and(|r| r.contains_key(f));
    let mut out = Vec::new();
    let group_fields: Vec<String> = t
        .pointer(&["pivot", "group_by"])
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|g| g.values())
        .filter_map(|g| g.as_object().and_then(|m| m.values().next()).and_then(|s| s.get("field")).map(Value::to_es_string))
        .collect();
    if !group_fields.is_empty() && group_fields.iter().all(|f| is_runtime(f)) {
        out.push("all the group-by fields are script-based runtime fields, this transform might run slowly, please check your configuration.".to_string());
    }
    if t.pointer(&["sync", "time", "field"]).and_then(Value::as_str).is_some_and(is_runtime) {
        out.push("sync time field is a script-based runtime field, this transform might run slowly, please check your configuration.".to_string());
    }
    out
}

pub async fn preview(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let body = ctx.body_map()?;
    let (t, id) = match (ctx.path_param("transform_id"), body) {
        (Some(_), Some(_)) => return Err(EsError::new(400, "status_exception", "Please provide either a transform [id] or the config object but not both")),
        (None, None) => return Err(EsError::new(400, "status_exception", "Please provide a transform [id] or the config object")),
        (Some(id), None) => (public(&get_one(&ctx.state(), id)?), id.to_string()),
        (None, Some(mut body)) => {
            // A preview writes nowhere: its destination index is a placeholder.
            let dest = body.entry("dest".to_string()).or_insert_with(Value::object);
            if let Value::Obj(d) = dest {
                d.entry("index".to_string()).or_insert_with(|| Value::from("unused-transform-preview-index"));
            }
            (build_config(ctx, "transform-preview", &body, ctx.now_ms)?, "transform-preview".to_string())
        }
    };
    let errs = source_dest_errors(&ctx.state(), &t, ctx.now_ms);
    if !errs.is_empty() {
        return Err(validation_failed(&errs));
    }
    unsupported_aggs(&t)?;
    ctx.warn_all(lint(&t));
    Reply::ok(preview_of(ctx, &t, &id).await?)
}

macro_rules! col {
    ($n:literal, [$($a:literal),*], $d:literal, $default:literal, $right:literal) => {
        crate::api::cat::Column { name: $n, aliases: &[$($a),*], desc: Some($d), default: $default, right: $right, sibling: None, cond: crate::api::cat::Cond::Always }
    };
}

/// `RestCatTransformAction`'s columns.
static CAT_COLUMNS: &[crate::api::cat::Column] = &[
    col!("id", [], "the id", true, false),
    col!("state", ["s"], "transform state", true, true),
    col!("checkpoint", ["c"], "checkpoint", true, false),
    col!("documents_processed", ["docp", "documentsProcessed"], "the number of documents read from source indices and processed", true, false),
    col!("checkpoint_progress", ["cp", "checkpointProgress"], "progress of the checkpoint", true, false),
    col!("last_search_time", ["lst", "lastSearchTime"], "last time transform searched for updates", true, false),
    col!("changes_last_detection_time", ["cldt"], "changes last detected time", true, false),
    col!("create_time", ["ct", "createTime"], "transform creation time", false, false),
    col!("version", ["v"], "the version of Elasticsearch when the transform was created", false, false),
    col!("source_index", ["si", "sourceIndex"], "source index", false, false),
    col!("project_routing", ["pr", "projectRouting"], "source project routing filter", false, false),
    col!("dest_index", ["di", "destIndex"], "destination index", false, false),
    col!("pipeline", ["p"], "transform pipeline", false, false),
    col!("description", ["d"], "description", false, false),
    col!("transform_type", ["tt"], "batch or continuous transform", false, false),
    col!("frequency", ["f"], "frequency of transform", false, false),
    col!("max_page_search_size", ["mpsz"], "max page search size", false, false),
    col!("docs_per_second", ["dps"], "docs per second", false, false),
    col!("reason", ["r", "reason"], "reason for the current state", false, false),
    col!("search_total", ["st"], "total number of search phases", false, false),
    col!("search_failure", ["sf"], "total number of search failures", false, false),
    col!("search_time", ["stime"], "total search time", false, false),
    col!("index_total", ["it"], "total number of index phases done by the transform", false, false),
    col!("index_failure", ["if"], "total number of index failures", false, false),
    col!("index_time", ["itime"], "total time spent indexing documents", false, false),
    col!("documents_indexed", ["doci"], "the number of documents written to the destination index", false, false),
    col!("delete_time", ["dtime"], "total time spent deleting documents", false, false),
    col!("documents_deleted", ["docd"], "the number of documents deleted from the destination index", false, false),
    col!("trigger_count", ["tc"], "the number of times the transform has been triggered", false, false),
    col!("pages_processed", ["pp"], "the number of pages processed", false, false),
    col!("processing_time", ["pt"], "the total time spent processing documents", false, false),
    col!("checkpoint_duration_time_exp_avg", ["cdtea", "checkpointTimeExpAvg"], "exponential average checkpoint processing time (milliseconds)", false, false),
    col!("indexed_documents_exp_avg", ["idea"], "exponential average number of documents indexed", false, false),
    col!("processed_documents_exp_avg", ["pdea"], "exponential average number of documents processed", false, false),
];

pub fn cat(ctx: &Ctx) -> EsResult<Reply> {
    use crate::api::cat::{Cell, Table};
    use crate::units::TimeValue;
    let st = ctx.state();
    let mut t = Table::new(CAT_COLUMNS);
    let ids = crate::xpack::ml::expand(&st, TRANSFORMS, ctx.path_param("transform_id"), ctx.bool_param("allow_no_match", true)?, not_found)?;
    let fmt = |ms: i64| crate::time::DateFormatter::new("EEE MMM dd HH:mm:ss 'GMT' yyyy").map(|f| f.format_millis(ms)).unwrap_or_default();
    let opt_str = |v: Option<&Value>| v.filter(|v| !v.is_null()).map_or(Cell::Null, |v| Cell::Str(v.to_es_string()));
    for (id, tr) in ids {
        let r = roll_state(&tr);
        let stats = r.get("stats").and_then(Value::as_object).cloned().unwrap_or_default();
        let count = |k: &str| Cell::Str(stats.get(k).and_then(Value::as_i64).unwrap_or(0).to_string());
        let time = |k: &str| Cell::Time(TimeValue::millis(stats.get(k).and_then(Value::as_i64).unwrap_or(0)));
        let avg = |k: &str| Cell::Str(format!("{:.2}", stats.get(k).and_then(Value::as_f64).unwrap_or(0.0)));
        let cp = r.get("checkpoint").and_then(Value::as_i64).unwrap_or(0);
        let state = shown_state(ctx.cluster, &id, &tr);
        let started = state == "started" || state == "indexing";
        let last = r.get("changes_last_detected_at").and_then(Value::as_i64).filter(|_| started);
        let frequency = tr.get("frequency").and_then(|f| TimeValue::parse(&f.to_es_string(), "frequency").ok()).unwrap_or(TimeValue::millis(60_000));
        let page = setting(&tr, "max_page_search_size").or_else(|| tr.pointer(&["pivot", "max_page_search_size"])).and_then(Value::as_i64).unwrap_or(PAGE_SIZE);
        t.rows.push(vec![
            Cell::Str(id),
            Cell::Str(state),
            Cell::Str(cp.to_string()),
            count("documents_processed"),
            if started && cp > 0 { Cell::Str("100.00".into()) } else { Cell::Null },
            last.map_or(Cell::Null, |ms| Cell::Str(fmt(ms))),
            last.map_or(Cell::Null, |ms| Cell::Str(fmt(ms))),
            tr.get("create_time").and_then(Value::as_i64).map_or(Cell::Null, |ms| Cell::Str(fmt(ms))),
            opt_str(tr.get("version")),
            Cell::Str(source_indices(&tr).join(",")),
            opt_str(tr.pointer(&["source", "project_routing"])),
            Cell::Str(dest_index(&tr).to_string()),
            opt_str(tr.pointer(&["dest", "pipeline"])),
            opt_str(tr.get("description")),
            Cell::str(if tr.get("sync").is_some() { "continuous" } else { "batch" }),
            Cell::Str(frequency.human()),
            Cell::Str(page.to_string()),
            setting(&tr, "docs_per_second").map_or(Cell::str("-"), |v| Cell::Str(v.to_es_string())),
            opt_str(r.get("reason")),
            count("search_total"),
            count("search_failures"),
            time("search_time_in_ms"),
            count("index_total"),
            count("index_failures"),
            time("index_time_in_ms"),
            count("documents_indexed"),
            time("delete_time_in_ms"),
            count("documents_deleted"),
            count("trigger_count"),
            count("pages_processed"),
            time("processing_time_in_ms"),
            avg("exponential_avg_checkpoint_duration_ms"),
            avg("exponential_avg_documents_indexed"),
            avg("exponential_avg_documents_processed"),
        ]);
    }
    Ok(Reply::Table(t))
}
