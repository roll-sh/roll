//! Trained models: configurations, definition parts and vocabularies, model aliases,
//! deployments, and inference.
//!
//! Tree-ensemble models (`definition.trained_model.tree` / `ensemble`) are evaluated for real:
//! `_infer` walks the trees over the documents' fields. Deployments of PyTorch models move
//! through Elasticsearch's states and count inferences and cache hits per input, but roll has no
//! PyTorch runtime, so their inference results are placeholders of the configured task's shape.

use super::*;
use crate::xpack::store;
use std::collections::HashMap;

fn no_model(id: &str) -> EsError {
    EsError::resource_not_found(format!("Could not find trained model [{id}]"))
}

/// Deployments of this cluster. Their processes (counters, cache) live in memory; their
/// assignments are cluster state on Elasticsearch, which restarts them after a node restart. A
/// durable server persists the assignments the same way ([`ASSIGNMENTS`], through
/// `Cluster::sync_ml_deployments` after a request that changed them, see
/// [`Deployments::take_changed`]) and restores them at wake ([`Deployments::restore`]).
#[derive(Default)]
pub struct Deployments {
    inner: parking_lot::Mutex<HashMap<String, Deployment>>,
    /// An assignment was started, updated or stopped since the last [`Deployments::take_changed`].
    changed: std::sync::atomic::AtomicBool,
}

/// The cluster-state family of persisted deployment assignments (Elasticsearch's
/// `trained_model_assignment` metadata), by deployment id: `{model_id, params, start_ms}`.
pub const ASSIGNMENTS: &str = "trained_model_assignment";

/// Most cached inputs per deployment an unload keeps.
pub const MAX_PERSISTED_CACHE: usize = 10_000;

