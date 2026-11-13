//! The inference API (`_inference`): endpoints that turn text into embeddings, rankings and
//! completions by calling a model service, as Elasticsearch 9.5.5's inference plugin does.
//!
//! An endpoint is stored like Elasticsearch stores it (`ModelConfigurations`: id, task type,
//! service, the service's settings in their stored form, task settings, chunking settings) in
//! the cluster metadata ([`ENDPOINTS`], home `.inference`), and its secrets (API keys) apart
//! from it ([`SECRETS`], home `.secrets-inference`), sealed ([`crate::seal`]): nothing that
//! prints an endpoint reads them, and they are opened only to send a request.
//!
//! Services (`services`): `openai` (with `url`, any OpenAI-compatible server), `cohere`,
//! `anthropic`, and `elasticsearch`, whose models need ML nodes roll doesn't run: its three
//! preconfigured endpoints (`.elser-2-elasticsearch`, `.multilingual-e5-small-elasticsearch`,
//! `.rerank-v1-elasticsearch`) are listed, and using them answers what a cluster without ML
//! nodes answers. On YAML test clusters (`roll.inference.test_services`) the services of
//! Elasticsearch's `inference-service-test` plugin are there too.
//!
//! Creating or updating an endpoint calls the service once, as Elasticsearch's model validators
//! do (`how big`), which also learns a text embedding model's dimensions.

pub mod chunking;
pub mod http;
pub mod results;
pub mod semantic;
pub mod services;
pub mod settings;

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::store;
use results::Results;
use services::Service;
use std::time::Duration;

/// Endpoints by id (stored form, without secrets).
pub const ENDPOINTS: &str = "inference";
/// Endpoints' secrets by id: `{key: sealed value}`.
pub const SECRETS: &str = "inference_secret";
/// Preconfigured endpoints deleted with `force=true`.
pub const DELETED_DEFAULTS: &str = "inference.deleted_default";

/// What kind of inference an endpoint does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaskType {
    TextEmbedding,
    SparseEmbedding,
    Rerank,
    Completion,
    Any,
    ChatCompletion,
    Embedding,
}

impl TaskType {
    pub const ALL: [TaskType; 7] =
        [TaskType::TextEmbedding, TaskType::SparseEmbedding, TaskType::Rerank, TaskType::Completion, TaskType::Any, TaskType::ChatCompletion, TaskType::Embedding];

    /// `TaskType.fromString`: the name in any case, surrounding spaces ignored.
    pub fn parse(s: &str) -> Option<TaskType> {
        let s = s.trim().to_ascii_lowercase();
        TaskType::ALL.into_iter().find(|t| t.name() == s)
    }

    /// `fromStringOrStatusException`.
    pub fn parse_or_error(s: &str) -> EsResult<TaskType> {
        TaskType::parse(s).ok_or_else(|| EsError::status(400, format!("Unknown task_type [{s}]")))
    }

    pub fn name(self) -> &'static str {
        match self {
            TaskType::TextEmbedding => "text_embedding",
            TaskType::SparseEmbedding => "sparse_embedding",
            TaskType::Rerank => "rerank",
            TaskType::Completion => "completion",
            TaskType::Any => "any",
            TaskType::ChatCompletion => "chat_completion",
            TaskType::Embedding => "embedding",
        }
    }

    pub fn upper(self) -> String {
        self.name().to_ascii_uppercase()
    }

    pub fn is_any_or_same(self, other: TaskType) -> bool {
        self == TaskType::Any || other == TaskType::Any || self == other
    }

    /// Whether endpoints of this type get chunking settings (the embedding tasks).
    pub fn chunks(self) -> bool {
        matches!(self, TaskType::TextEmbedding | TaskType::SparseEmbedding | TaskType::Embedding)
    }
}

/// `The [service] service does not support task type [task]`.
pub fn unsupported_task(task: TaskType, service: &str) -> EsError {
    EsError::status(400, format!("The [{service}] service does not support task type [{}]", task.name()))
}

