//! Connectors (`_connector`) and connector sync jobs (`_connector/_sync_job`), plus the
//! internal connector secrets API (`_connector/_secret`).
//!
//! A connector is the record an Elastic connector service reads and updates: its target index,
//! configuration, filtering (a draft that becomes active once validated), scheduling, status
//! (Elasticsearch's `ConnectorStateMachine`) and sync bookkeeping. Sync jobs carry a snapshot
//! of their connector and move through `ConnectorSyncJobStateMachine`; invalid transitions fail
//! with Elasticsearch's `status_exception`. Deletes are soft unless `hard=true`.

use super::now_nanos_string;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{store, unknown_field};

pub const CONNECTORS: &str = "connector";
pub const SYNC_JOBS: &str = "connector.sync_job";
pub const SECRETS: &str = "connector_secret";

const CONNECTOR_STATUSES: [&str; 5] = ["created", "needs_configuration", "configured", "connected", "error"];

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("connector [{id}] not found"))
}

fn updated() -> EsResult<Reply> {
    Reply::ok(obj! { "result" => "updated" })
}

fn default_filtering(now: &str) -> Value {
    let rule = obj! {
        "created_at" => now, "field" => "_", "id" => "DEFAULT", "order" => 0, "policy" => "include", "rule" => "regex", "updated_at" => now, "value" => ".*",
    };
    let rules = obj! {
        "advanced_snippet" => obj! { "created_at" => now, "updated_at" => now, "value" => obj! {} },
        "rules" => crate::arr![rule],
        "validation" => obj! { "errors" => Value::Arr(vec![]), "state" => "valid" },
    };
    crate::arr![obj! { "active" => rules.clone(), "domain" => "DEFAULT", "draft" => rules }]
}

fn default_scheduling() -> Value {
    let s = obj! { "enabled" => false, "interval" => "0 0 0 * * ?" };
    obj! { "access_control" => s.clone(), "full" => s.clone(), "incremental" => s }
}

fn new_connector(id: &str, body: &Map) -> Value {
    let now = now_nanos_string();
    let mut m = Map::new();
    m.insert("id".into(), id.into());
    if let Some(v) = body.get("api_key_id") {
        m.insert("api_key_id".into(), v.clone());
    }
    m.insert("configuration".into(), Value::object());
    m.insert("custom_scheduling".into(), Value::object());
    if let Some(d) = body.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert("filtering".into(), default_filtering(&now));
    m.insert("index_name".into(), body.get("index_name").cloned().unwrap_or(Value::Null));
    m.insert("is_native".into(), body.get("is_native").cloned().unwrap_or(Value::Bool(false)));
    if let Some(l) = body.get("language") {
        m.insert("language".into(), l.clone());
    }
    m.insert("name".into(), body.get("name").cloned().unwrap_or_else(|| Value::from("")));
    m.insert("scheduling".into(), default_scheduling());
    if let Some(s) = body.get("service_type") {
        m.insert("service_type".into(), s.clone());
    }
    m.insert("status".into(), "created".into());
    m.insert("sync_now".into(), false.into());
    m.insert("deleted".into(), false.into());
    if m.get("index_name").is_some_and(Value::is_null) {
        m.shift_remove("index_name");
    }
    Value::Obj(m)
}

const PUT_FIELDS: [&str; 6] = ["description", "index_name", "is_native", "language", "name", "service_type"];

fn check_index_name(st: &crate::meta::ClusterState, id: &str, index: Option<&Value>) -> EsResult<()> {
    let Some(Value::Str(index)) = index else { return Ok(()) };
    crate::meta::names::validate_index_name(index)?;
    if st.metadata.xpack.iter(CONNECTORS).any(|(k, c)| k != id && c.get("deleted") != Some(&Value::Bool(true)) && c.get("index_name").and_then(Value::as_str) == Some(index)) {
        return Err(EsError::illegal_argument(format!("Index name [{index}] is used by another connector.")));
    }
    Ok(())
}