impl Deployments {
    fn note(&self) {
        self.changed.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether assignments changed since the last call (one atomic swap); the caller persists
    /// [`Deployments::assignments`] and calls [`Deployments::note_changed`] if that fails.
    pub fn take_changed(&self) -> bool {
        self.changed.load(std::sync::atomic::Ordering::Acquire) && self.changed.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    pub fn note_changed(&self) {
        self.note();
    }

    /// Stops every deployment (`_features/_reset` of the machine learning feature).
    pub fn clear(&self) {
        let mut g = self.inner.lock();
        if !g.is_empty() {
            g.clear();
            self.note();
        }
    }

    /// Whether any deployment runs.
    pub fn has_any(&self) -> bool {
        !self.inner.lock().is_empty()
    }

    /// Every assignment in its persisted form, by deployment id; `with_process` adds what the
    /// running process counted (inference and cache-hit counts, last access, the inputs its
    /// cache holds, at most [`MAX_PERSISTED_CACHE`]), which a server unloading the cluster keeps
    /// so a wake doesn't look like a restart.
    pub fn assignments(&self, with_process: bool) -> Vec<(String, Value)> {
        let g = self.inner.lock();
        let mut v: Vec<(String, Value)> = g
            .iter()
            .map(|(id, d)| {
                let mut a = obj! { "model_id" => d.model_id.clone(), "params" => Value::Obj(d.params.clone()), "start_ms" => d.start_ms };
                if with_process && let Value::Obj(m) = &mut a {
                    m.insert("inference_count".into(), d.inference_count.into());
                    m.insert("cache_hits".into(), d.cache_hits.into());
                    m.insert("last_access".into(), d.last_access.into());
                    let mut seen: Vec<&String> = d.seen.iter().collect();
                    seen.sort();
                    seen.truncate(MAX_PERSISTED_CACHE);
                    m.insert("seen".into(), Value::Arr(seen.into_iter().map(|s| Value::from(s.as_str())).collect()));
                }
                (id.clone(), a)
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Starts the persisted assignments again (a wake), with what their processes counted when
    /// that was kept too ([`Deployments::assignments`]). Returns how many.
    pub fn restore<'a>(&self, assignments: impl Iterator<Item = (&'a String, &'a Value)>) -> usize {
        let mut g = self.inner.lock();
        let mut n = 0;
        for (id, a) in assignments {
            let (Some(model_id), Some(Value::Obj(params))) = (a.get("model_id").and_then(Value::as_str), a.get("params")) else { continue };
            let num = |k: &str| a.get(k).and_then(Value::as_i64).unwrap_or(0);
            let seen = a.get("seen").and_then(Value::as_array).map(|s| s.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            g.insert(
                id.clone(),
                Deployment {
                    model_id: model_id.to_string(),
                    params: params.clone(),
                    start_ms: num("start_ms"),
                    inference_count: num("inference_count"),
                    cache_hits: num("cache_hits"),
                    seen,
                    last_access: num("last_access"),
                },
            );
            n += 1;
        }
        n
    }
}

#[derive(Clone, Debug)]
struct Deployment {
    model_id: String,
    params: Map,
    start_ms: i64,
    inference_count: i64,
    cache_hits: i64,
    seen: std::collections::HashSet<String>,
    last_access: i64,
}

fn deployments<'a>(ctx: &Ctx<'a>) -> &'a Deployments {
    &ctx.cluster.xpack.deployments
}

/// Whether `id` names a trained model (or alias) or a deployment (`text_embedding` query
/// vector builders check it before anything runs).
pub(crate) fn model_exists(ctx: &Ctx, id: &str) -> bool {
    resolve(&ctx.state(), id).is_some() || deployments(ctx).inner.lock().contains_key(id)
}

/// Resolves a model id or alias.
fn resolve(st: &crate::meta::ClusterState, id: &str) -> Option<String> {
    if st.metadata.xpack.get(MODELS, id).is_some() {
        return Some(id.to_string());
    }
    st.metadata.xpack.get(ALIASES, id).and_then(|v| v.get("model_id")).map(Value::to_es_string)
}

fn default_inference_config(cfg: &Value) -> Value {
    let Some(m) = cfg.as_object() else { return cfg.clone() };
    let mut out = Map::new();
    for (task, body) in m {
        let mut b = body.as_object().cloned().unwrap_or_default();
        match task.as_str() {
            "regression" => {
                b.entry("results_field".to_string()).or_insert_with(|| Value::from("predicted_value"));
                b.entry("num_top_feature_importance_values".to_string()).or_insert_with(|| Value::from(0));
            }
            "classification" => {
                b.entry("num_top_classes".to_string()).or_insert_with(|| Value::from(0));
                b.entry("top_classes_results_field".to_string()).or_insert_with(|| Value::from("top_classes"));
                b.entry("results_field".to_string()).or_insert_with(|| Value::from("predicted_value"));
                b.entry("num_top_feature_importance_values".to_string()).or_insert_with(|| Value::from(0));
                b.entry("prediction_field_type".to_string()).or_insert_with(|| Value::from("string"));
            }
            _ => {
                let mut nb = Map::new();
                nb.insert("vocabulary".into(), obj! { "index" => ".ml-inference-native-000002" });
                if let Some(Value::Obj(tok)) = b.get("tokenization") {
                    let mut t = Map::new();
                    for (kind, tb) in tok {
                        let mut tb = tb.as_object().cloned().unwrap_or_default();
                        tb.entry("do_lower_case".to_string()).or_insert(Value::Bool(false));
                        tb.entry("with_special_tokens".to_string()).or_insert(Value::Bool(true));
                        tb.entry("max_sequence_length".to_string()).or_insert(Value::from(512));
                        tb.entry("truncate".to_string()).or_insert(Value::from("first"));
                        tb.entry("span".to_string()).or_insert(Value::from(-1));
                        t.insert(kind.clone(), Value::Obj(tb));
                    }
                    nb.insert("tokenization".into(), Value::Obj(t));
                }
                for (k, v) in &b {
                    if k != "tokenization" && k != "vocabulary" {
                        nb.insert(k.clone(), v.clone());
                    }
                }
                b = nb;
            }
        }
        out.insert(task.clone(), Value::Obj(b));
    }
    Value::Obj(out)
}

fn tree_size(def: &Value) -> (i64, i64) {
    let json = def.to_json();
    let nodes = json.matches("node_index").count() as i64;
    (json.len() as i64, (nodes * 2).max(1))
}

pub async fn put_model(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(invalid_id("model_id", &id));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(cfg) = body.get("inference_config") else {
        return Err(EsError::action_request_validation(&["[inference_config] must not be null.".into()]));
    };
    let pytorch = body.get("model_type").and_then(Value::as_str) == Some("pytorch");
    let definition = body.get("definition").cloned();
    if !pytorch && definition.is_none() && body.get("compressed_definition").is_none() {
        return Err(EsError::action_request_validation(&["[definition] must not be null.".into()]));
    }
    let (size, ops) = definition.as_ref().map(tree_size).unwrap_or((0, 0));
    let mut m = Map::new();
    m.insert("model_id".into(), id.clone().into());
    if pytorch {
        m.insert("model_type".into(), "pytorch".into());
    }
    m.insert("created_by".into(), "api_user".into());
    m.insert("version".into(), CONFIG_VERSION.into());
    m.insert("create_time".into(), ctx.now_ms.into());
    m.insert("model_size_bytes".into(), size.into());
    m.insert("estimated_operations".into(), ops.into());
    m.insert("license_level".into(), "platinum".into());
    if let Some(d) = body.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert("tags".into(), body.get("tags").cloned().unwrap_or_else(|| Value::Arr(Vec::new())));
    if let Some(md) = body.get("metadata") {
        m.insert("metadata".into(), md.clone());
    }
    m.insert("input".into(), body.get("input").cloned().unwrap_or_else(|| obj! { "field_names" => crate::arr!["input"] }));
    m.insert("inference_config".into(), default_inference_config(cfg));
    if pytorch {
        m.insert("location".into(), obj! { "index" => obj! { "name" => ".ml-inference-native-000002" } });
    }
    let mut stored = m.clone();
    stored.insert("_roll".into(), obj! { "definition" => definition.unwrap_or(Value::Null), "parts" => 0, "total_length" => 0 });
    let out = Value::Obj(m);
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(MODELS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("Trained machine learning model [{id}] already exists")));
            }
            store::put(st, MODELS, &id, Value::Obj(stored), now).map(|_| ())
        })
        .await?;
    Reply::ok(out)
}