/// An endpoint.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub task: TaskType,
    pub service: String,
    /// The service settings in their stored form (`toXContent`, internal fields included).
    pub service_settings: Map,
    pub task_settings: Map,
    pub chunking: Option<chunking::Chunking>,
    /// Secret settings as stored (sealed when the node has a key).
    pub secrets: Map,
}

impl Endpoint {
    fn from_stored(id: &str, v: &Value, secrets: Option<&Value>) -> Option<Endpoint> {
        Some(Endpoint {
            id: id.to_string(),
            task: TaskType::parse(v.get("task_type")?.as_str()?)?,
            service: v.get("service")?.as_str()?.to_string(),
            service_settings: v.get("service_settings").and_then(Value::as_object).cloned().unwrap_or_default(),
            task_settings: v.get("task_settings").and_then(Value::as_object).cloned().unwrap_or_default(),
            chunking: v.get("chunking_settings").and_then(chunking::Chunking::from_stored),
            secrets: secrets.and_then(Value::as_object).cloned().unwrap_or_default(),
        })
    }

    fn to_stored(&self) -> Value {
        let mut m = Map::new();
        m.insert("inference_id".into(), self.id.clone().into());
        m.insert("task_type".into(), self.task.name().into());
        m.insert("service".into(), self.service.clone().into());
        m.insert("service_settings".into(), Value::Obj(self.service_settings.clone()));
        m.insert("task_settings".into(), Value::Obj(self.task_settings.clone()));
        if let Some(c) = &self.chunking {
            m.insert("chunking_settings".into(), c.to_value());
        }
        Value::Obj(m)
    }

    /// The endpoint as the API prints it (`toFilteredXContent`): the service's exposed settings,
    /// task settings only when there are some, never a secret.
    pub fn to_value(&self, cluster: &Cluster) -> Value {
        let mut m = Map::new();
        m.insert("inference_id".into(), self.id.clone().into());
        m.insert("task_type".into(), self.task.name().into());
        m.insert("service".into(), self.service.clone().into());
        let exposed = Service::find(cluster, &self.service).map_or_else(|| self.service_settings.clone(), |s| s.exposed(self));
        m.insert("service_settings".into(), Value::Obj(exposed));
        if !self.task_settings.is_empty() {
            m.insert("task_settings".into(), Value::Obj(self.task_settings.clone()));
        }
        if let Some(c) = &self.chunking {
            m.insert("chunking_settings".into(), c.to_value());
        }
        Value::Obj(m)
    }

    /// A secret in the clear, for a request about to be sent.
    pub fn secret(&self, cluster: &Cluster, key: &str) -> EsResult<Option<String>> {
        match self.secrets.get(key).and_then(Value::as_str) {
            Some(s) => crate::seal::open(cluster, SECRET_KIND, s).map(Some),
            None => Ok(None),
        }
    }
}

/// What an endpoint's secret values are sealed as ([`crate::seal`]).
pub const SECRET_KIND: &str = "inference api key";

/// Whether `id` is a valid inference id (`MlStrings.isValidId`).
fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty() && edge(b[0]) && edge(b[b.len() - 1]) && b.iter().all(|&c| edge(c) || matches!(c, b'_' | b'-' | b'.'))
}

/// The endpoint `id`, preconfigured ones included.
pub fn endpoint(cluster: &Cluster, st: &crate::meta::ClusterState, id: &str) -> Option<Endpoint> {
    if let Some(v) = st.metadata.xpack.get(ENDPOINTS, id) {
        return Endpoint::from_stored(id, v, st.metadata.xpack.get(SECRETS, id));
    }
    services::builtin::default_endpoint(id).filter(|_| st.metadata.xpack.get(DELETED_DEFAULTS, id).is_none()).filter(|_| cluster.config.dialect == crate::cluster::Dialect::Elasticsearch)
}