async fn create(ctx: &Ctx<'_>, id: String) -> EsResult<(bool, String)> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !PUT_FIELDS.contains(&k.as_str()) {
            return Err(unknown_field(ctx, if ctx.method == crate::routes::Method::Post { "connector_post_request" } else { "connector_put_request" }, k));
        }
    }
    let now = ctx.now_ms;
    let created = ctx
        .cluster
        .update(move |st| {
            check_index_name(st, &id, body.get("index_name"))?;
            let c = new_connector(&id, &body);
            store::put(st, CONNECTORS, &id, c, now)
        })
        .await?;
    Ok((created, String::new()))
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let (created, _) = create(ctx, id.clone()).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "id" => id, "result" => if created { "created" } else { "updated" } }))
}

pub async fn post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = crate::meta::names::time_based_id();
    create(ctx, id.clone()).await?;
    Ok(Reply::Body(201, obj! { "id" => id, "result" => "created" }))
}

fn live(st: &crate::meta::ClusterState, id: &str, include_deleted: bool) -> EsResult<Value> {
    match st.metadata.xpack.get(CONNECTORS, id) {
        Some(c) if include_deleted || c.get("deleted") != Some(&Value::Bool(true)) => Ok(c.clone()),
        _ => Err(not_found(id)),
    }
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default();
    let include_deleted = ctx.bool_param("include_deleted", false)?;
    Reply::ok(live(&ctx.state(), id, include_deleted)?)
}

pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let from = ctx.int_param("from", 0)?;
    let size = ctx.int_param("size", 100)?;
    if from < 0 {
        return Err(EsError::action_request_validation(&["[from] parameter cannot be negative, found [".to_string() + &from.to_string() + "]"]));
    }
    if size < 0 {
        return Err(EsError::action_request_validation(&["[size] parameter cannot be negative, found [".to_string() + &size.to_string() + "]"]));
    }
    let include_deleted = ctx.bool_param("include_deleted", false)?;
    let filter = |name: &str| ctx.list_param(name).unwrap_or_default();
    let (indices, names, types) = (filter("index_name"), filter("connector_name"), filter("service_type"));
    let query = ctx.query("query").map(str::to_lowercase);
    let mut all: Vec<&Value> = st
        .metadata
        .xpack
        .iter(CONNECTORS)
        .map(|(_, c)| c)
        .filter(|c| include_deleted || c.get("deleted") != Some(&Value::Bool(true)))
        .filter(|c| indices.is_empty() || c.get("index_name").and_then(Value::as_str).is_some_and(|i| indices.iter().any(|p| crate::xcontent::filter::glob_match(p, i))))
        .filter(|c| names.is_empty() || c.get("name").and_then(Value::as_str).is_some_and(|n| names.iter().any(|p| p == n)))
        .filter(|c| types.is_empty() || c.get("service_type").and_then(Value::as_str).is_some_and(|n| types.iter().any(|p| p == n)))
        .filter(|c| {
            query.as_ref().is_none_or(|q| {
                ["name", "description", "index_name"].iter().any(|f| c.get(f).and_then(Value::as_str).is_some_and(|s| s.to_lowercase().contains(q.as_str())))
            })
        })
        .collect();
    all.sort_by(|a, b| {
        let k = |c: &Value| c.get("index_name").and_then(Value::as_str).unwrap_or("").to_string();
        k(a).cmp(&k(b))
    });
    let count = all.len();
    let results: Vec<Value> = all.into_iter().skip(from as usize).take(size as usize).cloned().collect();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(results) })
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let hard = ctx.bool_param("hard", false)?;
    let delete_jobs = ctx.bool_param("delete_sync_jobs", false)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let c = live(st, &id, hard)?;
            if hard {
                store::remove(st, CONNECTORS, &id);
            } else {
                let mut c = c;
                if let Value::Obj(m) = &mut c {
                    m.insert("deleted".into(), true.into());
                }
                store::put(st, CONNECTORS, &id, c, now)?;
            }
            if delete_jobs {
                let jobs: Vec<String> = st.metadata.xpack.iter(SYNC_JOBS).filter(|(_, j)| j.pointer(&["connector", "id"]).and_then(Value::as_str) == Some(&id)).map(|(k, _)| k.clone()).collect();
                for j in jobs {
                    store::remove(st, SYNC_JOBS, &j);
                }
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

/// Applies `f` to a live connector and stores it.
async fn modify(ctx: &Ctx<'_>, f: impl FnOnce(&crate::meta::ClusterState, &mut Map) -> EsResult<()> + Send) -> EsResult<Reply> {
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let c = live(st, &id, false)?;
            let Value::Obj(mut m) = c else { return Err(not_found(&id)) };
            f(st, &mut m)?;
            store::put(st, CONNECTORS, &id, Value::Obj(m), now).map(|_| ())
        })
        .await?;
    updated()
}