fn model_view(m: &Value, include_definition: bool) -> Value {
    let mut v = public(m);
    if let Value::Obj(o) = &mut v {
        // `model_type` prints after `model_id` (tree ensembles are typed on read).
        if !o.contains_key("model_type") {
            let mut n = Map::new();
            for (k, x) in o.iter() {
                n.insert(k.clone(), x.clone());
                if k == "model_id" {
                    n.insert("model_type".into(), "tree_ensemble".into());
                }
            }
            *o = n;
        }
        if include_definition && let Some(d) = roll_state(m).get("definition").filter(|d| !d.is_null()) {
            o.insert("definition".into(), d.clone());
        }
    }
    v
}

pub fn get_models(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let allow = ctx.bool_param("allow_no_match", true)?;
    let include = ctx.query("include").unwrap_or_default();
    let mut models = expand(&st, MODELS, ctx.path_param("model_id").map(|e| resolve(&st, e).map(|_| e).unwrap_or(e)), allow, no_model);
    if let (Err(_), Some(e)) = (&models, ctx.path_param("model_id"))
        && let Some(real) = resolve(&st, e)
    {
        models = expand(&st, MODELS, Some(&real), allow, no_model);
    }
    let models = models?;
    let from = ctx.query("from").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    let size = ctx.query("size").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100);
    let count = models.len();
    let list: Vec<Value> = models.iter().skip(from).take(size).map(|(_, m)| model_view(m, include.contains("definition"))).collect();
    Reply::ok(obj! { "count" => count, "trained_model_configs" => Value::Arr(list) })
}