/// Every endpoint, by id.
pub fn endpoints(cluster: &Cluster, st: &crate::meta::ClusterState) -> Vec<Endpoint> {
    let mut out: Vec<Endpoint> = st.metadata.xpack.iter(ENDPOINTS).filter_map(|(id, v)| Endpoint::from_stored(id, v, st.metadata.xpack.get(SECRETS, id))).collect();
    if cluster.config.dialect == crate::cluster::Dialect::Elasticsearch {
        for d in services::builtin::defaults() {
            if st.metadata.xpack.get(DELETED_DEFAULTS, &d.id).is_none() && !out.iter().any(|e| e.id == d.id) {
                out.push(d);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn not_found(id: &str) -> EsError {
    EsError::resource_not_found(format!("Inference endpoint not found [{id}]"))
}

/// The task type and inference id of a request's path: `{task_type}/{id}` or `{id}`.
fn path_ids(ctx: &Ctx) -> EsResult<(TaskType, String)> {
    let p: Vec<&str> = ctx.route.params.iter().map(|(_, v)| v.as_str()).collect();
    match p.as_slice() {
        [id] => Ok((TaskType::Any, id.to_string())),
        [task, id, ..] => Ok((TaskType::parse_or_error(task)?, id.to_string())),
        [] => Ok((TaskType::Any, "_all".into())),
    }
}

/// `ServiceUtils.resolveTaskType`.
fn resolve_task(url: TaskType, body: Option<&Value>) -> EsResult<TaskType> {
    let Some(b) = body else {
        if url == TaskType::Any {
            return Err(EsError::status(400, "model is missing required setting [task_type]"));
        }
        return Ok(url);
    };
    let b = b.to_es_string();
    let parsed = TaskType::parse_or_error(&b)?;
    if parsed == TaskType::Any {
        return Err(EsError::status(400, "task_type [any] is not valid type for inference"));
    }
    if !parsed.is_any_or_same(url) {
        return Err(EsError::status(400, format!("Cannot resolve conflicting task_type parameter in the request URL [{}] and the request body [{b}]", url.name())));
    }
    Ok(parsed)
}

fn timeout(ctx: &Ctx) -> EsResult<Duration> {
    let t = ctx.time_param("timeout", crate::units::TimeValue::parse("30s", "timeout")?)?;
    Ok(Duration::from_millis(t.as_millis().max(1) as u64))
}

/// `PUT _inference/{task_type}/{id}` (and every `inference.put_<service>` route).
pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let (url_task, id) = path_ids(ctx)?;
    if !valid_id(&id) {
        return Err(EsError::action_request_validation(&[format!(
            "Invalid inference_id; '{id}' can contain lowercase alphanumeric (a-z and 0-9), hyphens or underscores; must start and end with alphanumeric"
        )]));
    }
    if services::builtin::default_endpoint(&id).is_some() {
        return Err(EsError::status(400, format!("[{id}] is a reserved inference ID. Cannot create a new inference endpoint with a reserved ID.")));
    }
    let mut config = ctx.body_map()?.unwrap_or_default();
    let task = resolve_task(url_task, config.shift_remove("task_type").as_ref())?;
    let Some(service_name) = config.shift_remove("service").map(|s| s.to_es_string()) else {
        return Err(EsError::status(400, "Inference endpoint configuration is missing the [service] setting"));
    };
    let Some(service) = Service::find(ctx.cluster, &service_name) else {
        return Err(EsError::status(400, format!("Unknown service [{service_name}]")));
    };
    let timeout = timeout(ctx)?;
    let mut ep = service.parse(ctx.cluster, &id, task, config)?;
    // The elasticsearch service's model deployment starts after the endpoint is stored.
    let deploys = service == Service::Elasticsearch;
    if !skip_validation(ctx.cluster) && !deploys {
        service.validate(ctx.cluster, &mut ep, timeout).await?;
    }
    let stored = ep.to_stored();
    let secrets = (!ep.secrets.is_empty()).then(|| Value::Obj(ep.secrets.clone()));
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(ENDPOINTS, &id).is_some() {
                return Err(EsError::resource_already_exists(format!("Inference endpoint [{id}] already exists")));
            }
            store::put(st, ENDPOINTS, &id, stored, now)?;
            if let Some(s) = secrets {
                store::put(st, SECRETS, &id, s, now)?;
            }
            Ok(())
        })
        .await?;
    if deploys && !skip_validation(ctx.cluster) {
        return Err(services::builtin::no_ml_nodes());
    }
    Reply::ok(ep.to_value(ctx.cluster))
}

/// `xpack.inference.skip_validate_and_start`: store endpoints without calling the service.
fn skip_validation(cluster: &Cluster) -> bool {
    cluster.config.node_setting("xpack.inference.skip_validate_and_start").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// `GET _inference[/{task_type}]/{id}`, `GET _inference/_services[/{task_type}]`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let p: Vec<&str> = ctx.route.params.iter().map(|(_, v)| v.as_str()).collect();
    match p.as_slice() {
        ["_services"] => return Reply::ok(services::configurations(ctx.cluster, None)),
        ["_services", task] => return Reply::ok(services::configurations(ctx.cluster, Some(TaskType::parse_or_error(task)?))),
        _ => {}
    }
    let (task, id) = path_ids(ctx)?;
    let st = ctx.state();
    let list: Vec<Endpoint> = if id == "_all" || id.is_empty() {
        endpoints(ctx.cluster, &st).into_iter().filter(|e| task == TaskType::Any || e.task == task).collect()
    } else {
        let e = endpoint(ctx.cluster, &st, &id).ok_or_else(|| not_found(&id))?;
        if task != TaskType::Any && e.task != task {
            return Err(EsError::status(
                400,
                format!("Requested task type [{}] does not match the inference endpoint's task type [{}]", task.name(), e.task.name()),
            ));
        }
        vec![e]
    };
    Reply::ok(obj! { "endpoints" => Value::Arr(list.iter().map(|e| e.to_value(ctx.cluster)).collect()) })
}

/// Indices whose `semantic_text` fields and ingest pipelines whose `inference` processors use
/// an endpoint.
fn uses(st: &crate::meta::ClusterState, id: &str) -> (Vec<String>, Vec<String>) {
    let mut indexes: Vec<String> = st
        .metadata
        .indices
        .values()
        .filter(|i| semantic::inference_ids(&i.mapping).iter().any(|x| x == id))
        .map(|i| i.name.clone())
        .collect();
    indexes.sort();
    let mut pipelines: Vec<String> = st
        .metadata
        .pipelines
        .iter()
        .filter(|(_, p)| {
            p.get("processors").and_then(Value::as_array).into_iter().flatten().any(|proc| {
                proc.get("inference").and_then(|i| i.get("model_id")).and_then(Value::as_str) == Some(id)
            })
        })
        .map(|(name, _)| name.clone())
        .collect();
    pipelines.sort();
    (indexes, pipelines)
}

/// `DELETE _inference[/{task_type}]/{id}`.
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let (task, id) = path_ids(ctx)?;
    let force = ctx.bool_param("force", false)?;
    let dry_run = ctx.bool_param("dry_run", false)?;
    let st = ctx.state();
    let e = endpoint(ctx.cluster, &st, &id).ok_or_else(|| not_found(&id))?;
    if task != TaskType::Any && e.task != task {
        return Err(EsError::status(400, format!("Requested task type [{}] does not match the inference endpoint's task type [{}]", task.name(), e.task.name())));
    }
    let preconfigured = services::builtin::default_endpoint(&id).is_some();
    let (indexes, pipelines) = uses(&st, &id);
    let names = |v: &[String]| Value::Arr(v.iter().map(|s| Value::from(s.as_str())).collect());
    if dry_run {
        return Reply::ok(obj! { "acknowledged" => false, "pipelines" => names(&pipelines), "indexes" => names(&indexes), "error_message" => "" });
    }
    if preconfigured && !force {
        return Err(EsError::status(400, format!("[{id}] is a reserved inference endpoint. Use the force=true query parameter to delete the inference endpoint.")));
    }
    if !force && !pipelines.is_empty() {
        return Err(EsError::status(
            409,
            format!(
                "Inference endpoint {id} is referenced by pipelines: {} and cannot be deleted. Remove the inference endpoint from the pipelines and try again, or use force to ignore this warning and delete the inference endpoint.",
                java_set(&pipelines)
            ),
        ));
    }
    if !force && !indexes.is_empty() {
        return Err(EsError::status(
            400,
            format!(
                "Inference endpoint {id} is being used in the mapping for indexes: {}. Ensure that no index mappings are using this inference endpoint, or use force to ignore this warning and delete the inference endpoint.",
                java_set(&indexes)
            ),
        ));
    }
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if preconfigured {
                store::put(st, DELETED_DEFAULTS, &id, Value::Bool(true), now)?;
            } else {
                store::remove(st, ENDPOINTS, &id);
                store::remove(st, SECRETS, &id);
            }
            Ok(())
        })
        .await?;
    Reply::ok(obj! { "acknowledged" => true, "pipelines" => names(&pipelines), "indexes" => names(&indexes) })
}

