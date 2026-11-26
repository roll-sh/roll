//! ML Commons (`_plugins/_ml`) on opensearch-flavor clusters, phase 1 of
//! docs/search/opensearch-neural.md: connectors, model groups, remote models, predict and the
//! task documents, in OpenSearch 3.9's shapes.
//!
//! - **Records** live in the cluster metadata, homed at ML Commons' system indices
//!   (`.plugins-ml-connector`, `-model-group`, `-model`, `-task`). Each keeps a `_version` and the
//!   family a `_seq_no`, which writes and `_search` answer as OpenSearch's index answers do.
//! - **Remote models** call their connector ([`connector`]); a model's inline connector is kept
//!   on the model, its credential sealed like a connector's.
//! - **Local models:** roll runs one, E5 (`intfloat/multilingual-e5-small`), pre-registered as
//!   [`E5_MODEL_ID`] and registrable under its name. Other pretrained names and model uploads are
//!   refused: another model's vectors would differ.
//! - **Tasks** are done when they're answered: `_register` and `_deploy` answer `COMPLETED`
//!   tasks (a remote model holds no node memory, so deploying is a state).
//! - **Model groups' access modes** are checked when `plugins.ml_commons.model_access_control_enabled`
//!   is set, as OpenSearch checks them; without it they're refused, as OpenSearch refuses them.

pub mod connector;
pub mod ingest;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::routes::Api;
use crate::xpack::security::Principal;
use crate::xpack::store;
use std::time::Duration;

pub use connector::{CONNECTORS, SECRETS};

pub const GROUPS: &str = "os_ml.model_group";
pub const MODELS: &str = "os_ml.model";
pub const TASKS: &str = "os_ml.task";
/// The `_seq_no` counters of the families (in the cluster state: no home).
const SEQ: &str = "os_ml.seq";

/// The pre-registered local model's id, and the name it registers under.
pub const E5_MODEL_ID: &str = ".multilingual-e5-small";
pub const E5_NAME: &str = "intfloat/multilingual-e5-small";

const ACCESS_SETTING: &str = "plugins.ml_commons.model_access_control_enabled";

/// The APIs this module serves.
pub fn implemented(api: Api) -> bool {
    matches!(
        api,
        Api::MlCreateConnector
            | Api::MlGetConnector
            | Api::MlUpdateConnector
            | Api::MlDeleteConnector
            | Api::MlSearchConnectors
            | Api::MlRegisterModelGroup
            | Api::MlGetModelGroup
            | Api::MlUpdateModelGroup
            | Api::MlDeleteModelGroup
            | Api::MlSearchModelGroups
            | Api::MlRegisterModel
            | Api::MlGetModel
            | Api::MlDeleteModel
            | Api::MlSearchModels
            | Api::MlDeployModel
            | Api::MlUndeployModel
            | Api::MlPredict
            | Api::MlGetTask
    )
}

/// Browser keys never get here: they're refused every API but searches and reads
/// ([`crate::xpack::security::browser::check`]).
pub async fn dispatch(ctx: &Ctx<'_>) -> EsResult<Reply> {
    match ctx.api {
        Api::MlCreateConnector => create_connector(ctx).await,
        Api::MlGetConnector => get_connector(ctx),
        Api::MlUpdateConnector => update_connector(ctx).await,
        Api::MlDeleteConnector => delete_connector(ctx).await,
        Api::MlSearchConnectors => search(ctx, CONNECTORS),
        Api::MlRegisterModelGroup => register_group(ctx).await,
        Api::MlGetModelGroup => get_group(ctx),
        Api::MlUpdateModelGroup => update_group(ctx).await,
        Api::MlDeleteModelGroup => delete_group(ctx).await,
        Api::MlSearchModelGroups => search(ctx, GROUPS),
        Api::MlRegisterModel => register_model(ctx).await,
        Api::MlGetModel => get_model(ctx),
        Api::MlDeleteModel => delete_model(ctx).await,
        Api::MlSearchModels => search(ctx, MODELS),
        Api::MlDeployModel => deploy(ctx, true).await,
        Api::MlUndeployModel => deploy(ctx, false).await,
        Api::MlPredict => predict(ctx).await,
        Api::MlGetTask => get_task(ctx),
        other => Err(EsError::not_implemented(other.name())),
    }
}

fn iae(m: impl Into<String>) -> EsError {
    EsError::illegal_argument(m)
}

fn path(ctx: &Ctx, name: &str) -> String {
    ctx.param(name).unwrap_or_default().to_string()
}

/// The index a family's records answer as theirs.
fn index_of(family: &str) -> &'static str {
    store::home_index(family).unwrap_or_default()
}

/// A record as the APIs show it: without roll's own fields (`_roll…`).
fn shown(v: &Value) -> Value {
    match v {
        Value::Obj(m) => Value::Obj(m.iter().filter(|(k, _)| !k.starts_with("_roll")).map(|(k, v)| (k.clone(), v.clone())).collect()),
        other => other.clone(),
    }
}