fn required<'a>(body: &'a Map, key: &str) -> EsResult<&'a Value> {
    body.get(key).ok_or_else(|| EsError::illegal_argument(format!("Required [{key}]")))
}

fn body_fields(ctx: &Ctx, parser: &str, allowed: &[&str]) -> EsResult<Map> {
    let body = ctx.body_map()?.unwrap_or_default();
    for k in body.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(unknown_field(ctx, parser, k));
        }
    }
    Ok(body)
}

pub async fn check_in(ctx: &Ctx<'_>) -> EsResult<Reply> {
    modify(ctx, |_, m| {
        m.insert("last_seen".into(), now_nanos_string().into());
        Ok(())
    })
    .await
}

pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    use crate::routes::Api;
    let api = ctx.api;
    let body = match api {
        Api::ConnectorUpdateActiveFiltering => Map::new(),
        Api::ConnectorUpdateName => body_fields(ctx, "connector_update_name_request", &["name", "description"])?,
        Api::ConnectorUpdateFiltering => body_fields(ctx, "connector_update_filtering_request", &["filtering", "advanced_snippet", "rules"])?,
        Api::ConnectorUpdateFilteringValidation => body_fields(ctx, "connector_update_filtering_validation_request", &["validation"])?,
        Api::ConnectorUpdateError => body_fields(ctx, "connector_update_error_request", &["error"])?,
        Api::ConnectorUpdateConfiguration => body_fields(ctx, "connector_update_configuration_request", &["configuration", "values"])?,
        Api::ConnectorUpdateIndexName => body_fields(ctx, "connector_update_index_name_request", &["index_name"])?,
        Api::ConnectorUpdateNative => body_fields(ctx, "connector_update_native_request", &["is_native"])?,
        Api::ConnectorUpdatePipeline => body_fields(ctx, "connector_update_pipeline_request", &["pipeline"])?,
        Api::ConnectorUpdateScheduling => body_fields(ctx, "connector_update_scheduling_request", &["scheduling"])?,
        Api::ConnectorUpdateStatus => body_fields(ctx, "connector_update_status_request", &["status"])?,
        Api::ConnectorUpdateServiceType => body_fields(ctx, "connector_update_service_type_request", &["service_type"])?,
        Api::ConnectorUpdateApiKeyId => body_fields(ctx, "connector_update_api_key_id_request", &["api_key_id", "api_key_secret_id"])?,
        Api::ConnectorUpdateFeatures => body_fields(ctx, "connector_update_features_request", &["features"])?,
        _ => ctx.body_map()?.unwrap_or_default(),
    };
    // The status parses with the request, before the connector is looked up.
    if api == Api::ConnectorUpdateStatus
        && let Some(v) = body.get("status")
        && !v.is_null()
        && !CONNECTOR_STATUSES.contains(&v.to_es_string().as_str())
    {
        let (l, c) = crate::xpack::value_loc(ctx, "status");
        return Err(EsError::x_content_parse(l, c, "[connector_update_status_request] failed to parse field [status]")
            .caused_by(EsError::illegal_argument(format!("Unknown ConnectorStatus [{}].", v.to_es_string()))));
    }
    let id = ctx.path_param("connector_id").unwrap_or_default().to_string();
    modify(ctx, move |st, m| {
        match api {
            Api::ConnectorUpdateName => {
                if !body.contains_key("name") && !body.contains_key("description") {
                    return Err(EsError::action_request_validation(&["[name] and [description] cannot both be [null]. Please provide a value for at least one of them.".into()]));
                }
                for k in ["name", "description"] {
                    if let Some(v) = body.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
            }
            Api::ConnectorUpdateFiltering => {
                let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(Value::Arr(f)) = body.get("filtering") {
                    filtering = f.clone();
                } else {
                    if !body.contains_key("advanced_snippet") && !body.contains_key("rules") {
                        return Err(EsError::action_request_validation(&["[advanced_snippet] and [rules] cannot be both [null]. Please provide a value for at least one of them.".into()]));
                    }
                    let now = now_nanos_string();
                    let first = filtering.first().cloned().unwrap_or_else(|| default_filtering(&now).as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null));
                    let mut entry = first.as_object().cloned().unwrap_or_default();
                    let mut draft = entry.get("draft").and_then(Value::as_object).cloned().unwrap_or_default();
                    if let Some(s) = body.get("advanced_snippet") {
                        draft.insert("advanced_snippet".into(), s.clone());
                    }
                    if let Some(r) = body.get("rules") {
                        draft.insert("rules".into(), r.clone());
                    }
                    draft.insert("validation".into(), obj! { "state" => "edited", "errors" => Value::Arr(vec![]) });
                    entry.insert("draft".into(), Value::Obj(draft));
                    if filtering.is_empty() {
                        filtering.push(Value::Obj(entry));
                    } else {
                        filtering[0] = Value::Obj(entry);
                    }
                }
                m.insert("filtering".into(), Value::Arr(filtering));
            }
            Api::ConnectorUpdateFilteringValidation => {
                let v = required(&body, "validation")?.clone();
                let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(Value::Obj(entry)) = filtering.first_mut()
                    && let Some(Value::Obj(draft)) = entry.get_mut("draft")
                {
                    draft.insert("validation".into(), v);
                }
                m.insert("filtering".into(), Value::Arr(filtering));
            }
            Api::ConnectorUpdateActiveFiltering => {
                let mut filtering = m.get("filtering").and_then(Value::as_array).cloned().unwrap_or_default();
                if let Some(Value::Obj(entry)) = filtering.first_mut() {
                    let draft = entry.get("draft").cloned().unwrap_or(Value::Null);
                    let state = draft.pointer(&["validation", "state"]).and_then(Value::as_str).unwrap_or("");
                    if state != "valid" {
                        return Err(EsError::new(
                            400,
                            "status_exception",
                            format!("Filtering draft needs to be validated by the connector service before activation. Current filtering draft validation state [{state}]"),
                        ));
                    }
                    entry.insert("active".into(), draft);
                }
                m.insert("filtering".into(), Value::Arr(filtering));
            }
            Api::ConnectorUpdateError => {
                let e = required(&body, "error")?.clone();
                if !e.is_null() {
                    m.insert("status".into(), "error".into());
                }
                m.insert("error".into(), e);
            }
            Api::ConnectorUpdateConfiguration => {
                match (body.get("configuration"), body.get("values")) {
                    (Some(c), None) => {
                        m.insert("configuration".into(), normalize_configuration(c)?);
                        m.insert("status".into(), "configured".into());
                    }
                    (None, Some(Value::Obj(vals))) => {
                        let mut cfg = m.get("configuration").and_then(Value::as_object).cloned().unwrap_or_default();
                        for (k, v) in vals {
                            let Some(Value::Obj(entry)) = cfg.get_mut(k) else {
                                return Err(EsError::new(400, "status_exception", format!("Unknown [configuration] fields in the request payload: [{k}]. Remove them from request or register their schema first.")));
                            };
                            entry.insert("value".into(), v.clone());
                        }
                        m.insert("configuration".into(), Value::Obj(cfg));
                        m.insert("status".into(), "configured".into());
                    }
                    _ => {
                        return Err(EsError::action_request_validation(&["[configuration] and [values] cannot both be provided in the same request.".into()]));
                    }
                }
            }
            Api::ConnectorUpdateIndexName => {
                let idx = required(&body, "index_name")?.clone();
                check_index_name(st, &id, Some(&idx))?;
                m.insert("index_name".into(), idx);
            }
            Api::ConnectorUpdateNative => {
                let native = required(&body, "is_native")?.as_bool().unwrap_or(false);
                if native {
                    let index = m.get("index_name").and_then(Value::as_str).unwrap_or("");
                    if !index.starts_with("content-") {
                        return Err(EsError::new(400, "status_exception", format!(
                            "The index name [{index}] attached to the connector [{id}] must start with the required prefix: [content-] to be Elastic-managed. Please update the attached index first to comply with this requirement."
                        )));
                    }
                }
                m.insert("is_native".into(), native.into());
            }
            Api::ConnectorUpdatePipeline => {
                m.insert("pipeline".into(), required(&body, "pipeline")?.clone());
            }
            Api::ConnectorUpdateScheduling => {
                m.insert("scheduling".into(), required(&body, "scheduling")?.clone());
            }
            Api::ConnectorUpdateStatus => {
                let to = required(&body, "status")?.to_es_string();
                if !CONNECTOR_STATUSES.contains(&to.as_str()) {
                    return Err(EsError::illegal_argument(format!("Unknown ConnectorStatus [{to}].")));
                }
                let from = m.get("status").and_then(Value::as_str).unwrap_or("created").to_string();
                if !connector_transition(&from, &to) {
                    let msg = format!("Invalid transition attempt from [{from}] to [{to}]. Such a status transition is not supported by the Connector Protocol.");
                    return Err(EsError::new(400, "status_exception", msg.clone()).caused_by(EsError::new(400, "connector_invalid_status_transition_exception", msg)));
                }
                m.insert("status".into(), to.into());
            }
            Api::ConnectorUpdateServiceType => {
                m.insert("service_type".into(), required(&body, "service_type")?.clone());
            }
            Api::ConnectorUpdateApiKeyId => {
                if !body.contains_key("api_key_id") && !body.contains_key("api_key_secret_id") {
                    return Err(EsError::action_request_validation(&["[api_key_id] and [api_key_secret_id] cannot both be [null]. Please provide a value for at least one of them.".into()]));
                }
                for k in ["api_key_id", "api_key_secret_id"] {
                    if let Some(v) = body.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
            }
            Api::ConnectorUpdateFeatures => {
                let f = required(&body, "features")?;
                let mut cur = m.get("features").and_then(Value::as_object).cloned().unwrap_or_default();
                if let Value::Obj(fm) = f {
                    for (k, v) in fm {
                        cur.insert(k.clone(), v.clone());
                    }
                }
                m.insert("features".into(), Value::Obj(cur));
            }
            Api::ConnectorLastSync => {
                for (k, v) in &body {
                    m.insert(k.clone(), v.clone());
                }
            }
            _ => {}
        }
        Ok(())
    })
    .await
}