/// A set of names as Java prints a `HashSet` of strings.
fn java_set(names: &[String]) -> String {
    let m: Map = names.iter().map(|n| (n.clone(), Value::Null)).collect();
    format!("[{}]", crate::json::java_hash_map(m, 16).keys().cloned().collect::<Vec<_>>().join(", "))
}

/// `PUT _inference[/{task_type}]/{id}/_update`.
pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let (url_task, id) = path_ids(ctx)?;
    let mut body = ctx.body_map()?.unwrap_or_default();
    if body.is_empty() {
        return Err(EsError::status(400, "Request body is empty"));
    }
    let mut rest = Map::new();
    for (k, v) in &body {
        if !matches!(k.as_str(), "service_settings" | "task_settings" | "chunking_settings" | "task_type") {
            rest.insert(k.clone(), v.clone());
        }
    }
    if !rest.is_empty() {
        return Err(EsError::status(
            400,
            format!("Request contained fields which cannot be updated, remove these fields and try again [{}]", settings::java_string(&Value::Obj(rest))),
        ));
    }
    let task = resolve_task(url_task, body.shift_remove("task_type").as_ref())?;
    let st = ctx.state();
    let current = endpoint(ctx.cluster, &st, &id).ok_or_else(|| not_found(&id))?;
    if services::builtin::default_endpoint(&id).is_some() {
        return Err(EsError::status(400, format!("Cannot update inference endpoint [{id}] because it is a preconfigured endpoint")));
    }
    if current.task != task {
        return Err(EsError::status(400, format!("Task type must match the task type of the existing endpoint, expected [{}] but got [{}]", current.task.name(), task.name())));
    }
    let service = Service::find(ctx.cluster, &current.service).ok_or_else(|| EsError::status(400, format!("Unknown service [{}]", current.service)))?;
    let mut ep = service.update(ctx.cluster, &current, body)?;
    let timeout = timeout(ctx)?;
    if !skip_validation(ctx.cluster) {
        service.validate(ctx.cluster, &mut ep, timeout).await?;
    }
    let stored = ep.to_stored();
    let secrets = Value::Obj(ep.secrets.clone());
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            if st.metadata.xpack.get(ENDPOINTS, &id).is_none() {
                return Err(not_found(&id));
            }
            store::put(st, ENDPOINTS, &id, stored, now)?;
            if let Value::Obj(m) = &secrets
                && !m.is_empty()
            {
                store::put(st, SECRETS, &id, secrets, now)?;
            }
            Ok(())
        })
        .await?;
    Reply::ok(ep.to_value(ctx.cluster))
}