/// Stores `rec` with its next `_version` and the family's next `_seq_no`: (version, seq_no).
fn put(st: &mut crate::meta::ClusterState, family: &str, id: &str, mut rec: Value, now: i64) -> EsResult<(i64, i64)> {
    let version = st.metadata.xpack.get(family, id).and_then(|r| r.get("_roll_version")).and_then(Value::as_i64).unwrap_or(0) + 1;
    let seq = next_seq(st, family, now)?;
    if let Value::Obj(m) = &mut rec {
        m.insert("_roll_version".into(), version.into());
        m.insert("_roll_seq_no".into(), seq.into());
    }
    store::put(st, family, id, rec, now)?;
    Ok((version, seq))
}

fn next_seq(st: &mut crate::meta::ClusterState, family: &str, now: i64) -> EsResult<i64> {
    let mut counters = st.metadata.xpack.get(SEQ, "seq").and_then(Value::as_object).cloned().unwrap_or_default();
    let seq = counters.get(family).and_then(Value::as_i64).map_or(0, |n| n + 1);
    counters.insert(family.into(), seq.into());
    store::put(st, SEQ, "seq", Value::Obj(counters), now)?;
    Ok(seq)
}

/// What a write answers (an index's `IndexResponse` / `DeleteResponse`).
fn written(family: &str, id: &str, result: &str, (version, seq): (i64, i64)) -> Value {
    obj! {
        "_index" => index_of(family),
        "_id" => id,
        "_version" => version,
        "result" => result,
        "forced_refresh" => true,
        "_shards" => obj! { "total" => 1, "successful" => 1, "failed" => 0 },
        "_seq_no" => seq,
        "_primary_term" => 1,
    }
}

fn connector_missing(id: &str) -> EsError {
    EsError::status(404, format!("Failed to find connector with the provided connector id: {id}"))
}

fn model_missing(id: &str) -> EsError {
    EsError::status(404, format!("Failed to find model with the provided model id: {id}"))
}

fn group_missing(id: &str) -> EsError {
    EsError::status(404, format!("Failed to find model group with the provided model group id: {id}"))
}

// ---- connectors ----

async fn create_connector(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    let (rec, credential) = connector::parse(&body, None, now)?;
    let id = crate::meta::names::time_based_id();
    let secret = connector::seal(ctx.cluster, &credential.unwrap_or_default())?;
    let rid = id.clone();
    ctx.cluster
        .update(move |st| {
            put(st, CONNECTORS, &rid, rec, now)?;
            store::put(st, SECRETS, &rid, secret, now)?;
            Ok(())
        })
        .await?;
    Reply::ok(obj! { "connector_id" => id })
}

fn get_connector(ctx: &Ctx) -> EsResult<Reply> {
    let id = path(ctx, "connector_id");
    let st = ctx.state();
    let rec = st.metadata.xpack.get(CONNECTORS, &id).ok_or_else(|| connector_missing(&id))?;
    Reply::ok(shown(rec))
}

/// The models that call connector `id`.
fn users(st: &crate::meta::ClusterState, id: &str) -> Vec<(String, bool)> {
    st.metadata
        .xpack
        .iter(MODELS)
        .filter(|(_, m)| m.get("connector_id").and_then(Value::as_str) == Some(id))
        .map(|(mid, m)| (mid.clone(), deployed(m)))
        .collect()
}

fn deployed(m: &Value) -> bool {
    matches!(m.get("model_state").and_then(Value::as_str), Some("DEPLOYED" | "DEPLOYING" | "PARTIALLY_DEPLOYED"))
}

async fn update_connector(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "connector_id");
    let body = ctx.body_map()?.unwrap_or_default();
    let now = ctx.now_ms;
    // A sealed credential is made outside the update (sealing needs no state).
    let given = match body.get("credential") {
        Some(Value::Obj(m)) if !m.is_empty() && m.values().all(|v| v.as_str().is_some()) => Some(connector::seal(ctx.cluster, m)?),
        _ => None,
    };
    let rid = id.clone();
    let out = ctx
        .cluster
        .update(move |st| {
            let before = st.metadata.xpack.get(CONNECTORS, &rid).cloned().ok_or_else(|| connector_missing(&rid))?;
            let live: Vec<String> = users(st, &rid).into_iter().filter(|(_, d)| *d).map(|(m, _)| m).collect();
            if !live.is_empty() {
                return Err(EsError::status(400, format!("{} models are still using this connector, please undeploy the models first: [{}]", live.len(), live.join(", "))));
            }
            let (rec, _) = connector::parse(&body, Some(&shown(&before)), now)?;
            let mut rec = rec;
            if let (Value::Obj(m), Some(v)) = (&mut rec, before.get("_roll_version")) {
                m.insert("_roll_version".into(), v.clone());
            }
            let w = put(st, CONNECTORS, &rid, rec, now)?;
            if let Some(s) = given {
                store::put(st, SECRETS, &rid, s, now)?;
            }
            Ok(w)
        })
        .await?;
    Reply::ok(written(CONNECTORS, &id, "updated", out))
}