/// `ConnectorConfiguration` entries: `label` is required; `default_value`, `required` and
/// `sensitive` get their defaults.
fn normalize_configuration(c: &Value) -> EsResult<Value> {
    let Value::Obj(entries) = c else { return Err(EsError::illegal_argument("[configuration] must be an object")) };
    let mut out = Map::new();
    for (k, e) in entries {
        let Value::Obj(e) = e else { return Err(EsError::illegal_argument("Required [label]")) };
        if !e.contains_key("label") {
            return Err(EsError::x_content_parse_bare("[connector_update_configuration_request] failed to parse field [configuration]")
                .caused_by(EsError::illegal_argument("Required [label]")));
        }
        let mut n = e.clone();
        n.entry("default_value".to_string()).or_insert(Value::Null);
        n.entry("required".to_string()).or_insert(Value::Bool(false));
        n.entry("sensitive".to_string()).or_insert(Value::Bool(false));
        out.insert(k.clone(), Value::Obj(n));
    }
    Ok(Value::Obj(out))
}

/// `ConnectorStateMachine`.
fn connector_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("created", "needs_configuration" | "error")
            | ("needs_configuration", "configured" | "error")
            | ("configured", "needs_configuration" | "configured" | "connected" | "error")
            | ("connected", "connected" | "configured" | "error")
            | ("error", "connected" | "configured" | "error")
    )
}