/// The input of one inference call.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub input: Vec<String>,
    /// Rerank's query.
    pub query: Option<String>,
    pub top_n: Option<i64>,
    pub return_documents: Option<bool>,
    /// Task settings for this call, over the endpoint's.
    pub task_settings: Map,
    pub input_type: Option<String>,
    pub timeout: Duration,
}

impl Request {
    pub fn new(input: Vec<String>, timeout: Duration) -> Request {
        Request { input, timeout, ..Request::default() }
    }
}

/// The body of `POST _inference/…` for an endpoint of type `task`.
fn parse_request(ctx: &Ctx, task: TaskType, timeout: Duration) -> EsResult<Request> {
    let body = ctx.body_map()?.unwrap_or_default();
    let mut r = Request { timeout, ..Request::default() };
    let mut input = None;
    for (k, v) in &body {
        match k.as_str() {
            "input" => {
                input = Some(match v {
                    Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                    Value::Null => continue,
                    other => vec![other.to_es_string()],
                })
            }
            "input_type" => r.input_type = Some(v.to_es_string()),
            "task_settings" => r.task_settings = v.as_object().cloned().unwrap_or_default(),
            "timeout" => {}
            "query" if task == TaskType::Rerank => r.query = Some(v.to_es_string()),
            "top_n" if task == TaskType::Rerank => r.top_n = v.as_i64(),
            "return_documents" if task == TaskType::Rerank => r.return_documents = v.as_bool(),
            _ => return Err(crate::xpack::unknown_field(ctx, "cluster:internal/xpack/inference", k)),
        }
    }
    let Some(input) = input else {
        return Err(EsError::action_request_validation(&["Field [input] cannot be null".into()]));
    };
    if input.is_empty() {
        return Err(EsError::action_request_validation(&["Field [input] cannot be an empty array".into()]));
    }
    r.input = input;
    if task == TaskType::Rerank {
        match &r.query {
            None => return Err(EsError::action_request_validation(&["Field [query] cannot be null".into()])),
            Some(q) if q.is_empty() => return Err(EsError::action_request_validation(&["Field [query] cannot be empty".into()])),
            _ => {}
        }
        if r.top_n.is_some_and(|n| n < 1) {
            return Err(EsError::action_request_validation(&["Field [top_n] must be greater than or equal to 1".into()]));
        }
    }
    if let Some(it) = &r.input_type
        && task != TaskType::TextEmbedding
        && !matches!(it.to_ascii_lowercase().as_str(), "internal_search" | "internal_ingest" | "unspecified")
    {
        return Err(EsError::action_request_validation(&[format!("Field [input_type] cannot be specified for task type [{}]", task.name())]));
    }
    Ok(r)
}