async fn delete_connector(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "connector_id");
    let now = ctx.now_ms;
    let rid = id.clone();
    let out = ctx
        .cluster
        .update(move |st| {
            let rec = st.metadata.xpack.get(CONNECTORS, &rid).cloned().ok_or_else(|| connector_missing(&rid))?;
            let used: Vec<String> = users(st, &rid).into_iter().map(|(m, _)| m).collect();
            if !used.is_empty() {
                return Err(EsError::status(409, format!("{} models are still using this connector, please delete or update the models first: [{}]", used.len(), used.join(", "))));
            }
            let version = rec.get("_roll_version").and_then(Value::as_i64).unwrap_or(1) + 1;
            let seq = next_seq(st, CONNECTORS, now)?;
            store::remove(st, CONNECTORS, &rid);
            store::remove(st, SECRETS, &rid);
            Ok((version, seq))
        })
        .await?;
    Reply::ok(written(CONNECTORS, &id, "deleted", out))
}

// ---- model groups ----

fn access_control(st: &crate::meta::ClusterState) -> bool {
    st.metadata.cluster_setting(ACCESS_SETTING).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The caller's name and backend roles, and whether it's an admin (the cluster's owner or
/// `all_access`), for model group access.
struct Who {
    name: String,
    backend_roles: Vec<String>,
    admin: bool,
}

fn who(p: &Principal) -> Who {
    let os = p.os();
    Who {
        name: p.username.clone(),
        backend_roles: os.map(|i| i.backend_roles.clone()).unwrap_or_default(),
        admin: os.is_some_and(|i| i.owner) || p.roles.iter().any(|r| r == "all_access"),
    }
}

/// Whether `p` may use or see a model group (`ModelAccessControlHelper`): always without access
/// control; with it, an admin, or by the group's mode: `public` anyone, `private` its owner,
/// `restricted` a caller sharing one of its backend roles.
fn may_use(st: &crate::meta::ClusterState, p: Option<&Principal>, group: &Value) -> bool {
    if !access_control(st) {
        return true;
    }
    let Some(p) = p else { return true };
    let w = who(p);
    if w.admin {
        return true;
    }
    let owner = group.pointer(&["owner", "name"]).and_then(Value::as_str);
    match group.get("access").and_then(Value::as_str) {
        Some("public") | None => true,
        Some("private") => owner == Some(w.name.as_str()),
        Some(_) => {
            let roles: Vec<&str> = group.get("backend_roles").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            owner == Some(w.name.as_str()) || w.backend_roles.iter().any(|r| roles.contains(&r.as_str()))
        }
    }
}

fn no_permission(group: &str) -> EsError {
    EsError::new(403, "status_exception", format!("User doesn't have privilege to perform this operation on this model group: {group}"))
}

/// The access fields of a group's body, checked as `MLModelGroupManager` checks them: (mode,
/// backend roles) to store, `None` without access control.
fn access_fields(st: &crate::meta::ClusterState, body: &Map, p: &Principal, creating: bool) -> EsResult<Option<(String, Vec<String>)>> {
    let mode = body.get("access_mode").map(Value::to_es_string).map(|s| s.to_ascii_lowercase());
    let roles: Option<Vec<String>> = body.get("backend_roles").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect());
    let all = body.get("add_all_backend_roles").and_then(Value::as_bool);
    if !access_control(st) {
        if mode.is_some() || roles.is_some() || all.is_some() {
            return Err(iae("You cannot specify model access control parameters because the Security plugin or model access control is disabled on your cluster."));
        }
        return Ok(None);
    }
    if !creating && mode.is_none() && roles.is_none() && all.is_none() {
        return Ok(None);
    }
    if let Some(m) = &mode
        && !matches!(m.as_str(), "public" | "private" | "restricted")
    {
        return Err(iae(format!("Invalid access mode: {m}")));
    }
    if roles.as_ref().is_some_and(|r| !r.is_empty()) && all == Some(true) {
        return Err(iae("You cannot specify backend roles and add all backend roles at the same time."));
    }
    let given_roles = roles.as_ref().is_some_and(|r| !r.is_empty()) || all == Some(true);
    let mode = match mode {
        Some(m) => m,
        None if given_roles => "restricted".into(),
        // The stricter default where OpenSearch's docs leave it to the caller.
        None => "private".into(),
    };
    if mode != "restricted" && given_roles {
        return Err(iae("You can specify backend roles only for a model group with the restricted access mode."));
    }
    let w = who(p);
    let roles = if all == Some(true) { w.backend_roles.clone() } else { roles.unwrap_or_default() };
    if mode == "restricted" {
        if roles.is_empty() {
            return Err(iae("You must specify one or more backend roles or add all backend roles to register a restricted model group."));
        }
        if !w.admin && !roles.iter().all(|r| w.backend_roles.contains(r)) {
            return Err(iae("You don't have the backend roles specified."));
        }
    }
    Ok(Some((mode, roles)))
}

fn group_named<'a>(st: &'a crate::meta::ClusterState, name: &str) -> Option<(&'a String, &'a Value)> {
    st.metadata.xpack.iter(GROUPS).find(|(_, g)| g.get("name").and_then(Value::as_str) == Some(name))
}

