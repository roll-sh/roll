//! The `openai` service: text embeddings (`/v1/embeddings`), completions and streamed chat
//! completions (`/v1/chat/completions`), as Elasticsearch 9.5.5's `OpenAiService` sends and
//! reads them. `service_settings.url` points it at any OpenAI-compatible server (vLLM, Ollama,
//! LiteLLM, roll's model gateway), fetched through [`crate::xpack::inference::http`].
//!
//! Not supported: OAuth2 client credentials (`client_secret`, `token_url`; an API key is
//! required) and the multimodal `embedding` task type.

use super::{Config, answer_json, json_headers, missing_field, sse_data};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::{Reader, validation};
use crate::xpack::inference::{Endpoint, Request, TaskType};
use base64::Engine;
use std::time::Duration;

const EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";
const CHAT_URL: &str = "https://api.openai.com/v1/chat/completions";

/// Reads `service_settings`, `task_settings` and the API key of a new endpoint.
pub fn parse(cluster: &Cluster, task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let mut ss = Map::new();
    match task {
        TaskType::TextEmbedding => {
            let url = r.optional_url("url");
            let org = r.optional_str("organization_id");
            let similarity = r.similarity();
            let max_input = r.optional_positive_int("max_input_tokens");
            let dims = r.optional_positive_int("dimensions");
            let model = r.required_str("model_id");
            let rate = r.rate_limit(3000);
            r.check()?;
            ss.insert("model_id".into(), model.unwrap_or_default().into());
            put_opt(&mut ss, "url", url);
            put_opt(&mut ss, "organization_id", org);
            put_opt(&mut ss, "similarity", similarity);
            if let Some(d) = dims {
                ss.insert("dimensions".into(), d.into());
            }
            if let Some(m) = max_input {
                ss.insert("max_input_tokens".into(), m.into());
            }
            ss.insert("rate_limit".into(), rate);
            ss.insert("dimensions_set_by_user".into(), dims.is_some().into());
        }
        _ => {
            let model = r.required_str("model_id");
            let org = r.optional_str("organization_id");
            let url = r.optional_url("url");
            let max_input = r.optional_positive_int("max_input_tokens");
            let rate = r.rate_limit(500);
            r.check()?;
            ss.insert("model_id".into(), model.unwrap_or_default().into());
            put_opt(&mut ss, "url", url);
            put_opt(&mut ss, "organization_id", org);
            if let Some(m) = max_input {
                ss.insert("max_input_tokens".into(), m.into());
            }
            ss.insert("rate_limit".into(), rate);
        }
    }
    let ts = task_settings(&mut cfg.task_settings)?;
    let mut rest = r.map;
    let secrets = match rest.shift_remove("api_key") {
        Some(Value::Str(k)) if !k.is_empty() => {
            let mut m = Map::new();
            m.insert("api_key".into(), crate::seal::seal(cluster, crate::xpack::inference::SECRET_KIND, &k).into());
            m
        }
        Some(Value::Str(_)) => return Err(validation(&["[secret_settings] Invalid value empty string. [api_key] must be a non-empty string".into()])),
        _ if rest.contains_key("client_secret") => {
            return Err(validation(&["[service_settings] OAuth2 client credentials ([client_secret]) aren't supported by roll; set [api_key]".into()]));
        }
        _ => return Err(validation(&["[service_settings] must have exactly one field of [api_key, client_secret] set".into()])),
    };
    cfg.service_settings = rest;
    Ok((ss, ts, secrets))
}

fn put_opt(m: &mut Map, k: &str, v: Option<String>) {
    if let Some(v) = v {
        m.insert(k.into(), v.into());
    }
}

/// `OpenAiTaskSettings.fromMap`: `user`, `headers` (in that order).
pub fn task_settings(ts: &mut Map) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let user = r.optional_str("user");
    let headers = r.optional_string_map("headers");
    let rest = r.finish()?;
    *ts = rest;
    let mut out = Map::new();
    put_opt(&mut out, "user", user);
    if let Some(h) = headers {
        out.insert("headers".into(), Value::Obj(h));
    }
    Ok(out)
}