// --- sync jobs ---------------------------------------------------------------------------------

/// `ConnectorSyncJobStateMachine`.
fn sync_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("pending", "in_progress" | "canceled")
            | ("in_progress", "canceling" | "completed" | "suspended" | "error")
            | ("suspended", "in_progress" | "canceled")
            | ("canceling", "canceled" | "error")
    )
}

fn sync_not_found(id: &str) -> EsError {
    EsError::resource_not_found(id.to_string())
}

fn invalid_sync(from: &str, to: &str) -> EsError {
    let msg = format!("Invalid transition attempt from [{from}] to [{to}]. Such a ConnectorSyncStatus transition is not supported by the Connector Protocol.");
    EsError::new(400, "status_exception", msg.clone()).caused_by(EsError::new(400, "connector_sync_job_invalid_status_transition_exception", msg))
}

pub async fn sync_job_post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(ctx, "connector_sync_job_post_request", &["id", "job_type", "trigger_method"])?;
    let Some(cid) = body.get("id").map(Value::to_es_string) else { return Err(EsError::illegal_argument("Required [id]")) };
    let job_type = body.get("job_type").map(Value::to_es_string).unwrap_or_else(|| "full".into());
    if !matches!(job_type.as_str(), "full" | "incremental" | "access_control") {
        return Err(EsError::x_content_parse_bare("Failed to build [connector_sync_job_post_request] after last required field arrived")
            .caused_by(EsError::illegal_argument(format!("Unknown ConnectorSyncJobType [{job_type}]."))));
    }
    let trigger = body.get("trigger_method").map(Value::to_es_string).unwrap_or_else(|| "on_demand".into());
    if !matches!(trigger.as_str(), "on_demand" | "scheduled") {
        return Err(EsError::x_content_parse_bare("Failed to build [connector_sync_job_post_request] after last required field arrived")
            .caused_by(EsError::illegal_argument(format!("Unknown ConnectorSyncJobTriggerMethod [{trigger}]."))));
    }
    let id = crate::meta::names::time_based_id();
    let out = id.clone();
    let now_ms = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(c) = st.metadata.xpack.get(CONNECTORS, &cid).filter(|c| c.get("deleted") != Some(&Value::Bool(true))).cloned() else {
                return Err(EsError::resource_not_found(format!("Connector with id '{cid}' does not exist.")));
            };
            let now = now_nanos_string();
            let mut snap = Map::new();
            snap.insert("id".into(), cid.clone().into());
            let filtering = c.get("filtering").and_then(Value::as_array).and_then(|f| f.first()).and_then(|f| f.get("active")).cloned().unwrap_or(Value::Null);
            snap.insert("filtering".into(), filtering);
            for k in ["index_name", "language", "pipeline", "service_type"] {
                if let Some(v) = c.get(k) {
                    snap.insert(k.into(), v.clone());
                }
            }
            snap.insert("configuration".into(), c.get("configuration").cloned().unwrap_or_else(Value::object));
            let job = obj! {
                "id" => id.clone(), "connector" => Value::Obj(snap), "created_at" => now.clone(), "deleted_document_count" => 0,
                "indexed_document_count" => 0, "indexed_document_volume" => 0, "job_type" => job_type, "last_seen" => now,
                "metadata" => obj! {}, "status" => "pending", "total_document_count" => 0, "trigger_method" => trigger,
            };
            store::put(st, SYNC_JOBS, &id, job, now_ms).map(|_| ())
        })
        .await?;
    Ok(Reply::Body(201, obj! { "id" => out }))
}