/// The endpoint a run names, checked against the path's task type.
fn run_target(ctx: &Ctx, url_task: TaskType, id: &str) -> EsResult<(Endpoint, Service)> {
    let st = ctx.state();
    let e = endpoint(ctx.cluster, &st, id).ok_or_else(|| not_found(id))?;
    if !url_task.is_any_or_same(e.task) {
        return Err(EsError::status(
            400,
            format!("Incompatible task_type, the requested type [{}] does not match the model type [{}]", url_task.name(), e.task.name()),
        ));
    }
    let s = Service::find(ctx.cluster, &e.service).ok_or_else(|| EsError::status(400, format!("Unknown service [{}]", e.service)))?;
    Ok((e, s))
}

/// `POST _inference[/{task_type}]/{id}`.
pub async fn infer(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let p: Vec<String> = ctx.route.params.iter().map(|(_, v)| v.clone()).collect();
    // `POST _inference/{id}/_stream` routes here (the specs list only the typed forms).
    if p.len() == 2 && p[1] == "_stream" {
        return stream(ctx, TaskType::Any, &p[0]).await;
    }
    let (url_task, id) = path_ids(ctx)?;
    let (ep, service) = run_target(ctx, url_task, &id)?;
    if ep.task == TaskType::ChatCompletion {
        return Err(EsError::status(400, "The [chat_completion] task type only supports streaming, please try again with the _stream API"));
    }
    let req = parse_request(ctx, ep.task, timeout(ctx)?)?;
    let results = service.infer(ctx.cluster, &ep, &req).await?;
    Reply::ok(results.to_value())
}

