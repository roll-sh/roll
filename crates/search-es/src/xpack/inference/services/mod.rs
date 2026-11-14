//! The inference services: how each parses an endpoint's settings, prints them, and runs it.
//!
//! Every service follows Elasticsearch's `SenderService.parseRequestConfig`: `service_settings`
//! is required, `task_settings` optional, `chunking_settings` taken for the embedding tasks only
//! (`{}` when absent: the default sentence chunking), then the service reads its settings and
//! whatever is left anywhere is unknown to it. Creating or updating an endpoint then runs it
//! once ([`Service::validate`]), as the model validators do.

pub mod anthropic;
pub mod builtin;
pub mod cohere;
pub mod openai;
pub mod test;

use super::chunking::Chunking;
use super::results::Results;
use super::settings::{check_empty, java_string};
use super::{Endpoint, Request, TaskType, unsupported_task};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use std::time::Duration;

/// Node setting that turns on the `inference-service-test` plugin's services (YAML test
/// clusters only: `search-dev --yaml-test-cluster`).
pub const TEST_SERVICES: &str = "roll.inference.test_services";

/// A service roll can run endpoints of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    OpenAi,
    Cohere,
    Anthropic,
    /// The `elasticsearch` service (and its old name `elser`): models on ML nodes.
    Elasticsearch,
    Test(test::Kind),
}

/// The settings of a request, split the way every service reads them.
pub struct Config {
    pub service_settings: Map,
    pub task_settings: Map,
    pub chunking: Option<Chunking>,
    /// The rest of the top-level request (must end up empty).
    pub rest: Map,
}

impl Config {
    fn take(task: TaskType, mut config: Map) -> EsResult<Config> {
        let service_settings = match config.shift_remove("service_settings") {
            Some(Value::Obj(m)) => m,
            Some(Value::Null) | None => return Err(EsError::status(400, "Missing required field [service_settings]")),
            Some(other) => return Err(class_cast(&other)),
        };
        let task_settings = match config.shift_remove("task_settings") {
            Some(Value::Obj(m)) => m,
            Some(Value::Null) | None => Map::new(),
            Some(other) => return Err(class_cast(&other)),
        };
        let chunking = if task.chunks() { Some(Chunking::from_request(Some(&config.shift_remove("chunking_settings").unwrap_or_else(Value::object)))?) } else { None };
        Ok(Config { service_settings, task_settings, chunking, rest: config })
    }

    /// Fails on settings no service read.
    pub fn finish(&self, service: &str) -> EsResult<()> {
        check_empty(&self.rest, service)?;
        check_empty(&self.service_settings, service)?;
        check_empty(&self.task_settings, service)
    }
}

fn class_cast(v: &Value) -> EsError {
    EsError::java(500, "class_cast_exception", format!("class {} cannot be cast to class java.util.Map", java_class(v)))
}

fn java_class(v: &Value) -> &'static str {
    match v {
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(_) => "java.lang.Integer",
        Value::Arr(_) => "java.util.ArrayList",
        _ => "java.lang.Object",
    }
}

/// `DefaultSecretSettings.fromMap`: a required `api_key`, sealed.
pub fn api_key_secret(cluster: &Cluster, ss: &mut Map, errors: &mut Vec<String>) -> Map {
    let mut out = Map::new();
    match ss.shift_remove("api_key") {
        Some(Value::Str(k)) if !k.is_empty() => {
            out.insert("api_key".into(), crate::seal::seal_given(cluster, super::SECRET_KIND, &k).into());
        }
        Some(Value::Str(_)) => errors.push("[secret_settings] Invalid value empty string. [api_key] must be a non-empty string".into()),
        Some(other) => errors.push(format!("field [api_key] is not of the expected type. The value [{}] cannot be converted to a [String]", java_string(&other))),
        None => errors.push("[secret_settings] does not contain the required setting [api_key]".into()),
    }
    out
}

/// A new key from an update's `service_settings`, sealed; `None` keeps the current one.
pub fn updated_api_key(cluster: &Cluster, ss: &mut Map) -> EsResult<Option<String>> {
    match ss.shift_remove("api_key") {
        None => Ok(None),
        Some(Value::Str(k)) if !k.is_empty() => Ok(Some(crate::seal::seal_given(cluster, super::SECRET_KIND, &k))),
        Some(Value::Str(_)) => Err(super::settings::validation(&["[secret_settings] Invalid value empty string. [api_key] must be a non-empty string".into()])),
        Some(other) => Err(super::settings::validation(&[format!(
            "field [api_key] is not of the expected type. The value [{}] cannot be converted to a [String]",
            java_string(&other)
        )])),
    }
}