pub fn model_stats(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let models = expand(&st, MODELS, ctx.path_param("model_id"), true, no_model)?;
    let deps = deployments(ctx).inner.lock();
    let mut out = Vec::new();
    for (id, m) in models {
        let rs = roll_state(&m);
        let size = rs.get("total_length").and_then(Value::as_i64).filter(|l| *l > 0).unwrap_or_else(|| m.get("model_size_bytes").and_then(Value::as_i64).unwrap_or(0));
        let mut o = Map::new();
        o.insert("model_id".into(), id.clone().into());
        let dep = deps.values().find(|d| d.model_id == id);
        o.insert("model_size_stats".into(), obj! { "model_size_bytes" => size, "required_native_memory_bytes" => if dep.is_some() { 251_661_500i64 } else { 0 } });
        o.insert("pipeline_count".into(), 0.into());
        if let Some(d) = dep {
            o.insert(
                "inference_stats".into(),
                obj! { "failure_count" => 0, "inference_count" => d.inference_count, "cache_miss_count" => 0, "missing_all_fields_count" => 0, "timestamp" => ctx.now_ms },
            );
            o.insert("deployment_stats".into(), deployment_stats(ctx, d));
        }
        out.push(Value::Obj(o));
    }
    Reply::ok(obj! { "count" => out.len(), "trained_model_stats" => Value::Arr(out) })
}

fn deployment_stats(ctx: &Ctx, d: &Deployment) -> Value {
    let p = &d.params;
    let get = |k: &str| p.get(k).cloned().unwrap_or(Value::Null);
    let mut node = Map::new();
    let mut nv = match node_value(ctx, false) {
        Value::Obj(m) => m,
        _ => Map::new(),
    };
    nv.insert("external_id".into(), ctx.cluster.node.name.clone().into());
    let mut n = Map::new();
    n.insert(ctx.cluster.node.id.clone(), Value::Obj(nv));
    node.insert("node".into(), Value::Obj(n));
    node.insert("routing_state".into(), obj! { "routing_state" => "started" });
    node.insert("inference_count".into(), d.inference_count.into());
    node.insert("average_inference_time_ms".into(), 0.0.into());
    node.insert("average_inference_time_ms_excluding_cache_hits".into(), 0.0.into());
    node.insert("average_inference_process_memory_rss_bytes".into(), 0.into());
    node.insert("inference_cache_hit_count".into(), d.cache_hits.into());
    node.insert("last_access".into(), d.last_access.into());
    node.insert("number_of_pending_requests".into(), 0.into());
    node.insert("start_time".into(), d.start_ms.into());
    node.insert("threads_per_allocation".into(), get("threads_per_allocation"));
    node.insert("number_of_allocations".into(), get("number_of_allocations"));
    node.insert("peak_throughput_per_minute".into(), 0.into());
    node.insert("throughput_last_minute".into(), 0.into());
    node.insert("inference_cache_hit_count_last_minute".into(), 0.into());
    obj! {
        "deployment_id" => get("deployment_id"),
        "model_id" => d.model_id.clone(),
        "threads_per_allocation" => get("threads_per_allocation"),
        "number_of_allocations" => get("number_of_allocations"),
        "queue_capacity" => get("queue_capacity"),
        "state" => "started",
        "allocation_status" => obj! { "allocation_count" => get("number_of_allocations"), "target_allocation_count" => get("number_of_allocations"), "state" => "fully_allocated" },
        "cache_size" => get("cache_size"),
        "priority" => get("priority"),
        "start_time" => d.start_ms,
        "inference_count" => d.inference_count,
        "peak_throughput_per_minute" => 0,
        "nodes" => crate::arr![Value::Obj(node)],
    }
}