/// What an update may change (`updateServiceSettings`): the organization, `max_input_tokens`,
/// the rate limit.
pub fn update(ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut r = Reader::new(std::mem::take(ss), "service_settings");
    let org = r.optional_str("organization_id");
    let max_input = r.optional_positive_int("max_input_tokens");
    let current = ep.service_settings.get("rate_limit").and_then(|v| v.get("requests_per_minute")).and_then(Value::as_i64).unwrap_or(3000);
    let rate = r.rate_limit(current);
    *ss = r.finish()?;
    let old = std::mem::take(&mut ep.service_settings);
    for (k, v) in old {
        let v = match k.as_str() {
            "organization_id" => org.clone().map(Value::from).unwrap_or(v),
            "max_input_tokens" => max_input.map(Value::from).unwrap_or(v),
            "rate_limit" => rate.clone(),
            _ => v,
        };
        ep.service_settings.insert(k, v);
    }
    // A setting the endpoint didn't have goes where `toXContent` prints it.
    if !ep.service_settings.contains_key("organization_id")
        && let Some(o) = org
    {
        let pos = ep.service_settings.get_index_of("url").or_else(|| ep.service_settings.get_index_of("model_id")).map_or(0, |i| i + 1);
        ep.service_settings.shift_insert(pos, "organization_id".into(), o.into());
    }
    if !ep.service_settings.contains_key("max_input_tokens")
        && let Some(m) = max_input
    {
        let pos = ep.service_settings.get_index_of("rate_limit").unwrap_or(ep.service_settings.len());
        ep.service_settings.shift_insert(pos, "max_input_tokens".into(), m.into());
    }
    Ok(())
}

/// After validation: the model's dimensions, and `dot_product` (OpenAI's embeddings are unit
/// vectors) unless a similarity was given.
pub fn embedding_details(ep: &mut Endpoint, size: usize) {
    let ss = &mut ep.service_settings;
    if !ss.contains_key("similarity") {
        let pos = ss.get_index_of("organization_id").or_else(|| ss.get_index_of("url")).or_else(|| ss.get_index_of("model_id")).map_or(0, |i| i + 1);
        ss.shift_insert(pos, "similarity".into(), "dot_product".into());
    }
    match ss.get_index_of("dimensions") {
        Some(_) => {
            ss.insert("dimensions".into(), (size as i64).into());
        }
        None => {
            let pos = ss.get_index_of("similarity").map_or(ss.len(), |i| i + 1);
            ss.shift_insert(pos, "dimensions".into(), (size as i64).into());
        }
    }
}

/// The request's task settings over the endpoint's.
fn effective_task_settings(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(&mut over)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

fn headers(cluster: &Cluster, ep: &Endpoint, ts: &Map) -> EsResult<Vec<(String, String)>> {
    let mut h = json_headers();
    if let Some(Value::Obj(extra)) = ts.get("headers") {
        for (k, v) in extra {
            h.push((k.clone(), v.to_es_string()));
        }
    }
    if let Some(org) = ep.service_settings.get("organization_id").and_then(Value::as_str) {
        h.push(("OpenAI-Organization".into(), org.to_string()));
    }
    if let Some(key) = ep.secret(cluster, "api_key")? {
        h.push(("Authorization".into(), format!("Bearer {key}")));
    }
    Ok(h)
}

fn url(ep: &Endpoint, default: &str) -> String {
    ep.service_settings.get("url").and_then(Value::as_str).unwrap_or(default).to_string()
}

async fn send(cluster: &Cluster, ep: &Endpoint, url: String, headers: Vec<(String, String)>, body: Value, timeout: Duration) -> EsResult<http::Answer> {
    let call = Call { inference_id: &ep.id, url: url.clone(), headers, body: body.to_json().into_bytes(), timeout, retry: http::retry_server_errors };
    let answer = http::post(cluster, &call).await?;
    http::check(&ep.id, &url, &answer)?;
    Ok(answer)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    let ts = effective_task_settings(ep, req)?;
    let headers = headers(cluster, ep, &ts)?;
    match ep.task {
        TaskType::TextEmbedding => {
            let mut body = Map::new();
            body.insert("input".into(), Value::Arr(req.input.iter().map(|s| Value::from(s.as_str())).collect()));
            body.insert("model".into(), ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null));
            if let Some(u) = ts.get("user") {
                body.insert("user".into(), u.clone());
            }
            if ep.service_settings.get("dimensions_set_by_user").and_then(Value::as_bool) == Some(true)
                && let Some(d) = ep.service_settings.get("dimensions")
            {
                body.insert("dimensions".into(), d.clone());
            }
            body.insert("encoding_format".into(), "base64".into());
            let answer = send(cluster, ep, url(ep, EMBEDDINGS_URL), headers, Value::Obj(body), req.timeout).await?;
            embeddings(&answer_json("openai", &answer)?, req.input.len())
        }
        TaskType::Completion => {
            if req.input.len() != 1 {
                return Err(EsError::status(400, "OpenAI chat completions only accepts 1 input"));
            }
            let body = chat_body(&ts, messages_of(&req.input[0]), &Map::new(), ep, false);
            let answer = send(cluster, ep, url(ep, CHAT_URL), headers, body, req.timeout).await?;
            let v = answer_json("openai", &answer)?;
            let content = v
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.pointer(&["message", "content"]))
                .and_then(Value::as_str)
                .ok_or_else(|| missing_field("choices", "OpenAI chat completions"))?;
            Ok(Results::Completion(vec![content.to_string()]))
        }
        other => Err(crate::xpack::inference::unsupported_task(other, "openai")),
    }
}