pub fn sync_job_get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default();
    ctx.state().metadata.xpack.get(SYNC_JOBS, id).cloned().map_or_else(|| Err(sync_not_found(id)), Reply::ok)
}

pub fn sync_job_list(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let from = ctx.int_param("from", 0)?.max(0) as usize;
    let size = ctx.int_param("size", 100)?.max(0) as usize;
    let cid = ctx.query("connector_id");
    let status = ctx.query("status");
    let types = ctx.list_param("job_type").unwrap_or_default();
    let mut jobs: Vec<&Value> = st
        .metadata
        .xpack
        .iter(SYNC_JOBS)
        .map(|(_, j)| j)
        .filter(|j| cid.is_none_or(|c| j.pointer(&["connector", "id"]).and_then(Value::as_str) == Some(c)))
        .filter(|j| status.is_none_or(|s| j.get("status").and_then(Value::as_str) == Some(s)))
        .filter(|j| types.is_empty() || j.get("job_type").and_then(Value::as_str).is_some_and(|t| types.iter().any(|x| x == t)))
        .collect();
    // Newest first, as Elasticsearch sorts by `created_at`.
    jobs.sort_by(|a, b| b.get("created_at").map(Value::to_es_string).cmp(&a.get("created_at").map(Value::to_es_string)));
    let count = jobs.len();
    Reply::ok(obj! { "count" => count, "results" => Value::Arr(jobs.into_iter().skip(from).take(size).cloned().collect()) })
}