/// A new group's record.
fn group_record(name: &str, description: Option<&Value>, access: Option<(String, Vec<String>)>, p: &Principal, now: i64) -> Value {
    let mut g = Map::new();
    g.insert("name".into(), name.into());
    g.insert("latest_version".into(), 0.into());
    if let Some(d) = description {
        g.insert("description".into(), d.clone());
    }
    match access {
        Some((mode, roles)) => {
            if mode == "restricted" {
                g.insert("backend_roles".into(), Value::Arr(roles.into_iter().map(Value::from).collect()));
            }
            let w = who(p);
            g.insert("owner".into(), obj! { "name" => w.name, "backend_roles" => Value::Arr(w.backend_roles.into_iter().map(Value::from).collect()), "roles" => Value::Arr(p.roles.iter().map(|r| Value::from(r.as_str())).collect()) });
            g.insert("access".into(), mode.into());
        }
        None => {
            g.insert("access".into(), "public".into());
        }
    }
    g.insert("created_time".into(), now.into());
    g.insert("last_updated_time".into(), now.into());
    Value::Obj(g)
}

async fn register_group(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let name = body.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).ok_or_else(|| iae("Model group name is required"))?.to_string();
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    let id = crate::meta::names::time_based_id();
    let rid = id.clone();
    ctx.cluster
        .update(move |st| {
            if let Some((gid, _)) = group_named(st, &name) {
                return Err(iae(format!("The name you provided is already being used by a model group with ID: {gid}.")));
            }
            let access = access_fields(st, &body, &p, true)?;
            put(st, GROUPS, &rid, group_record(&name, body.get("description"), access, &p, now), now)?;
            Ok(())
        })
        .await?;
    Reply::ok(obj! { "model_group_id" => id, "status" => "CREATED" })
}

fn get_group(ctx: &Ctx) -> EsResult<Reply> {
    let id = path(ctx, "model_group_id");
    let st = ctx.state();
    let g = st.metadata.xpack.get(GROUPS, &id).ok_or_else(|| group_missing(&id))?;
    if !may_use(&st, Some(crate::xpack::security::principal(ctx)), g) {
        return Err(no_permission(&id));
    }
    Reply::ok(shown(g))
}

async fn update_group(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "model_group_id");
    let body = ctx.body_map()?.unwrap_or_default();
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let mut g = st.metadata.xpack.get(GROUPS, &id).cloned().ok_or_else(|| group_missing(&id))?;
            if !may_use(st, Some(&p), &g) {
                return Err(no_permission(&id));
            }
            let access = access_fields(st, &body, &p, false)?;
            if let Some(n) = body.get("name").and_then(Value::as_str)
                && let Some((other, _)) = group_named(st, n).filter(|(gid, _)| **gid != id)
            {
                return Err(iae(format!("The name you provided is already being used by a model group with ID: {other}.")));
            }
            if let Value::Obj(m) = &mut g {
                for k in ["name", "description"] {
                    if let Some(v) = body.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
                if let Some((mode, roles)) = access {
                    m.shift_remove("backend_roles");
                    if mode == "restricted" {
                        m.insert("backend_roles".into(), Value::Arr(roles.into_iter().map(Value::from).collect()));
                    }
                    m.insert("access".into(), mode.into());
                }
                m.insert("last_updated_time".into(), now.into());
            }
            put(st, GROUPS, &id, g, now).map(drop)
        })
        .await?;
    Reply::ok(obj! { "status" => "Updated" })
}

async fn delete_group(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "model_group_id");
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    let rid = id.clone();
    let out = ctx
        .cluster
        .update(move |st| {
            let g = st.metadata.xpack.get(GROUPS, &rid).cloned().ok_or_else(|| group_missing(&rid))?;
            if !may_use(st, Some(&p), &g) {
                return Err(no_permission(&rid));
            }
            if st.metadata.xpack.iter(MODELS).any(|(_, m)| m.get("model_group_id").and_then(Value::as_str) == Some(rid.as_str())) {
                return Err(EsError::status(409, "Cannot delete the model group when it has associated model versions"));
            }
            let version = g.get("_roll_version").and_then(Value::as_i64).unwrap_or(1) + 1;
            let seq = next_seq(st, GROUPS, now)?;
            store::remove(st, GROUPS, &rid);
            Ok((version, seq))
        })
        .await?;
    Reply::ok(written(GROUPS, &id, "deleted", out))
}

// ---- models ----

