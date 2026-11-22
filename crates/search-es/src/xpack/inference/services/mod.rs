//! The inference services: how each parses an endpoint's settings, prints them, and runs it.
//!
//! Every service follows Elasticsearch's `SenderService.parseRequestConfig`: `service_settings`
//! is required, `task_settings` optional, `chunking_settings` taken for the embedding tasks only
//! (`{}` when absent: the default sentence chunking), then the service reads its settings and
//! whatever is left anywhere is unknown to it. Creating or updating an endpoint then runs it
//! once ([`Service::validate`]), as the model validators do.

pub mod anthropic;
pub mod azureopenai;
pub mod bedrock;
pub mod builtin;
pub mod cohere;
pub mod custom;
pub mod deepseek;
pub mod google;
pub mod huggingface;
pub mod jinaai;
pub mod mistral;
pub mod oai;
pub mod openai;
pub mod test;
pub mod voyageai;

use super::chunking::Chunking;
use super::results::Results;
use super::stream::Streaming;
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
    AzureOpenAi,
    Bedrock,
    VertexAi,
    AiStudio,
    Mistral,
    VoyageAi,
    JinaAi,
    DeepSeek,
    HuggingFace,
    Custom,
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
            "azureopenai" => Service::AzureOpenAi,
            "amazonbedrock" => Service::Bedrock,
            "googlevertexai" => Service::VertexAi,
            "googleaistudio" => Service::AiStudio,
            "mistral" => Service::Mistral,
            "voyageai" => Service::VoyageAi,
            "jinaai" => Service::JinaAi,
            "deepseek" => Service::DeepSeek,
            "hugging_face" => Service::HuggingFace,
            "custom" => Service::Custom,
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
            Service::AzureOpenAi => "azureopenai",
            Service::Bedrock => "amazonbedrock",
            Service::VertexAi => "googlevertexai",
            Service::AiStudio => "googleaistudio",
            Service::Mistral => "mistral",
            Service::VoyageAi => "voyageai",
            Service::JinaAi => "jinaai",
            Service::DeepSeek => "deepseek",
            Service::HuggingFace => "hugging_face",
            Service::Custom => "custom",
            Service::Elasticsearch => "elasticsearch",
            Service::Test(k) => k.name(),
        }
    }

    pub fn task_types(self) -> &'static [TaskType] {
        match self {
            Service::OpenAi => &[TaskType::TextEmbedding, TaskType::Completion, TaskType::ChatCompletion],
            Service::Cohere => &[TaskType::TextEmbedding, TaskType::Rerank, TaskType::Completion],
            Service::Anthropic => &[TaskType::Completion, TaskType::ChatCompletion],
            Service::AzureOpenAi | Service::Mistral => &[TaskType::TextEmbedding, TaskType::Completion, TaskType::ChatCompletion],
            Service::Bedrock => &[TaskType::TextEmbedding, TaskType::Completion],
            Service::VertexAi => &[TaskType::TextEmbedding, TaskType::Rerank, TaskType::Completion, TaskType::ChatCompletion],
            Service::AiStudio => &[TaskType::TextEmbedding, TaskType::Completion],
            Service::VoyageAi | Service::JinaAi => &[TaskType::TextEmbedding, TaskType::Rerank],
            Service::DeepSeek => &[TaskType::Completion, TaskType::ChatCompletion],
            Service::HuggingFace => &[TaskType::TextEmbedding, TaskType::SparseEmbedding, TaskType::Rerank, TaskType::Completion, TaskType::ChatCompletion],
            Service::Custom => &[TaskType::TextEmbedding, TaskType::SparseEmbedding, TaskType::Rerank, TaskType::Completion],
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
            Service::AzureOpenAi => azureopenai::parse(cluster, task, &mut cfg)?,
            Service::Bedrock => bedrock::parse(cluster, task, &mut cfg)?,
            Service::VertexAi => google::parse_vertex(cluster, task, &mut cfg)?,
            Service::AiStudio => google::parse_studio(cluster, task, &mut cfg)?,
            Service::Mistral => mistral::parse(cluster, task, &mut cfg)?,
            Service::VoyageAi => voyageai::parse(cluster, task, &mut cfg)?,
            Service::JinaAi => jinaai::parse(cluster, task, &mut cfg)?,
            Service::DeepSeek => deepseek::parse(cluster, task, &mut cfg)?,
            Service::HuggingFace => huggingface::parse(cluster, task, &mut cfg)?,
            Service::Custom => custom::parse(cluster, task, &mut cfg)?,
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
        let own_secrets = matches!(self, Service::AzureOpenAi | Service::Bedrock | Service::VertexAi | Service::Custom | Service::Test(_));
        if !own_secrets && let Some(k) = updated_api_key(cluster, &mut ss)? {
            ep.secrets.insert("api_key".into(), k.into());
        }
        match self {
            Service::OpenAi => openai::update(&mut ep, &mut ss)?,
            Service::Cohere => cohere::update(&mut ep, &mut ss)?,
            Service::Anthropic => anthropic::update(&mut ep, &mut ss)?,
            Service::AzureOpenAi => azureopenai::update(cluster, &mut ep, &mut ss)?,
            Service::Bedrock => bedrock::update(cluster, &mut ep, &mut ss)?,
            Service::VertexAi => google::update_vertex(cluster, &mut ep, &mut ss)?,
            Service::Custom => custom::update(cluster, &mut ep, &mut ss)?,
            Service::AiStudio => update_rate(&mut ep, &mut ss, 360)?,
            Service::Mistral => update_rate(&mut ep, &mut ss, 240)?,
            Service::VoyageAi | Service::JinaAi => update_rate(&mut ep, &mut ss, 2000)?,
            Service::HuggingFace => update_rate(&mut ep, &mut ss, 3000)?,
            Service::DeepSeek => {}
            Service::Elasticsearch => return Err(EsError::status(400, "Updating the elasticsearch service's endpoints needs ML nodes, which roll doesn't run")),
            Service::Test(k) => return k.update(&mut ep, ss, ts).map(|_| ep),
        }
        check_empty(&ss, self.name())?;
        let mut ts_reader = ts.clone();
        let parsed_ts = match self {
            Service::OpenAi => openai::task_settings(&mut ts_reader)?,
            Service::Cohere => cohere::task_settings(current.task, &mut ts_reader)?,
            Service::Anthropic => anthropic::task_settings(&mut ts_reader, false)?,
            Service::AzureOpenAi => azureopenai::task_settings(&mut ts_reader)?,
            Service::Bedrock => bedrock::task_settings(current.task, &mut ts_reader)?,
            Service::VertexAi => google::vertex_task_settings(current.task, &mut ts_reader)?,
            Service::VoyageAi => voyageai::task_settings(current.task, &mut ts_reader)?,
            Service::JinaAi => jinaai::task_settings(current.task, &mut ts_reader)?,
            Service::HuggingFace => huggingface::task_settings(current.task, &mut ts_reader)?,
            Service::Custom => custom::task_settings(&mut ts_reader)?,
            _ => Map::new(),
        };
        check_empty(&ts_reader, self.name())?;
        // The merged settings print in their `toXContent` order.
        let mut merged = merge(&current.task_settings, &parsed_ts);
        ep.task_settings = match self {
            Service::OpenAi => openai::task_settings(&mut merged)?,
            Service::Cohere => cohere::task_settings(current.task, &mut merged)?,
            Service::AzureOpenAi => azureopenai::task_settings(&mut merged)?,
            Service::Bedrock => bedrock::task_settings(current.task, &mut merged)?,
            Service::VertexAi => google::vertex_task_settings(current.task, &mut merged)?,
            Service::VoyageAi => voyageai::task_settings(current.task, &mut merged)?,
            Service::JinaAi => jinaai::task_settings(current.task, &mut merged)?,
            Service::HuggingFace => huggingface::task_settings(current.task, &mut merged)?,
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
        // The services after Anthropic validate an embedding endpoint as an ingest would run it.
        if ep.task == TaskType::TextEmbedding && !matches!(self, Service::OpenAi | Service::Cohere | Service::Anthropic | Service::Elasticsearch | Service::Test(_)) {
            req.input_type = Some("internal_ingest".into());
        }
        let run = match ep.task {
            TaskType::Rerank => {
                req.query = Some("test query".into());
                // The custom service's validator asks for the top document only.
                if self == Service::Custom {
                    req.top_n = Some(1);
                }
                self.infer(cluster, ep, &req).await
            }
            TaskType::ChatCompletion => {
                let body = obj! { "messages" => crate::arr![obj! { "content" => "how big", "role" => "user" }] };
                // Elasticsearch's validation passes once the stream opened (recorded: an endpoint
                // whose stream fails midway is created); dropping it closes the upstream request.
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
            Service::AzureOpenAi => azureopenai::embedding_details(ep, size),
            Service::Bedrock => bedrock::embedding_details(ep, size),
            Service::VertexAi => google::vertex_embedding_details(ep, size),
            Service::AiStudio => google::studio_embedding_details(ep, size),
            Service::Mistral => mistral::embedding_details(ep, size),
            Service::VoyageAi => voyageai::embedding_details(ep, size),
            Service::JinaAi => jinaai::embedding_details(ep, size),
            Service::HuggingFace => huggingface::embedding_details(ep, size),
            Service::Custom => custom::embedding_details(ep, size),
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
            Service::AzureOpenAi => azureopenai::infer(cluster, ep, req).await,
            Service::Bedrock => bedrock::infer(cluster, ep, req).await,
            Service::VertexAi => google::infer_vertex(cluster, ep, req).await,
            Service::AiStudio => google::infer_studio(cluster, ep, req).await,
            Service::Mistral => mistral::infer(cluster, ep, req).await,
            Service::VoyageAi => voyageai::infer(cluster, ep, req).await,
            Service::JinaAi => jinaai::infer(cluster, ep, req).await,
            Service::DeepSeek => deepseek::infer(cluster, ep, req).await,
            Service::HuggingFace => huggingface::infer(cluster, ep, req).await,
            Service::Custom => custom::infer(cluster, ep, req).await,
            Service::Elasticsearch => Err(builtin::infer_error(ep, req)),
            Service::Test(k) => k.infer(ep, req),
        }
    }

    /// A streamed completion (`{"completion":[{"delta":…}]}` events), its upstream answer open.
    pub async fn stream_completion(self, cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Streaming> {
        match self {
            Service::OpenAi => openai::stream_completion(cluster, ep, req).await,
            Service::Anthropic => anthropic::stream_completion(cluster, ep, req).await,
            Service::AzureOpenAi => azureopenai::stream_completion(cluster, ep, req).await,
            Service::VertexAi => google::stream_completion_vertex(cluster, ep, req).await,
            Service::AiStudio => google::stream_completion_studio(cluster, ep, req).await,
            Service::Mistral => mistral::stream_completion(cluster, ep, req).await,
            Service::DeepSeek => deepseek::stream_completion(cluster, ep, req).await,
            Service::HuggingFace => huggingface::stream_completion(cluster, ep, req).await,
            Service::Test(k) => k.stream_completion(ep, req),
            _ => Err(EsError::status(400, format!("Streaming is not allowed for service [{}] and task [{}]", self.name(), ep.task.name()))),
        }
    }

    /// A streamed chat completion (the unified API's chunks), its upstream answer open.
    pub async fn stream_chat(self, cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Streaming> {
        match self {
            Service::OpenAi => openai::stream_chat(cluster, ep, body, timeout).await,
            Service::Anthropic => anthropic::stream_chat(cluster, ep, body, timeout).await,
            Service::AzureOpenAi => azureopenai::stream_chat(cluster, ep, body, timeout).await,
            Service::VertexAi => google::stream_chat_vertex(cluster, ep, body, timeout).await,
            Service::Mistral => mistral::stream_chat(cluster, ep, body, timeout).await,
            Service::DeepSeek => deepseek::stream_chat(cluster, ep, body, timeout).await,
            Service::HuggingFace => huggingface::stream_chat(cluster, ep, body, timeout).await,
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

/// `m`'s keys in `order` (each service's `toXContent` order), anything else after them.
pub fn ordered(mut m: Map, order: &[&str]) -> Map {
    let mut out = Map::new();
    for k in order {
        if let Some(v) = m.shift_remove(*k) {
            out.insert((*k).to_string(), v);
        }
    }
    out.extend(m);
    out
}

/// A secret string setting (`extractRequiredSecureString`, `extractOptionalSecureString`),
/// sealed; errors are collected.
pub fn take_secret(cluster: &Cluster, ss: &mut Map, key: &str, scope: &str, required: bool, errors: &mut Vec<String>) -> Option<String> {
    match ss.shift_remove(key) {
        Some(Value::Str(k)) if !k.is_empty() => Some(crate::seal::seal_given(cluster, super::SECRET_KIND, &k)),
        Some(Value::Str(_)) => {
            errors.push(format!("[{scope}] Invalid value empty string. [{key}] must be a non-empty string"));
            None
        }
        Some(other) => {
            errors.push(format!("field [{key}] is not of the expected type. The value [{}] cannot be converted to a [String]", java_string(&other)));
            None
        }
        None => {
            if required {
                errors.push(format!("[{scope}] does not contain the required setting [{key}]"));
            }
            None
        }
    }
}

/// The current `rate_limit.requests_per_minute` of an endpoint.
pub fn current_rate(ep: &Endpoint, default: i64) -> i64 {
    ep.requests_per_minute().unwrap_or(default)
}

/// An update that may change only the rate limit.
pub fn update_rate(ep: &mut Endpoint, ss: &mut Map, default: i64) -> EsResult<()> {
    let mut r = super::settings::Reader::new(std::mem::take(ss), "service_settings");
    let rate = r.rate_limit(current_rate(ep, default));
    *ss = r.finish()?;
    ep.service_settings.insert("rate_limit".into(), rate);
    Ok(())
}

/// The inputs as a JSON array of strings.
pub fn strings(input: &[String]) -> Value {
    Value::Arr(input.iter().map(|s| Value::from(s.as_str())).collect())
}

/// A request's `input_type` in lower case, else the task settings' one.
pub fn input_type(req: &Request, ts: &Map) -> Option<String> {
    req.input_type.clone().or_else(|| ts.get("input_type").and_then(Value::as_str).map(str::to_owned)).map(|s| s.to_ascii_lowercase())
}

/// Whether an input type is one roll sends itself (`internal_ingest`, `internal_search`).
pub fn internal(it: &str) -> bool {
    it.starts_with("internal_")
}

/// Records a text embedding model's learned dimensions and default similarity, printed in the
/// service's order.
pub fn learned(ep: &mut Endpoint, size: usize, similarity: &str, order: &[&str]) {
    let mut ss = std::mem::take(&mut ep.service_settings);
    ss.entry("similarity".to_string()).or_insert_with(|| similarity.into());
    ss.insert("dimensions".into(), (size as i64).into());
    ep.service_settings = ordered(ss, order);
}

/// Sends a JSON POST and checks the status the OpenAI way; the answer's JSON.
pub async fn post_json(cluster: &Cluster, ep: &Endpoint, service: &str, url: String, headers: Vec<(String, String)>, body: &Value, timeout: Duration) -> EsResult<Value> {
    let call = super::http::Call::json(&ep.id, url.clone(), headers, body, timeout, ep.requests_per_minute());
    let answer = super::http::post(cluster, &call).await?;
    super::http::check(&ep.id, &url, &answer)?;
    answer_json(service, &answer)
}

/// `Authorization: Bearer <api_key>` after the given headers.
pub fn bearer(cluster: &Cluster, ep: &Endpoint, mut headers: Vec<(String, String)>) -> EsResult<Vec<(String, String)>> {
    if let Some(k) = ep.secret(cluster, "api_key")? {
        headers.push(("Authorization".into(), format!("Bearer {k}")));
    }
    Ok(headers)
}

/// `Content-Type: application/json;charset=utf-8`, the API key as a bearer token, then
/// `Request-Source: unspecified:elasticsearch` when `source`.
pub fn json_bearer(cluster: &Cluster, ep: &Endpoint, source: bool) -> EsResult<Vec<(String, String)>> {
    let mut h = bearer(cluster, ep, json_headers())?;
    if source {
        h.push(("Request-Source".into(), "unspecified:elasticsearch".into()));
    }
    Ok(h)
}

/// `"x only accepts 1 input"`, the completion services' check.
pub fn one_input<'a>(req: &'a Request, what: &str) -> EsResult<&'a str> {
    match req.input.as_slice() {
        [one] => Ok(one),
        _ => Err(EsError::status(400, format!("{what} only accepts 1 input"))),
    }
}

/// The rerank results of `[{index, <score>, <text>}]`, indexes checked against the inputs.
pub fn ranked(items: &[Value], inputs: usize, score: &str, text: impl Fn(&Value) -> Option<String>, what: &str) -> EsResult<Results> {
    let mut out = Vec::with_capacity(items.len());
    for r in items {
        let index = r.get("index").and_then(Value::as_i64).filter(|i| *i >= 0 && (*i as usize) < inputs).ok_or_else(|| missing_field("index", what))?;
        let s = r.get(score).and_then(Value::as_f64).ok_or_else(|| missing_field(score, what))?;
        out.push(super::results::Ranked { index: index as usize, score: s as f32, text: text(r) });
    }
    Ok(Results::Rerank(out))
}

/// A list of numeric vectors as float embeddings, `expected` of them.
pub fn float_vectors(list: &[Value], expected: usize, what: &str) -> EsResult<Results> {
    if list.len() != expected {
        return Err(EsError::java(500, "illegal_state_exception", format!("The {what} response holds [{}] embeddings for [{expected}] inputs", list.len())));
    }
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        let nums = v.as_array().ok_or_else(|| missing_field("embedding", what))?;
        out.push(nums.iter().map(|n| n.as_f64().map(|f| f as f32)).collect::<Option<Vec<f32>>>().ok_or_else(|| missing_field("embedding", what))?);
    }
    Ok(Results::Floats(out))
}

/// Byte (`int8`) or bit (`binary`) embeddings of numeric vectors.
pub fn byte_vectors(list: &[Value], expected: usize, bits: bool, what: &str) -> EsResult<Results> {
    if list.len() != expected {
        return Err(EsError::java(500, "illegal_state_exception", format!("The {what} response holds [{}] embeddings for [{expected}] inputs", list.len())));
    }
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        let nums = v.as_array().ok_or_else(|| missing_field("embedding", what))?;
        out.push(nums.iter().map(|n| n.as_f64().map(|f| f.clamp(-128.0, 127.0) as i8)).collect::<Option<Vec<i8>>>().ok_or_else(|| missing_field("embedding", what))?);
    }
    Ok(if bits { Results::Bits(out) } else { Results::Bytes(out) })
}

/// A URL path segment encoded as `URIBuilder.setPathSegments` encodes it (RFC 3986 `pchar`s
/// stay as they are).
pub fn path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