/// The embeddings of an answer: `data[].embedding`, a base64 string of little-endian floats
/// (what `encoding_format: base64` returns) or an array of numbers (servers that ignore it).
pub fn embeddings(v: &Value, expected: usize) -> EsResult<Results> {
    let data = v.get("data").and_then(Value::as_array).ok_or_else(|| missing_field("data", "OpenAI embeddings"))?;
    let mut out = Vec::with_capacity(data.len());
    for d in data {
        let e = d.get("embedding").ok_or_else(|| missing_field("embedding", "OpenAI embeddings"))?;
        out.push(match e {
            Value::Str(s) => {
                let raw = base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| EsError::java(500, "illegal_state_exception", "Failed to decode the base64 embedding in the OpenAI embeddings response"))?;
                if raw.len() % 4 != 0 {
                    return Err(EsError::java(500, "illegal_state_exception", "A base64 embedding in the OpenAI embeddings response isn't a whole number of floats"));
                }
                raw.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
            }
            Value::Arr(a) => a.iter().map(|x| x.as_f64().map(|f| f as f32)).collect::<Option<Vec<f32>>>().ok_or_else(|| missing_field("embedding", "OpenAI embeddings"))?,
            _ => return Err(missing_field("embedding", "OpenAI embeddings")),
        });
    }
    if out.len() != expected {
        return Err(EsError::java(500, "illegal_state_exception", format!("The OpenAI embeddings response holds [{}] embeddings for [{expected}] inputs", out.len())));
    }
    Ok(Results::Floats(out))
}

fn messages_of(input: &str) -> Value {
    crate::arr![obj! { "content" => input, "role" => "user" }]
}

/// The chat completions request (`OpenAiUnifiedChatCompletionRequestEntity`): `extra` holds a
/// unified request's options (`stop`, `temperature`, `tool_choice`, `tools`, `top_p`,
/// `max_completion_tokens`), in that order.
fn chat_body(ts: &Map, messages: Value, extra: &Map, ep: &Endpoint, stream: bool) -> Value {
    let mut body = Map::new();
    body.insert("messages".into(), messages);
    for k in ["stop", "temperature", "tool_choice", "tools", "top_p", "max_completion_tokens"] {
        if let Some(v) = extra.get(k) {
            if matches!(v, Value::Arr(a) if a.is_empty()) {
                continue;
            }
            body.insert(k.into(), v.clone());
        }
    }
    body.insert("model".into(), extra.get("model").cloned().or_else(|| ep.service_settings.get("model_id").cloned()).unwrap_or(Value::Null));
    body.insert("n".into(), 1.into());
    body.insert("stream".into(), stream.into());
    if stream {
        body.insert("stream_options".into(), obj! { "include_usage" => true });
    }
    if let Some(u) = ts.get("user").and_then(Value::as_str).filter(|u| !u.is_empty()) {
        body.insert("user".into(), u.into());
    }
    Value::Obj(body)
}

/// A streamed completion: the deltas of the answer's chunks, in one event (roll reads the
/// stream whole before answering).
pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Vec<Value>> {
    if req.input.len() != 1 {
        return Err(EsError::status(400, "OpenAI chat completions only accepts 1 input"));
    }
    let ts = effective_task_settings(ep, req)?;
    let body = chat_body(&ts, messages_of(&req.input[0]), &Map::new(), ep, true);
    let answer = send(cluster, ep, url(ep, CHAT_URL), headers(cluster, ep, &ts)?, body, req.timeout).await?;
    let mut deltas = Vec::new();
    for (_, data) in sse_data(&answer.body) {
        let Some(chunk) = crate::json::parse(data.as_bytes()).ok().flatten() else { continue };
        for c in chunk.get("choices").and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = c.pointer(&["delta", "content"]).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                deltas.push(obj! { "delta" => t });
            }
        }
    }
    Ok(if deltas.is_empty() { vec![] } else { vec![obj! { "completion" => Value::Arr(deltas) }] })
}