/// The pre-registered E5 model as `GET` shows it.
fn e5_record() -> Value {
    obj! {
        "name" => E5_NAME,
        "algorithm" => "TEXT_EMBEDDING",
        "model_version" => "1",
        "model_format" => "ONNX",
        "model_state" => "DEPLOYED",
        "model_config" => obj! { "model_type" => "bert", "embedding_dimension" => crate::xpack::inference::services::builtin::E5_DIMS, "framework_type" => "sentence_transformers" },
        "is_hidden" => false,
        "_roll_local" => "e5",
    }
}

/// A model's record, the pre-registered one included.
pub fn model(st: &crate::meta::ClusterState, id: &str) -> Option<Value> {
    if id == E5_MODEL_ID {
        return Some(e5_record());
    }
    st.metadata.xpack.get(MODELS, id).cloned()
}

fn task_record(model_id: &str, ty: &str, function: &str, node: &str, asynchronous: bool, now: i64) -> Value {
    obj! {
        "model_id" => model_id,
        "task_type" => ty,
        "function_name" => function,
        "state" => "COMPLETED",
        "worker_node" => Value::Arr(vec![node.into()]),
        "create_time" => now,
        "last_update_time" => now,
        "is_async" => asynchronous,
    }
}

/// What registering the body makes: the record, the inline connector's credential, and the
/// function name its task shows.
fn model_of(st: &crate::meta::ClusterState, body: &Map, now: i64) -> EsResult<(Map, Option<Map>, &'static str)> {
    let name = body.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).ok_or_else(|| iae("model name is null"))?;
    let function = body.get("function_name").map(Value::to_es_string).unwrap_or_default().to_ascii_uppercase();
    let mut m = Map::new();
    m.insert("name".into(), name.into());
    if function == "REMOTE" {
        m.insert("algorithm".into(), "REMOTE".into());
        let mut credential = None;
        match (body.get("connector_id").and_then(Value::as_str), body.get("connector")) {
            (Some(cid), _) => {
                if st.metadata.xpack.get(CONNECTORS, cid).is_none() {
                    return Err(connector_missing(cid));
                }
                m.insert("connector_id".into(), cid.into());
            }
            (None, Some(Value::Obj(inline))) => {
                let (rec, cred) = connector::parse(inline, None, now)?;
                credential = cred;
                m.insert("connector".into(), rec);
            }
            _ => return Err(iae("Must provide connector_id or connector when registering a remote model")),
        }
        if let Some(d) = body.get("description") {
            m.insert("description".into(), d.clone());
        }
        return Ok((m, credential, "REMOTE"));
    }
    // A local model: the upload fields first (roll never runs a tenant's model file).
    if ["url", "model_content_hash_value", "model_config", "model_content"].iter().any(|k| body.contains_key(*k)) {
        return Err(iae("roll doesn't run uploaded models (TorchScript or ONNX files); register a remote model through a connector, or roll's local E5 model"));
    }
    if name != E5_NAME && name != E5_MODEL_ID {
        return Err(iae(format!(
            "roll runs one local model, [{E5_NAME}] (pre-registered as [{E5_MODEL_ID}]); [{name}] isn't run, and no other model stands in for it: its vectors would differ"
        )));
    }
    m.insert("algorithm".into(), "TEXT_EMBEDDING".into());
    m.insert("model_format".into(), "ONNX".into());
    m.insert("_roll_local".into(), "e5".into());
    Ok((m, None, "TEXT_EMBEDDING"))
}