pub async fn delete_model(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default().to_string();
    let force = ctx.bool_param("force", false)?;
    {
        let mut deps = deployments(ctx).inner.lock();
        if deps.values().any(|d| d.model_id == id) {
            if !force {
                return Err(conflict(format!("Cannot delete model [{id}] as it is currently deployed")));
            }
            deps.retain(|_, d| d.model_id != id);
            deployments(ctx).note();
        }
    }
    ctx.cluster
        .update(move |st| {
            if store::remove(st, MODELS, &id).is_none() {
                return Err(no_model(&id));
            }
            let aliases: Vec<String> = st.metadata.xpack.iter(ALIASES).filter(|(_, a)| a.get("model_id").and_then(Value::as_str) == Some(id.as_str())).map(|(k, _)| k.clone()).collect();
            for a in aliases {
                store::remove(st, ALIASES, &a);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

fn conflict(reason: impl Into<String>) -> EsError {
    EsError::status(409, reason)
}

pub async fn put_definition_part(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default().to_string();
    let part = ctx.path_param("part").and_then(|p| p.parse::<i64>().ok()).unwrap_or(0);
    let body = ctx.body_map()?.unwrap_or_default();
    let total_len = body.get("total_definition_length").and_then(Value::as_i64).unwrap_or(0);
    let total_parts = body.get("total_parts").and_then(Value::as_i64).unwrap_or(1);
    if part < 0 || part >= total_parts {
        return Err(EsError::action_request_validation(&["[part] must be less than total_parts".to_string()]));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(m) = st.metadata.xpack.get(MODELS, &id).cloned() else { return Err(no_model(&id)) };
            let m = with_roll_state(m, |r| {
                let parts = r.get("parts").and_then(Value::as_i64).unwrap_or(0);
                r.insert("parts".into(), (parts.max(part + 1)).into());
                r.insert("total_parts".into(), total_parts.into());
                r.insert("total_length".into(), total_len.into());
            });
            store::put(st, MODELS, &id, m, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn put_vocabulary(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default().to_string();
    let body = ctx.body_map()?.unwrap_or_default();
    let Some(vocab) = body.get("vocabulary").cloned() else {
        return Err(EsError::action_request_validation(&["[vocabulary] must not be null.".into()]));
    };
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let Some(m) = st.metadata.xpack.get(MODELS, &id).cloned() else { return Err(no_model(&id)) };
            if m.get("model_type").and_then(Value::as_str) != Some("pytorch") {
                return Err(conflict(format!("cannot put vocabulary for model [{id}] as it is not pytorch")));
            }
            let m = with_roll_state(m, |r| {
                r.insert("vocabulary".into(), vocab);
            });
            store::put(st, MODELS, &id, m, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn put_alias(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let model = ctx.path_param("model_id").unwrap_or_default().to_string();
    let alias = ctx.path_param("model_alias").unwrap_or_default().to_string();
    let reassign = ctx.bool_param("reassign", false)?;
    if !valid_id(&alias) {
        return Err(invalid_id("model_alias", &alias));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(MODELS, &model).is_none() {
                return Err(no_model(&model));
            }
            if st.metadata.xpack.get(MODELS, &alias).is_some() {
                return Err(EsError::illegal_argument(format!("model_alias [{alias}] cannot equal model_id [{alias}]")));
            }
            if let Some(cur) = st.metadata.xpack.get(ALIASES, &alias).and_then(|a| a.get("model_id")).map(Value::to_es_string)
                && cur != model
                && !reassign
            {
                return Err(EsError::illegal_argument(format!("cannot assign model_alias [{alias}] to model_id [{model}] as model_alias already refers to [{cur}]. Set parameter [reassign] to [true] if model_alias should be reassigned.")));
            }
            store::put(st, ALIASES, &alias, obj! { "model_id" => model }, now).map(|_| ())
        })
        .await?;
    Reply::acknowledged()
}

pub async fn delete_alias(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let model = ctx.path_param("model_id").unwrap_or_default().to_string();
    let alias = ctx.path_param("model_alias").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            match st.metadata.xpack.get(ALIASES, &alias).and_then(|a| a.get("model_id")).map(Value::to_es_string) {
                None => Err(EsError::resource_not_found(format!("trained model alias [{alias}] could not be found"))),
                Some(m) if m != model => Err(EsError::illegal_argument(format!("trained model alias [{alias}] does not refer to provided model id [{model}]"))),
                Some(_) => {
                    store::remove(st, ALIASES, &alias);
                    Ok(())
                }
            }
        })
        .await?;
    Reply::acknowledged()
}

fn assignment(ctx: &Ctx, d: &Deployment) -> Value {
    let mut routing = Map::new();
    let allocs = d.params.get("number_of_allocations").cloned().unwrap_or(Value::from(1));
    routing.insert(
        ctx.cluster.node.id.clone(),
        obj! { "current_allocations" => allocs.clone(), "target_allocations" => allocs.clone(), "routing_state" => "started", "reason" => "" },
    );
    obj! { "assignment" => obj! {
        "task_parameters" => Value::Obj(d.params.clone()),
        "routing_table" => Value::Obj(routing),
        "assignment_state" => "started",
        "start_time" => crate::time::java_instant_string(d.start_ms),
        "max_assigned_allocations" => allocs,
        "adaptive_allocations" => Value::Null,
    } }
}

pub fn start_deployment(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let id = ctx.path_param("model_id").unwrap_or_default();
    let model_id = resolve(&st, id).ok_or_else(|| no_model(id))?;
    let m = st.metadata.xpack.get(MODELS, &model_id).cloned().unwrap_or(Value::Null);
    if m.get("model_type").and_then(Value::as_str) != Some("pytorch") {
        return Err(EsError::illegal_argument(format!("model [{model_id}] of type [tree_ensemble] cannot be deployed. Only PyTorch models can be deployed")));
    }
    let rs = roll_state(&m);
    let total = rs.get("total_parts").and_then(Value::as_i64).unwrap_or(0);
    if total == 0 || rs.get("parts").and_then(Value::as_i64).unwrap_or(0) < total {
        return Err(EsError::resource_not_found(format!("Could not find trained model definition [{model_id}]")));
    }
    let dep_id = ctx.query("deployment_id").unwrap_or(&model_id).to_string();
    let mut deps = deployments(ctx).inner.lock();
    if deps.contains_key(&dep_id) {
        return Err(EsError::resource_already_exists(format!("Could not start model deployment because an existing deployment with the same id [{dep_id}] exist")));
    }
    let allocations = ctx.query("number_of_allocations").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
    let threads = ctx.query("threads_per_allocation").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1);
    let mut p = Map::new();
    p.insert("model_id".into(), model_id.clone().into());
    p.insert("deployment_id".into(), dep_id.clone().into());
    p.insert("model_bytes".into(), rs.get("total_length").cloned().unwrap_or(Value::from(0)));
    p.insert("threads_per_allocation".into(), threads.into());
    p.insert("number_of_allocations".into(), allocations.into());
    p.insert("queue_capacity".into(), ctx.query("queue_capacity").and_then(|v| v.parse::<i64>().ok()).unwrap_or(10_000).into());
    if let Some(c) = ctx.query("cache_size") {
        p.insert("cache_size".into(), c.into());
    }
    p.insert("priority".into(), ctx.query("priority").unwrap_or("normal").into());
    p.insert("per_deployment_memory_bytes".into(), 0.into());
    p.insert("per_allocation_memory_bytes".into(), 0.into());
    let d = Deployment { model_id, params: p, start_ms: ctx.now_ms, inference_count: 0, cache_hits: 0, seen: Default::default(), last_access: 0 };
    let out = assignment(ctx, &d);
    deps.insert(dep_id, d);
    deployments(ctx).note();
    Reply::ok(out)
}

fn find_deployment<'a>(deps: &'a mut HashMap<String, Deployment>, id: &str) -> Option<&'a mut Deployment> {
    if deps.contains_key(id) {
        return deps.get_mut(id);
    }
    deps.values_mut().find(|d| d.model_id == id)
}

pub fn update_deployment(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let mut deps = deployments(ctx).inner.lock();
    let Some(d) = find_deployment(&mut deps, id) else {
        return Err(EsError::resource_not_found(format!("deployment for model with id [{id}] not found")));
    };
    if let Some(n) = body.get("number_of_allocations").or_else(|| ctx.query("number_of_allocations").map(|_| &Value::Null)) {
        let n = n.as_i64().or_else(|| ctx.query("number_of_allocations").and_then(|v| v.parse().ok())).unwrap_or(1);
        d.params.insert("number_of_allocations".into(), n.into());
        deployments(ctx).note();
    }
    let d = d.clone();
    Reply::ok(assignment(ctx, &d))
}

pub fn stop_deployment(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default();
    let mut deps = deployments(ctx).inner.lock();
    let key = deps.iter().find(|(k, d)| k.as_str() == id || d.model_id == id).map(|(k, _)| k.clone());
    match key {
        Some(k) => {
            deps.remove(&k);
            deployments(ctx).note();
            Reply::ok(obj! { "stopped" => true })
        }
        None if ctx.bool_param("allow_no_match", true)? => Reply::ok(obj! { "stopped" => true }),
        None => Err(EsError::resource_not_found(format!("No known trained model deployment with id [{id}]"))),
    }
}

pub fn clear_cache(ctx: &Ctx) -> EsResult<Reply> {
    let id = ctx.path_param("model_id").unwrap_or_default();
    let mut deps = deployments(ctx).inner.lock();
    let Some(d) = find_deployment(&mut deps, id) else {
        return Err(EsError::resource_not_found(format!("No known trained model deployment with id [{id}]")));
    };
    d.seen.clear();
    Reply::ok(obj! { "cleared" => true })
}

/// Evaluates one tree (`tree_structure` nodes) over the document's features.
fn eval_tree(tree: &Value, doc: &Value) -> f64 {
    let features: Vec<String> = tree.get("feature_names").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect()).unwrap_or_default();
    let nodes = tree.get("tree_structure").and_then(Value::as_array).cloned().unwrap_or_default();
    let by_index: HashMap<i64, &Value> = nodes.iter().filter_map(|n| n.get("node_index").and_then(Value::as_i64).map(|i| (i, n))).collect();
    let mut cur = 0i64;
    for _ in 0..nodes.len().max(1) {
        let Some(n) = by_index.get(&cur) else { break };
        if let Some(leaf) = n.get("leaf_value") {
            return match leaf {
                Value::Arr(a) => a.first().and_then(Value::as_f64).unwrap_or(0.0),
                v => v.as_f64().unwrap_or(0.0),
            };
        }
        let fi = n.get("split_feature").and_then(Value::as_i64).unwrap_or(0) as usize;
        let threshold = n.get("threshold").and_then(Value::as_f64).unwrap_or(0.0);
        let x = features.get(fi).and_then(|f| doc.get(f)).and_then(Value::as_f64);
        let left = n.get("left_child").and_then(Value::as_i64).unwrap_or(-1);
        let right = n.get("right_child").and_then(Value::as_i64).unwrap_or(-1);
        cur = match x {
            Some(v) if v < threshold || (v == threshold && n.get("decision_type").and_then(Value::as_str) == Some("lte")) => left,
            Some(_) => right,
            None if n.get("default_left").and_then(Value::as_bool).unwrap_or(true) => left,
            None => right,
        };
    }
    0.0
}

/// A `text_expansion` trained model as the `inference_id` of a `sparse_vector` query
/// (`CoordinatedInferenceAction` falls back from endpoints to trained models): `None` when `id`
/// names no such model. roll runs no PyTorch models: a deployed one expands a text into the
/// words of its vocabulary that the text holds, each weighted 1 (BERT's basic tokenization,
/// lowercased when the model's tokenizer lowercases).
pub(crate) fn text_expansion(cluster: &crate::cluster::Cluster, id: &str, text: &str) -> Option<EsResult<Vec<(String, f32)>>> {
    let st = cluster.state();
    let deps = cluster.xpack.deployments.inner.lock();
    let model_id = resolve(&st, id).or_else(|| deps.get(id).map(|d| d.model_id.clone()))?;
    let m = st.metadata.xpack.get(MODELS, &model_id)?;
    let cfg = m.pointer(&["inference_config", "text_expansion"])?;
    if !deps.contains_key(id) && !deps.values().any(|d| d.model_id == model_id) {
        return Some(Err(conflict(format!("Trained model [{model_id}] is not deployed"))));
    }
    let lower = cfg.pointer(&["tokenization", "bert", "do_lower_case"]).and_then(Value::as_bool).unwrap_or(false);
    let vocab = roll_state(m).get("vocabulary").and_then(Value::as_array).cloned().unwrap_or_default();
    let vocab: std::collections::HashSet<String> = vocab.iter().map(Value::to_es_string).collect();
    let mut out: Vec<(String, f32)> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation()).filter(|w| !w.is_empty()) {
        let word = if lower { word.to_lowercase() } else { word.to_string() };
        if vocab.contains(&word) && !out.iter().any(|(t, _)| *t == word) {
            out.push((word, 1.0));
        }
    }
    Some(Ok(out))
}

pub fn infer(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let id = ctx.path_param("model_id").unwrap_or_default();
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let docs = body.get("docs").and_then(Value::as_array).cloned().unwrap_or_default();
    let model_id = match resolve(&st, id) {
        Some(m) => m,
        None => {
            let deps = deployments(ctx).inner.lock();
            deps.get(id).map(|d| d.model_id.clone()).ok_or_else(|| no_model(id))?
        }
    };
    let m = st.metadata.xpack.get(MODELS, &model_id).cloned().unwrap_or(Value::Null);
    if m.get("model_type").and_then(Value::as_str) == Some("pytorch") {
        let mut deps = deployments(ctx).inner.lock();
        let Some(d) = find_deployment(&mut deps, id) else {
            return Err(conflict(format!("Trained model [{model_id}] is not deployed")));
        };
        let mut results = Vec::new();
        for doc in &docs {
            let input = doc.get("text_field").or_else(|| doc.get("input")).map(Value::to_es_string).unwrap_or_default();
            d.inference_count += 1;
            if !d.seen.insert(input) {
                d.cache_hits += 1;
            }
            d.last_access = ctx.now_ms;
            results.push(obj! { "predicted_value" => crate::arr![crate::arr![1.0, 1.0]] });
        }
        return Reply::ok(obj! { "inference_results" => Value::Arr(results) });
    }
    let def = roll_state(&m).get("definition").cloned().unwrap_or(Value::Null);
    let tm = def.get("trained_model").cloned().unwrap_or(Value::Null);
    let classification = m.pointer(&["inference_config", "classification"]).is_some();
    let labels: Vec<Value> = tm
        .pointer(&["tree", "classification_labels"])
        .or_else(|| tm.pointer(&["ensemble", "classification_labels"]))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::new();
    for doc in &docs {
        let score = if let Some(tree) = tm.get("tree") {
            eval_tree(tree, doc)
        } else if let Some(e) = tm.get("ensemble") {
            let models = e.get("trained_models").and_then(Value::as_array).cloned().unwrap_or_default();
            let weights: Vec<f64> = e.pointer(&["aggregate_output", "weighted_sum", "weights"]).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
            models.iter().enumerate().map(|(i, t)| t.get("tree").map_or(0.0, |t| eval_tree(t, doc)) * weights.get(i).copied().unwrap_or(1.0)).sum()
        } else {
            0.0
        };
        if classification {
            let label = labels.get(score.round().max(0.0) as usize).cloned().unwrap_or_else(|| Value::from(score.to_string()));
            results.push(obj! { "predicted_value" => label, "prediction_probability" => 1.0, "prediction_score" => 1.0 });
        } else {
            results.push(obj! { "predicted_value" => score });
        }
    }
    Reply::ok(obj! { "inference_results" => Value::Arr(results) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_evaluate() {
        let tree = obj! {
            "feature_names" => crate::arr!["a"],
            "tree_structure" => crate::arr![
                obj! { "node_index" => 0, "split_feature" => 0, "threshold" => 5.0, "left_child" => 1, "right_child" => 2 },
                obj! { "node_index" => 1, "leaf_value" => 1.0 },
                obj! { "node_index" => 2, "leaf_value" => 2.0 },
            ],
        };
        assert_eq!(eval_tree(&tree, &obj! { "a" => 3 }), 1.0);
        assert_eq!(eval_tree(&tree, &obj! { "a" => 7 }), 2.0);
        assert_eq!(eval_tree(&tree, &obj! {}), 1.0);
    }

    #[test]
    fn inference_config_defaults() {
        let c = default_inference_config(&obj! { "regression" => obj! {} });
        assert_eq!(c.to_json(), r#"{"regression":{"results_field":"predicted_value","num_top_feature_importance_values":0}}"#);
        let c = default_inference_config(&obj! { "pass_through" => obj! { "tokenization" => obj! { "bert" => obj! { "with_special_tokens" => false } } } });
        assert_eq!(
            c.to_json(),
            r#"{"pass_through":{"vocabulary":{"index":".ml-inference-native-000002"},"tokenization":{"bert":{"with_special_tokens":false,"do_lower_case":false,"max_sequence_length":512,"truncate":"first","span":-1}}}}"#
        );
    }
}