/// `POST _inference/completion/{id}/_stream`, `POST _inference/chat_completion/{id}/_stream`.
/// `POST _inference/{task_type}/{id}/_stream`.
pub async fn stream_route(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let p: Vec<&str> = ctx.route.params.iter().map(|(_, v)| v.as_str()).collect();
    let (task, id) = match p.as_slice() {
        [task, id] => (TaskType::parse_or_error(task)?, id.to_string()),
        _ if ctx.api == crate::routes::Api::InferenceChatCompletionUnified => (TaskType::ChatCompletion, p.first().copied().unwrap_or_default().to_string()),
        _ => (TaskType::Completion, p.first().copied().unwrap_or_default().to_string()),
    };
    stream(ctx, task, &id).await
}

async fn stream(ctx: &Ctx<'_>, url_task: TaskType, id: &str) -> EsResult<Reply> {
    let (ep, service) = run_target(ctx, url_task, id)?;
    let timeout = timeout(ctx)?;
    let events = match ep.task {
        TaskType::Completion => {
            let req = parse_request(ctx, ep.task, timeout)?;
            service.stream_completion(ctx.cluster, &ep, &req).await?
        }
        TaskType::ChatCompletion => {
            let body = ctx.body()?.unwrap_or_else(Value::object);
            service.stream_chat(ctx.cluster, &ep, &body, timeout).await?
        }
        other => return Err(EsError::status(400, format!("Streaming is not allowed for service [{}] and task [{}]", ep.service, other.name()))),
    };
    Ok(Reply::Raw(200, "text/event-stream".into(), sse(&events)))
}

/// Server-sent events as Elasticsearch's `ServerSentEventsRestActionListener` writes them: each
/// event starts with a byte order mark, then `event: message` and its `data:`; `[DONE]` last.
fn sse(events: &[Value]) -> bytes::Bytes {
    let mut out = String::new();
    for e in events {
        out.push_str("\u{feff}event: message\ndata: ");
        out.push_str(&e.to_json());
        out.push_str("\n\n");
    }
    out.push_str("\u{feff}event: message\ndata: [DONE]\n\n");
    bytes::Bytes::from(out)
}

/// Runs an endpoint from inside roll (semantic text, the `semantic` query, retrievers).
pub async fn run(cluster: &Cluster, id: &str, req: &Request) -> EsResult<(Endpoint, Results)> {
    let st = cluster.state();
    let ep = endpoint(cluster, &st, id).ok_or_else(|| not_found(id))?;
    let s = Service::find(cluster, &ep.service).ok_or_else(|| EsError::status(400, format!("Unknown service [{}]", ep.service)))?;
    let r = s.infer(cluster, &ep, req).await?;
    Ok((ep, r))
}