async fn register_model(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let deploy_now = ctx.bool_param("deploy", false)?;
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    let node = ctx.cluster.node.id.clone();
    // The inline connector's credential is sealed before the update.
    let inline_credential = match body.get("connector").and_then(|c| c.get("credential")) {
        Some(Value::Obj(m)) if !m.is_empty() && m.values().all(|v| v.as_str().is_some()) => Some(connector::seal(ctx.cluster, m)?),
        _ => None,
    };
    let (model_id, task_id) = (crate::meta::names::time_based_id(), crate::meta::names::time_based_id());
    let (mid, tid) = (model_id.clone(), task_id.clone());
    ctx.cluster
        .update(move |st| {
            let (mut m, _, function) = model_of(st, &body, now)?;
            // The group: the one named, or one named after the model (made when missing).
            let gid = match body.get("model_group_id").and_then(Value::as_str) {
                Some(g) => {
                    let group = st.metadata.xpack.get(GROUPS, g).ok_or_else(|| group_missing(g))?;
                    if !may_use(st, Some(&p), group) {
                        return Err(no_permission(g));
                    }
                    g.to_string()
                }
                None => {
                    let name = m.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                    match group_named(st, &name) {
                        Some((g, group)) if may_use(st, Some(&p), group) => g.clone(),
                        Some((g, _)) => return Err(no_permission(g)),
                        None => {
                            let g = crate::meta::names::time_based_id();
                            let access = access_control(st).then(|| ("private".to_string(), Vec::new()));
                            put(st, GROUPS, &g, group_record(&name, None, access, &p, now), now)?;
                            g
                        }
                    }
                }
            };
            let mut group = st.metadata.xpack.get(GROUPS, &gid).cloned().unwrap_or_default();
            let version = group.get("latest_version").and_then(Value::as_i64).unwrap_or(0) + 1;
            if let Value::Obj(g) = &mut group {
                g.insert("latest_version".into(), version.into());
                g.insert("last_updated_time".into(), now.into());
            }
            put(st, GROUPS, &gid, group, now)?;
            let mut rec = Map::new();
            rec.insert("name".into(), m.shift_remove("name").unwrap_or_default());
            rec.insert("model_group_id".into(), gid.into());
            rec.insert("algorithm".into(), m.shift_remove("algorithm").unwrap_or_default());
            rec.insert("model_version".into(), version.to_string().into());
            if let Some(d) = m.shift_remove("description") {
                rec.insert("description".into(), d);
            }
            if let Some(f) = m.shift_remove("model_format") {
                rec.insert("model_format".into(), f);
            }
            rec.insert("model_state".into(), "REGISTERED".into());
            rec.insert("created_time".into(), now.into());
            rec.insert("last_updated_time".into(), now.into());
            rec.insert("is_hidden".into(), false.into());
            rec.extend(m);
            let mut rec = Value::Obj(rec);
            if deploy_now {
                rec = deployed_record(rec, &node, now);
            }
            put(st, MODELS, &mid, rec, now)?;
            if let Some(s) = inline_credential {
                store::put(st, SECRETS, &format!("model:{mid}"), s, now)?;
            }
            put(st, TASKS, &tid, task_record(&mid, "REGISTER_MODEL", function, &node, false, now), now)?;
            if deploy_now {
                let dt = crate::meta::names::time_based_id();
                put(st, TASKS, &dt, task_record(&mid, "DEPLOY_MODEL", function, &node, true, now), now)?;
            }
            Ok(())
        })
        .await?;
    Reply::ok(obj! { "task_id" => task_id, "status" => "CREATED", "model_id" => model_id })
}

/// The model in the `DEPLOYED` state, as `_deploy` leaves it.
fn deployed_record(mut rec: Value, node: &str, now: i64) -> Value {
    if let Value::Obj(m) = &mut rec {
        for k in ["planning_worker_node_count", "current_worker_node_count", "planning_worker_nodes"] {
            m.shift_remove(k);
        }
        m.insert("model_state".into(), "DEPLOYED".into());
        m.insert("last_updated_time".into(), now.into());
        m.insert("last_deployed_time".into(), now.into());
        m.insert("auto_redeploy_retry_times".into(), 0.into());
        m.insert("planning_worker_node_count".into(), 1.into());
        m.insert("current_worker_node_count".into(), 1.into());
        m.insert("planning_worker_nodes".into(), Value::Arr(vec![node.into()]));
        m.insert("deploy_to_all_nodes".into(), true.into());
    }
    rec
}

fn undeployed_record(mut rec: Value, now: i64) -> Value {
    if let Value::Obj(m) = &mut rec {
        m.insert("model_state".into(), "UNDEPLOYED".into());
        m.insert("last_updated_time".into(), now.into());
        m.insert("last_deployed_time".into(), now.into());
        m.insert("planning_worker_node_count".into(), 0.into());
        m.insert("current_worker_node_count".into(), 0.into());
        m.shift_remove("planning_worker_nodes");
    }
    rec
}

/// The model `id` if `p` may use it (its group's access mode).
fn usable(st: &crate::meta::ClusterState, id: &str, p: Option<&Principal>) -> EsResult<Value> {
    let m = model(st, id).ok_or_else(|| model_missing(id))?;
    if let Some(g) = m.get("model_group_id").and_then(Value::as_str)
        && let Some(group) = st.metadata.xpack.get(GROUPS, g)
        && !may_use(st, p, group)
    {
        return Err(no_permission(g));
    }
    Ok(m)
}

fn get_model(ctx: &Ctx) -> EsResult<Reply> {
    let id = path(ctx, "model_id");
    let st = ctx.state();
    Reply::ok(shown(&usable(&st, &id, Some(crate::xpack::security::principal(ctx)))?))
}

async fn delete_model(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "model_id");
    if id == E5_MODEL_ID {
        return Err(iae(format!("[{E5_MODEL_ID}] is roll's built-in model; it can't be deleted")));
    }
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    let rid = id.clone();
    let out = ctx
        .cluster
        .update(move |st| {
            let m = usable(st, &rid, Some(&p))?;
            if deployed(&m) {
                return Err(EsError::status(400, "Model cannot be deleted in deploying or deployed state. Try undeploy model first then delete"));
            }
            let version = m.get("_roll_version").and_then(Value::as_i64).unwrap_or(1) + 1;
            let seq = next_seq(st, MODELS, now)?;
            store::remove(st, MODELS, &rid);
            store::remove(st, SECRETS, &format!("model:{rid}"));
            Ok((version, seq))
        })
        .await?;
    Reply::ok(written(MODELS, &id, "deleted", out))
}