/// Merges an update's task settings into the current ones (`updatedTaskSettings`: given keys
/// replace, the rest stay).
pub fn merge(current: &Map, update: &Map) -> Map {
    let mut out = current.clone();
    for (k, v) in update {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// The `_services` configurations roll's services answer with (`InferenceServiceConfiguration`),
/// captured from Elasticsearch 9.5.5 and limited to what roll implements.
pub fn configurations(cluster: &Cluster, task: Option<TaskType>) -> Value {
    let mut all: Vec<Value> = crate::json::parse(include_bytes!("../services.json")).ok().flatten().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    if test_services_on(cluster) {
        all.extend(test::configurations());
        all.sort_by(|a, b| a.get("service").map(Value::to_es_string).cmp(&b.get("service").map(Value::to_es_string)));
    }
    let Some(task) = task else { return Value::Arr(all) };
    let mut out = Vec::new();
    for s in all {
        let types: Vec<String> = s.get("task_types").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
        if !types.iter().any(|t| t == task.name()) {
            continue;
        }
        // Only the settings that apply to the task.
        let mut s = s;
        if let Some(Value::Obj(c)) = s.as_object_mut().and_then(|m| m.get_mut("configurations")) {
            c.retain(|_, cfg| cfg.get("supported_task_types").and_then(Value::as_array).is_none_or(|t| t.iter().any(|x| x.as_str() == Some(task.name()))));
        }
        out.push(s);
    }
    Value::Arr(out)
}

pub fn test_services_on(cluster: &Cluster) -> bool {
    cluster.config.node_setting(TEST_SERVICES).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The validation call's failure, as `ServiceIntegrationValidator.wrapListenerForValidation`
/// reports it.
fn validation_failed(e: EsError) -> EsError {
    EsError::status(400, "Could not complete inference endpoint creation as validation call to service threw an exception.").caused_by(e)
}

impl Service {
    pub fn find(cluster: &Cluster, name: &str) -> Option<Service> {
        Some(match name {
            "openai" => Service::OpenAi,
            "cohere" => Service::Cohere,
            "anthropic" => Service::Anthropic,
            "elasticsearch" | "elser" => Service::Elasticsearch,
            other if test_services_on(cluster) => Service::Test(test::Kind::find(other)?),
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::OpenAi => "openai",
            Service::Cohere => "cohere",
            Service::Anthropic => "anthropic",
            Service::Elasticsearch => "elasticsearch",
            Service::Test(k) => k.name(),
        }
    }

    pub fn task_types(self) -> &'static [TaskType] {
        match self {
            Service::OpenAi => &[TaskType::TextEmbedding, TaskType::Completion, TaskType::ChatCompletion],
            Service::Cohere => &[TaskType::TextEmbedding, TaskType::Rerank, TaskType::Completion],
            Service::Anthropic => &[TaskType::Completion, TaskType::ChatCompletion],
            Service::Elasticsearch => &[TaskType::TextEmbedding, TaskType::SparseEmbedding, TaskType::Rerank],
            Service::Test(k) => k.task_types(),
        }
    }

    /// Reads a create request's settings into a new endpoint.
    pub fn parse(self, cluster: &Cluster, id: &str, task: TaskType, config: Map) -> EsResult<Endpoint> {
        if let Service::Test(k) = self {
            // The test plugin's services take what they know and ignore the rest.
            return k.parse(cluster, id, task, config);
        }
        let mut cfg = Config::take(task, config)?;
        if !self.task_types().contains(&task) {
            return Err(unsupported_task(task, self.name()));
        }
        let (service_settings, task_settings, secrets) = match self {
            Service::OpenAi => openai::parse(cluster, task, &mut cfg)?,
            Service::Cohere => cohere::parse(cluster, task, &mut cfg)?,
            Service::Anthropic => anthropic::parse(cluster, task, &mut cfg)?,
            Service::Elasticsearch => builtin::parse(task, &mut cfg)?,
            Service::Test(_) => unreachable!(),
        };
        cfg.finish(self.name())?;
        Ok(Endpoint { id: id.to_string(), task, service: self.name().to_string(), service_settings, task_settings, chunking: cfg.chunking, secrets })
    }

    /// An update request (`service_settings`, `task_settings`, `chunking_settings`) applied.
    pub fn update(self, cluster: &Cluster, current: &Endpoint, mut body: Map) -> EsResult<Endpoint> {
        let mut ss = body.shift_remove("service_settings").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let ts = body.shift_remove("task_settings").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let mut ep = current.clone();
        if let Some(c) = body.shift_remove("chunking_settings") {
            if !current.task.chunks() {
                return Err(EsError::status(400, format!("Chunking settings are not supported for the task type [{}]", current.task.name())));
            }
            ep.chunking = Some(Chunking::from_request(Some(&c))?);
        }
        if let Some(k) = updated_api_key(cluster, &mut ss)? {
            ep.secrets.insert("api_key".into(), k.into());
        }
        match self {
            Service::OpenAi => openai::update(&mut ep, &mut ss)?,
            Service::Cohere => cohere::update(&mut ep, &mut ss)?,
            Service::Anthropic => anthropic::update(&mut ep, &mut ss)?,
            Service::Elasticsearch => return Err(EsError::status(400, "Updating the elasticsearch service's endpoints needs ML nodes, which roll doesn't run")),
            Service::Test(k) => return k.update(&mut ep, ss, ts).map(|_| ep),
        }
        check_empty(&ss, self.name())?;
        let mut ts_reader = ts.clone();
        let parsed_ts = match self {
            Service::OpenAi => openai::task_settings(&mut ts_reader)?,
            Service::Cohere => cohere::task_settings(current.task, &mut ts_reader)?,
            Service::Anthropic => anthropic::task_settings(&mut ts_reader, false)?,
            _ => Map::new(),
        };
        check_empty(&ts_reader, self.name())?;
        // The merged settings print in their `toXContent` order.
        let mut merged = merge(&current.task_settings, &parsed_ts);
        ep.task_settings = match self {
            Service::OpenAi => openai::task_settings(&mut merged)?,
            Service::Cohere => cohere::task_settings(current.task, &mut merged)?,
            _ => merged,
        };
        Ok(ep)
    }

    /// The service settings as the API prints them (`getFilteredXContentObject`).
    pub fn exposed(self, ep: &Endpoint) -> Map {
        match self {
            Service::Test(k) => k.exposed(ep),
            _ => {
                let mut m = ep.service_settings.clone();
                m.shift_remove("dimensions_set_by_user");
                m
            }
        }
    }

    /// Runs the endpoint once, as creating or updating it does, and records what that taught
    /// (a text embedding model's dimensions and similarity).
    pub async fn validate(self, cluster: &Cluster, ep: &mut Endpoint, timeout: Duration) -> EsResult<()> {
        let mut req = Request::new(vec!["how big".into()], timeout);
        let run = match ep.task {
            TaskType::Rerank => {
                req.query = Some("test query".into());
                self.infer(cluster, ep, &req).await
            }
            TaskType::ChatCompletion => {
                let body = obj! { "messages" => crate::arr![obj! { "content" => "how big", "role" => "user" }] };
                self.stream_chat(cluster, ep, &body, timeout).await.map(|_| Results::Completion(vec![]))
            }
            _ => self.infer(cluster, ep, &req).await,
        };
        let results = run.map_err(validation_failed)?;
        if matches!(ep.task, TaskType::TextEmbedding | TaskType::Embedding) {
            let Some(size) = results.first_embedding_size() else {
                return Err(EsError::status(
                    400,
                    format!("Validation call did not return expected results type. Expected a result of type [DenseEmbeddingResults] got [{}]", result_class(&results)),
                ));
            };
            let by_user = ep.service_settings.get("dimensions_set_by_user").and_then(Value::as_bool) == Some(true);
            if let Some(d) = ep.service_settings.get("dimensions").and_then(Value::as_i64)
                && by_user
                && d != size as i64
            {
                return Err(EsError::status(
                    400,
                    format!(
                        "The retrieved embeddings size [{size}] does not match the size specified in the settings [{d}]. Please recreate the [{}] configuration with the correct dimensions",
                        ep.id
                    ),
                ));
            }
            self.embedding_details(ep, size);
        }
        Ok(())
    }

    /// `updateModelWithEmbeddingDetails`.
    fn embedding_details(self, ep: &mut Endpoint, size: usize) {
        match self {
            Service::OpenAi => openai::embedding_details(ep, size),
            Service::Cohere => cohere::embedding_details(ep, size),
            Service::Test(k) => k.embedding_details(ep, size),
            _ => {}
        }
    }

    /// One inference call.
    pub async fn infer(self, cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
        match self {
            Service::OpenAi => openai::infer(cluster, ep, req).await,
            Service::Cohere => cohere::infer(cluster, ep, req).await,
            Service::Anthropic => anthropic::infer(cluster, ep, req).await,
            Service::Elasticsearch => Err(builtin::infer_error(ep, req)),
            Service::Test(k) => k.infer(ep, req),
        }
    }

    /// A streamed completion's events (`{"completion":[{"delta":…}]}` each).
    pub async fn stream_completion(self, cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Vec<Value>> {
        match self {
            Service::OpenAi => openai::stream_completion(cluster, ep, req).await,
            Service::Anthropic => anthropic::stream_completion(cluster, ep, req).await,
            Service::Test(k) => k.stream_completion(ep, req),
            _ => Err(EsError::status(400, format!("Streaming is not allowed for service [{}] and task [{}]", self.name(), ep.task.name()))),
        }
    }

    /// A streamed chat completion (the unified API)'s chunks.
    pub async fn stream_chat(self, cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Vec<Value>> {
        match self {
            Service::OpenAi => openai::stream_chat(cluster, ep, body, timeout).await,
            Service::Anthropic => anthropic::stream_chat(cluster, ep, body, timeout).await,
            Service::Test(k) => k.stream_chat(ep, body),
            _ => Err(EsError::status(400, format!("Streaming is not allowed for service [{}] and task [{}]", self.name(), ep.task.name()))),
        }
    }
}

fn result_class(r: &Results) -> &'static str {
    match r {
        Results::Floats(_) => "DenseEmbeddingFloatResults",
        Results::Bytes(_) => "DenseEmbeddingByteResults",
        Results::Bits(_) => "DenseEmbeddingBitResults",
        Results::Sparse(_) => "SparseEmbeddingResults",
        Results::Rerank(_) => "RankedDocsResults",
        Results::Completion(_) => "ChatCompletionResults",
    }
}

/// The base URL of a service whose host Elasticsearch fixes (`api.cohere.ai`,
/// `api.anthropic.com`): the node may point it elsewhere (`roll.inference.<service>.url`, for a
/// gateway or a test mock); tenants can't.
pub fn base_url(cluster: &Cluster, service: &str, default: &str) -> String {
    cluster.config.node_setting(&format!("roll.inference.{service}.url")).unwrap_or(default).trim_end_matches('/').to_string()
}

/// Request headers every service sends.
pub fn json_headers() -> Vec<(String, String)> {
    vec![("Content-Type".into(), "application/json;charset=utf-8".into())]
}

/// Parses a successful answer's JSON or fails the way Elasticsearch's response entities do
/// when a field is missing.
pub fn answer_json(service: &str, answer: &super::http::Answer) -> EsResult<Value> {
    answer.json().ok_or_else(|| EsError::java(500, "illegal_state_exception", format!("Failed to parse the [{service}] response: it isn't JSON")))
}

/// `Failed to find required field [x] in … response`.
pub fn missing_field(field: &str, what: &str) -> EsError {
    EsError::java(500, "illegal_state_exception", format!("Failed to find required field [{field}] in {what} response"))
}

/// The events of a server-sent event stream as `(event name, data)`, `[DONE]` dropped.
pub fn sse_data(body: &[u8]) -> Vec<(Option<String>, String)> {
    let text = String::from_utf8_lossy(body);
    let mut out = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let mut event = None;
        let mut data: Vec<&str> = Vec::new();
        for line in block.lines() {
            let line = line.trim_start_matches('\u{feff}');
            if let Some(e) = line.strip_prefix("event:") {
                event = Some(e.trim().to_string());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push(d.strip_prefix(' ').unwrap_or(d));
            }
        }
        let data = data.join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        out.push((event, data));
    }
    out
}