/// The `inference` section of `_xpack/usage` (`TransportInferenceUsageAction`): endpoints per
/// service and task type, per task type over every service (`_all`), and per preconfigured
/// model (`_<service>_<model>`), each with the `semantic_text` fields that use them.
pub fn usage(cluster: &Cluster, st: &crate::meta::ClusterState) -> Value {
    use std::collections::{BTreeMap, BTreeSet};
    let eps = endpoints(cluster, st);
    // Inference fields of visible indices by (service, task): index → field count, ids.
    let by_id: std::collections::HashMap<&str, &Endpoint> = eps.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut fields: Vec<(String, TaskType, String, String)> = Vec::new(); // service, task, index, inference id
    for idx in st.metadata.indices.values() {
        if idx.system || idx.is_hidden() {
            continue;
        }
        for inference_id in semantic::inference_ids(&idx.mapping) {
            if let Some(e) = by_id.get(inference_id.as_str()) {
                fields.push((e.service.clone(), e.task, idx.name.clone(), inference_id));
            }
        }
    }
    struct Stat {
        service: String,
        task: TaskType,
        count: i64,
        fields: Vec<(String, String)>, // index, inference id
    }
    let mut stats: BTreeMap<String, Stat> = BTreeMap::new();
    let key = |service: &str, task: TaskType| format!("{service}:{}", task.upper());
    for e in &eps {
        for service in [e.service.as_str(), "_all"] {
            stats.entry(key(service, e.task)).or_insert_with(|| Stat { service: service.into(), task: e.task, count: 0, fields: vec![] }).count += 1;
        }
    }
    for t in TaskType::ALL {
        if t != TaskType::Any {
            stats.entry(key("_all", t)).or_insert_with(|| Stat { service: "_all".into(), task: t, count: 0, fields: vec![] });
        }
    }
    for (service, task, index, id) in &fields {
        if let Some(s) = stats.get_mut(&key(service, *task)) {
            s.fields.push((index.clone(), id.clone()));
        }
        if let Some(s) = stats.get_mut(&key("_all", *task)) {
            s.fields.push((index.clone(), id.clone()));
        }
    }
    // Preconfigured models: every endpoint of their service and task that uses the model.
    for d in services::builtin::defaults() {
        let model = services::builtin::model_of(&d);
        let name = format!("_{}_{}", d.service, model.replace('.', "_"));
        let users: Vec<&Endpoint> = eps.iter().filter(|e| e.service == d.service && e.task == d.task && services::builtin::model_of(e) == model).collect();
        if users.is_empty() {
            continue;
        }
        let ids: BTreeSet<&str> = users.iter().map(|e| e.id.as_str()).collect();
        let f = fields.iter().filter(|(_, _, _, id)| ids.contains(id.as_str())).map(|(_, _, i, id)| (i.clone(), id.clone())).collect();
        stats.insert(format!("{name}:{}", d.task.upper()), Stat { service: name, task: d.task, count: users.len() as i64, fields: f });
    }
    let models: Vec<Value> = stats
        .values()
        .map(|s| {
            let mut m = Map::new();
            m.insert("service".into(), s.service.clone().into());
            m.insert("task_type".into(), s.task.upper().into());
            m.insert("count".into(), s.count.into());
            if !s.fields.is_empty() {
                let indices: BTreeSet<&str> = s.fields.iter().map(|(i, _)| i.as_str()).collect();
                let ids: BTreeSet<&str> = s.fields.iter().map(|(_, id)| id.as_str()).collect();
                m.insert(
                    "semantic_text".into(),
                    obj! { "field_count" => s.fields.len() as i64, "indices_count" => indices.len() as i64, "inference_id_count" => ids.len() as i64 },
                );
            }
            Value::Obj(m)
        })
        .collect();
    obj! { "available" => true, "enabled" => true, "models" => Value::Arr(models) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_task_types() {
        assert!(valid_id("my-endpoint_1"));
        assert!(valid_id("a"));
        assert!(!valid_id(".elser-2-elasticsearch"));
        assert!(!valid_id("Bad_ID"));
        assert!(!valid_id("x-"));
        assert_eq!(TaskType::parse(" Text_Embedding "), Some(TaskType::TextEmbedding));
        assert_eq!(TaskType::parse_or_error("bad").unwrap_err().reason(), "Unknown task_type [bad]");
        assert_eq!(resolve_task(TaskType::Any, None).unwrap_err().reason(), "model is missing required setting [task_type]");
        assert_eq!(
            resolve_task(TaskType::SparseEmbedding, Some(&Value::from("rerank"))).unwrap_err().reason(),
            "Cannot resolve conflicting task_type parameter in the request URL [sparse_embedding] and the request body [rerank]"
        );
    }
}