async fn deploy(ctx: &Ctx<'_>, on: bool) -> EsResult<Reply> {
    let id = path(ctx, "model_id");
    let p = crate::xpack::security::principal(ctx).clone();
    let now = ctx.now_ms;
    let node = ctx.cluster.node.id.clone();
    let task_id = crate::meta::names::time_based_id();
    let (rid, tid, n) = (id.clone(), task_id.clone(), node.clone());
    if id != E5_MODEL_ID {
        ctx.cluster
            .update(move |st| {
                let m = usable(st, &rid, Some(&p))?;
                let function = if m.get("algorithm").and_then(Value::as_str) == Some("REMOTE") { "REMOTE" } else { "TEXT_EMBEDDING" };
                let next = if on { deployed_record(m, &n, now) } else { undeployed_record(m, now) };
                put(st, MODELS, &rid, next, now)?;
                if on {
                    put(st, TASKS, &tid, task_record(&rid, "DEPLOY_MODEL", function, &n, true, now), now)?;
                }
                Ok(())
            })
            .await?;
    } else {
        usable(&ctx.state(), &id, Some(&p))?;
    }
    if on {
        return Reply::ok(obj! { "task_id" => task_id, "task_type" => "DEPLOY_MODEL", "status" => "COMPLETED" });
    }
    let mut stats = Map::new();
    stats.insert(id, "undeployed".into());
    let mut out = Map::new();
    out.insert(node, obj! { "stats" => Value::Obj(stats) });
    Reply::ok(Value::Obj(out))
}

/// How a model is run.
enum Runner {
    Remote(connector::Connector),
    E5,
}

fn runner(st: &crate::meta::ClusterState, id: &str, m: &Value) -> EsResult<Runner> {
    if m.get("_roll_local").is_some() {
        return Ok(Runner::E5);
    }
    if let Some(inline) = m.get("connector") {
        return connector::from_record(inline, &format!("model:{id}")).map(Runner::Remote);
    }
    let cid = m.get("connector_id").and_then(Value::as_str).unwrap_or_default();
    let rec = st.metadata.xpack.get(CONNECTORS, cid).ok_or_else(|| connector_missing(cid))?;
    connector::from_record(rec, cid).map(Runner::Remote)
}

/// A remote model that isn't deployed is deployed by its first predict (`auto_deploy`), as
/// OpenSearch does.
async fn ensure_deployed(cluster: &Cluster, id: &str, m: &Value, now: i64) -> EsResult<()> {
    if deployed(m) || id == E5_MODEL_ID {
        return Ok(());
    }
    let (rid, node) = (id.to_string(), cluster.node.id.clone());
    cluster
        .update(move |st| {
            let Some(m) = st.metadata.xpack.get(MODELS, &rid).cloned() else { return Ok(()) };
            if !deployed(&m) {
                put(st, MODELS, &rid, deployed_record(m, &node, now), now)?;
            }
            Ok(())
        })
        .await
}

/// Embeds `texts` with model `id` as `p` (ingest's `text_embedding`, `neural`): `search` says
/// whether they're queries (E5's query prefix) or documents.
pub async fn embed(cluster: &Cluster, id: &str, texts: &[String], p: Option<&Principal>, search: bool) -> EsResult<Vec<Vec<f32>>> {
    let st = cluster.state();
    let m = usable(&st, id, p)?;
    let run = runner(&st, id, &m)?;
    drop(st);
    let timeout = Duration::from_secs(30);
    let vectors = match run {
        Runner::E5 => {
            let ep = crate::xpack::inference::services::builtin::default_endpoint(crate::xpack::inference::services::builtin::E5_ENDPOINT).ok_or_else(|| EsError::illegal_state("E5 isn't built in"))?;
            let mut req = crate::xpack::inference::Request::new(texts.to_vec(), timeout);
            req.input_type = Some(if search { "internal_search" } else { "internal_ingest" }.into());
            match crate::xpack::inference::services::builtin::infer(cluster, &ep, &req).await? {
                crate::xpack::inference::results::Results::Floats(v) => v,
                _ => return Err(EsError::illegal_state("E5 answered no embeddings")),
            }
        }
        Runner::Remote(c) => {
            ensure_deployed(cluster, id, &m, crate::time::now_millis()).await?;
            let answers = connector::predict(cluster, id, &c, connector::Input::Texts(texts), timeout).await?;
            let mut out = Vec::new();
            for a in answers {
                out.extend(a.embeddings.ok_or_else(|| iae(format!("model [{id}]'s connector has no embedding post_process_function, so its answer holds no embeddings")))?);
            }
            out
        }
    };
    if vectors.len() != texts.len() {
        return Err(EsError::illegal_state(format!("model [{id}] answered [{}] embeddings for [{}] texts", vectors.len(), texts.len())));
    }
    Ok(vectors)
}

/// A vector's element as Java prints a `float` (`0.1`, not `0.10000000149011612`).
pub fn float_value(f: f32) -> Value {
    Value::from(crate::json::write::java_float_value(f))
}