/// The unified chat completion request's fields (`UnifiedCompletionRequest`).
const UNIFIED_FIELDS: &[&str] = &["messages", "model", "max_completion_tokens", "stop", "temperature", "tool_choice", "tools", "top_p", "reasoning"];

/// Reads and checks a unified chat completion request.
pub fn unified_request(body: &Value) -> EsResult<Map> {
    let Some(m) = body.as_object() else { return Err(EsError::x_content_parse_bare("[chat_completion_request] expected an object")) };
    for k in m.keys() {
        if !UNIFIED_FIELDS.contains(&k.as_str()) {
            return Err(EsError::x_content_parse_bare(format!("[chat_completion_request] unknown field [{k}]")));
        }
    }
    match m.get("messages") {
        Some(Value::Arr(a)) if !a.is_empty() => {}
        Some(Value::Arr(_)) => return Err(EsError::action_request_validation(&["Field [messages] cannot be an empty array".into()])),
        _ => return Err(EsError::x_content_parse_bare("Required [messages]")),
    }
    Ok(m.clone())
}

/// A message as the entity writes it: `content`, `role`, `name`, `tool_call_id`, `tool_calls`.
fn message(v: &Value) -> Value {
    let mut out = Map::new();
    for k in ["content", "role", "name", "tool_call_id", "tool_calls"] {
        if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
            out.insert(k.into(), x.clone());
        }
    }
    Value::Obj(out)
}

/// A streamed chat completion (`_inference/chat_completion/{id}/_stream`): the upstream's
/// chunks as `StreamingUnifiedChatCompletionResults` prints them.
pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Vec<Value>> {
    let req = unified_request(body)?;
    let messages = Value::Arr(req.get("messages").and_then(Value::as_array).into_iter().flatten().map(message).collect());
    let ts = ep.task_settings.clone();
    let body = chat_body(&ts, messages, &req, ep, true);
    let answer = send(cluster, ep, url(ep, CHAT_URL), headers(cluster, ep, &ts)?, body, timeout).await?;
    let mut out = Vec::new();
    for (_, data) in sse_data(&answer.body) {
        if let Some(chunk) = crate::json::parse(data.as_bytes()).ok().flatten() {
            out.push(unified_chunk(&chunk));
        }
    }
    Ok(out)
}

/// One chunk in `ChatCompletionChunk.toXContent`'s shape.
pub fn unified_chunk(c: &Value) -> Value {
    let mut out = Map::new();
    if let Some(id) = c.get("id") {
        out.insert("id".into(), id.clone());
    }
    let choices: Vec<Value> = c
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|ch| {
            let mut m = Map::new();
            let mut delta = Map::new();
            if let Some(d) = ch.get("delta") {
                for k in ["content", "refusal", "role", "tool_calls"] {
                    if let Some(x) = d.get(k).filter(|x| !x.is_null()) {
                        delta.insert(k.into(), x.clone());
                    }
                }
            }
            m.insert("delta".into(), Value::Obj(delta));
            if let Some(f) = ch.get("finish_reason").filter(|x| !x.is_null()) {
                m.insert("finish_reason".into(), f.clone());
            }
            m.insert("index".into(), ch.get("index").cloned().unwrap_or(Value::from(0)));
            Value::Obj(m)
        })
        .collect();
    out.insert("choices".into(), Value::Arr(choices));
    for k in ["model", "object"] {
        if let Some(x) = c.get(k) {
            out.insert(k.into(), x.clone());
        }
    }
    if let Some(u) = c.get("usage").filter(|u| !u.is_null()) {
        out.insert(
            "usage".into(),
            obj! {
                "completion_tokens" => u.get("completion_tokens").cloned().unwrap_or(Value::from(0)),
                "prompt_tokens" => u.get("prompt_tokens").cloned().unwrap_or(Value::from(0)),
                "total_tokens" => u.get("total_tokens").cloned().unwrap_or(Value::from(0)),
            },
        );
    }
    Value::Obj(out)
}