async fn modify_job(ctx: &Ctx<'_>, f: impl FnOnce(&mut Map) -> EsResult<()> + Send) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default().to_string();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(Value::Obj(mut m)) = st.metadata.xpack.get(SYNC_JOBS, &id).cloned() else { return Err(sync_not_found(&id)) };
            f(&mut m)?;
            store::put(st, SYNC_JOBS, &id, Value::Obj(m), now).map(|_| ())
        })
        .await?;
    updated()
}

fn status_of(m: &Map) -> String {
    m.get("status").and_then(Value::as_str).unwrap_or("pending").to_string()
}

pub async fn sync_job_check_in(ctx: &Ctx<'_>) -> EsResult<Reply> {
    modify_job(ctx, |m| {
        m.insert("last_seen".into(), now_nanos_string().into());
        Ok(())
    })
    .await
}

pub async fn sync_job_cancel(ctx: &Ctx<'_>) -> EsResult<Reply> {
    modify_job(ctx, |m| {
        let from = status_of(m);
        let now = now_nanos_string();
        if from == "pending" || from == "suspended" {
            m.insert("status".into(), "canceled".into());
            m.insert("completed_at".into(), now.clone().into());
            m.insert("cancelation_requested_at".into(), now.clone().into());
            m.insert("canceled_at".into(), now.into());
        } else if sync_transition(&from, "canceling") {
            m.insert("status".into(), "canceling".into());
            m.insert("cancelation_requested_at".into(), now.into());
        } else {
            return Err(invalid_sync(&from, "canceling"));
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_error(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(ctx, "connector_sync_job_error_request", &["error"])?;
    let err = required(&body, "error")?.clone();
    modify_job(ctx, move |m| {
        let from = status_of(m);
        if !sync_transition(&from, "error") {
            return Err(invalid_sync(&from, "error"));
        }
        m.insert("status".into(), "error".into());
        m.insert("error".into(), err);
        Ok(())
    })
    .await
}

pub async fn sync_job_update_stats(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(
        ctx,
        "connector_sync_job_update_ingestion_stats",
        &["deleted_document_count", "indexed_document_count", "indexed_document_volume", "total_document_count", "last_seen", "metadata"],
    )?;
    let missing: Vec<&str> = ["deleted_document_count", "indexed_document_count", "indexed_document_volume"].into_iter().filter(|k| !body.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    modify_job(ctx, move |m| {
        for (k, v) in body {
            m.insert(k, v);
        }
        if !m.contains_key("last_seen") {
            m.insert("last_seen".into(), now_nanos_string().into());
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_claim(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = body_fields(ctx, "connector_sync_job_claim_request", &["worker_hostname", "sync_cursor"])?;
    let host = required(&body, "worker_hostname")?.clone();
    let cursor = body.get("sync_cursor").cloned();
    modify_job(ctx, move |m| {
        let now = now_nanos_string();
        m.insert("last_seen".into(), now.clone().into());
        m.insert("status".into(), "in_progress".into());
        m.insert("worker_hostname".into(), host);
        m.insert("started_at".into(), now.into());
        if let Some(c) = cursor
            && let Some(Value::Obj(conn)) = m.get_mut("connector")
        {
            conn.insert("sync_cursor".into(), c);
        }
        Ok(())
    })
    .await
}

pub async fn sync_job_delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("connector_sync_job_id").unwrap_or_default().to_string();
    ctx.cluster.update(move |st| store::remove(st, SYNC_JOBS, &id).map(|_| ()).ok_or_else(|| sync_not_found(&id))).await?;
    Reply::acknowledged()
}

// --- secrets -----------------------------------------------------------------------------------

pub async fn secret_post(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(v) = body.get("value").filter(|v| !v.is_null()) else {
        return Err(EsError::action_request_validation(&["[value] of the connector secret cannot be [null] or [\"\"]".into()]));
    };
    let id = crate::meta::names::time_based_id();
    let out = id.clone();
    let v = v.clone();
    let now = ctx.now_ms;
    ctx.cluster.update(move |st| store::put(st, SECRETS, &id, obj! { "value" => v }, now).map(|_| ())).await?;
    Reply::ok(obj! { "id" => out })
}

pub async fn secret_put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(v) = body.get("value").filter(|v| !v.is_null()).cloned() else {
        return Err(EsError::action_request_validation(&["[value] of the connector secret cannot be [null] or [\"\"]".into()]));
    };
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, SECRETS, &id, obj! { "value" => v }, now)).await?;
    Ok(Reply::Body(if created { 201 } else { 200 }, obj! { "result" => if created { "created" } else { "updated" } }))
}

pub fn secret_get(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default();
    match ctx.state().metadata.xpack.get(SECRETS, id) {
        Some(s) => Reply::ok(obj! { "id" => id, "value" => s.get("value").cloned().unwrap_or(Value::Null) }),
        None => Err(EsError::resource_not_found(format!("No secret with id [{id}]"))),
    }
}

pub async fn secret_delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| store::remove(st, SECRETS, &id).map(|_| ()).ok_or_else(|| EsError::resource_not_found(format!("No secret with id [{id}]"))))
        .await?;
    Reply::ok(obj! { "deleted" => true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machines() {
        assert!(connector_transition("created", "needs_configuration"));
        assert!(!connector_transition("error", "needs_configuration"));
        assert!(sync_transition("pending", "canceled"));
        assert!(!sync_transition("pending", "error"));
        assert!(!sync_transition("canceled", "canceling"));
        assert!(sync_transition("in_progress", "error"));
    }

    #[test]
    fn new_connector_shape() {
        let mut b = Map::new();
        b.insert("index_name".into(), "x".into());
        let c = new_connector("c", &b);
        let keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "configuration", "custom_scheduling", "filtering", "index_name", "is_native", "name", "scheduling", "status", "sync_now", "deleted"]);
    }
}