fn tensor(v: &[f32]) -> Value {
    obj! {
        "name" => "sentence_embedding",
        "data_type" => "FLOAT32",
        "shape" => Value::Arr(vec![(v.len() as i64).into()]),
        "data" => Value::Arr(v.iter().map(|x| float_value(*x)).collect()),
    }
}

async fn predict(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = path(ctx, "model_id");
    let body = ctx.body_map()?.unwrap_or_default();
    let p = crate::xpack::security::principal(ctx);
    let st = ctx.state();
    let m = usable(&st, &id, Some(p))?;
    let run = runner(&st, &id, &m)?;
    drop(st);
    let texts: Option<Vec<String>> = body.get("text_docs").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect());
    let outputs: Vec<Value> = match run {
        Runner::E5 => {
            let texts = texts.ok_or_else(|| iae("[text_docs] is required for a text embedding model"))?;
            embed(ctx.cluster, &id, &texts, Some(p), false).await?.iter().map(|v| obj! { "output" => Value::Arr(vec![tensor(v)]) }).collect()
        }
        Runner::Remote(c) => {
            ensure_deployed(ctx.cluster, &id, &m, ctx.now_ms).await?;
            // A remote model reads `parameters`; `text_docs` alone fills none of them.
            let params = body.get("parameters").and_then(Value::as_object).cloned().unwrap_or_default();
            let answers = connector::predict(ctx.cluster, &id, &c, connector::Input::Parameters(&params), Duration::from_secs(30)).await?;
            answers
                .into_iter()
                .map(|a| {
                    let output = match a.embeddings {
                        Some(vs) => vs.iter().map(|v| tensor(v)).collect(),
                        None => vec![obj! { "name" => "response", "dataAsMap" => a.raw }],
                    };
                    obj! { "output" => Value::Arr(output), "status_code" => 200 }
                })
                .collect()
        }
    };
    Reply::ok(obj! { "inference_results" => Value::Arr(outputs) })
}

fn get_task(ctx: &Ctx) -> EsResult<Reply> {
    let id = path(ctx, "task_id");
    let st = ctx.state();
    let t = st.metadata.xpack.get(TASKS, &id).ok_or_else(|| EsError::status(404, "Failed to find task"))?;
    Reply::ok(shown(t))
}

// ---- _search ----

/// `_search` over a family's records: the query, `from`, `size` and `sort` the security query
/// APIs evaluate ([`crate::xpack::security::query`]), answered as a search of its index.
/// Fields may be named with `.keyword`, as over OpenSearch's mapping.
fn search(ctx: &Ctx, family: &str) -> EsResult<Reply> {
    let started = std::time::Instant::now();
    let mut body = ctx.body_map()?.unwrap_or_default();
    for k in ["_source", "track_total_hits", "seq_no_primary_term", "version", "timeout", "explain"] {
        body.shift_remove(k);
    }
    use crate::xpack::security::query;
    let req = query::QueryRequest::parse(Some(&Value::Obj(body)), "search")?;
    let st = ctx.state();
    let p = crate::xpack::security::principal(ctx);
    let group_ok = |gid: Option<&str>| gid.and_then(|g| st.metadata.xpack.get(GROUPS, g)).is_none_or(|g| may_use(&st, Some(p), g));
    let mut objects: Vec<(String, Value)> = Vec::new();
    for (id, rec) in st.metadata.xpack.iter(family) {
        let visible = match family {
            GROUPS => may_use(&st, Some(p), rec),
            MODELS => group_ok(rec.get("model_group_id").and_then(Value::as_str)),
            _ => true,
        };
        if visible {
            let mut r = rec.clone();
            if let Value::Obj(m) = &mut r {
                m.insert("_roll_id".into(), id.clone().into());
            }
            objects.push((id.clone(), r));
        }
    }
    let fields = |o: &Value, f: &str| query::path_values(o, f.strip_suffix(".keyword").unwrap_or(f));
    let (total, page) = query::run(&req, objects, &fields)?;
    let hits: Vec<Value> = page
        .into_iter()
        .map(|(o, _)| {
            obj! {
                "_index" => index_of(family),
                "_id" => o.get("_roll_id").cloned().unwrap_or_default(),
                "_version" => o.get("_roll_version").cloned().unwrap_or(Value::from(1)),
                "_seq_no" => o.get("_roll_seq_no").cloned().unwrap_or(Value::from(0)),
                "_primary_term" => 1,
                "_score" => 1.0,
                "_source" => shown(&o),
            }
        })
        .collect();
    let max = if hits.is_empty() { Value::Null } else { Value::from(1.0) };
    Reply::ok(obj! {
        "took" => started.elapsed().as_millis() as i64,
        "timed_out" => false,
        "_shards" => obj! { "total" => 1, "successful" => 1, "skipped" => 0, "failed" => 0 },
        "hits" => obj! { "total" => obj! { "value" => total, "relation" => "eq" }, "max_score" => max, "hits" => Value::Arr(hits) },
    })
}
