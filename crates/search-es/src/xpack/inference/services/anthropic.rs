//! The `anthropic` service: completions through Anthropic's Messages API (`/v1/messages`), as
//! Elasticsearch 9.5.5's `AnthropicService` sends them (`x-api-key`, `anthropic-version:
//! 2023-06-01`, every input a user message, `max_tokens` from the task settings), plus the
//! streamed unified chat completion, translated to and from the Messages API. The host is
//! `api.anthropic.com`; the node's `roll.inference.anthropic.url` points it elsewhere.

use super::{Config, answer_json, base_url, missing_field, sse_data};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::http::{self, Call};
use crate::xpack::inference::results::Results;
use crate::xpack::inference::settings::Reader;
use crate::xpack::inference::{Endpoint, Request, TaskType};
use std::time::Duration;

const HOST: &str = "https://api.anthropic.com";

pub fn parse(cluster: &Cluster, _task: TaskType, cfg: &mut Config) -> EsResult<(Map, Map, Map)> {
    let mut r = Reader::new(std::mem::take(&mut cfg.service_settings), "service_settings");
    let model = r.required_str("model_id");
    let rate = r.rate_limit(50);
    r.check()?;
    let ts = task_settings(&mut cfg.task_settings, true)?;
    let mut errors = Vec::new();
    let secrets = super::api_key_secret(cluster, &mut r.map, &mut errors);
    if !errors.is_empty() {
        return Err(crate::xpack::inference::settings::validation(&errors));
    }
    cfg.service_settings = r.map;
    let mut ss = Map::new();
    ss.insert("model_id".into(), model.into());
    ss.insert("rate_limit".into(), rate);
    Ok((ss, ts, secrets))
}

/// `AnthropicChatCompletionTaskSettings`: `max_tokens` (required when creating), `temperature`,
/// `top_p`, `top_k`; printed as Elasticsearch prints them (its `top_k` under the `top_p` key).
pub fn task_settings(ts: &mut Map, create: bool) -> EsResult<Map> {
    let mut r = Reader::new(std::mem::take(ts), "task_settings");
    let max = if create { r.required_positive_int("max_tokens") } else { r.optional_positive_int("max_tokens") };
    let temperature = r.optional_float("temperature");
    let top_p = r.optional_float("top_p");
    let top_k = r.optional_int("top_k");
    *ts = r.finish()?;
    let mut out = Map::new();
    if let Some(m) = max {
        out.insert("max_tokens".into(), m.into());
    }
    if let Some(t) = temperature {
        out.insert("temperature".into(), t.into());
    }
    if let Some(p) = top_p {
        out.insert("top_p".into(), p.into());
    }
    if let Some(k) = top_k {
        out.insert("top_p".into(), k.into());
    }
    Ok(out)
}

pub fn update(ep: &mut Endpoint, ss: &mut Map) -> EsResult<()> {
    let mut r = Reader::new(std::mem::take(ss), "service_settings");
    let current = ep.service_settings.get("rate_limit").and_then(|v| v.get("requests_per_minute")).and_then(Value::as_i64).unwrap_or(50);
    let rate = r.rate_limit(current);
    *ss = r.finish()?;
    ep.service_settings.insert("rate_limit".into(), rate);
    Ok(())
}

async fn send(cluster: &Cluster, ep: &Endpoint, body: Value, timeout: Duration) -> EsResult<http::Answer> {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if let Some(k) = ep.secret(cluster, "api_key")? {
        headers.push(("x-api-key".into(), k));
    }
    headers.push(("anthropic-version".into(), "2023-06-01".into()));
    let url = format!("{}/v1/messages", base_url(cluster, "anthropic", HOST));
    let call = Call { inference_id: &ep.id, url: url.clone(), headers, body: body.to_json().into_bytes(), timeout, retry: |s| matches!(s, 429 | 500 | 503 | 529), per_minute: ep.requests_per_minute() };
    let answer = http::post(cluster, &call).await?;
    http::check(&ep.id, &url, &answer)?;
    Ok(answer)
}

fn effective(ep: &Endpoint, req: &Request) -> EsResult<Map> {
    let mut over = req.task_settings.clone();
    let parsed = task_settings(&mut over, false)?;
    Ok(super::merge(&ep.task_settings, &parsed))
}

/// The Messages request of a completion: every input a user message.
fn body(ep: &Endpoint, ts: &Map, inputs: &[String], stream: bool) -> Value {
    let mut b = Map::new();
    b.insert("messages".into(), Value::Arr(inputs.iter().map(|i| obj! { "role" => "user", "content" => i.clone() }).collect()));
    b.insert("model".into(), ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null));
    b.insert("max_tokens".into(), ts.get("max_tokens").cloned().unwrap_or(Value::from(-1)));
    for k in ["temperature", "top_p"] {
        if let Some(v) = ts.get(k) {
            b.insert(k.into(), v.clone());
        }
    }
    if stream {
        b.insert("stream".into(), true.into());
    }
    Value::Obj(b)
}

pub async fn infer(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Results> {
    if ep.task != TaskType::Completion {
        return Err(crate::xpack::inference::unsupported_task(ep.task, "anthropic"));
    }
    let ts = effective(ep, req)?;
    let answer = send(cluster, ep, body(ep, &ts, &req.input, false), req.timeout).await?;
    let v = answer_json("anthropic", &answer)?;
    let content = v.get("content").and_then(Value::as_array).ok_or_else(|| missing_field("content", "Anthropic chat completions"))?;
    // `AnthropicChatCompletionResponseEntity`: one result per text block.
    Ok(Results::Completion(content.iter().filter(|c| c.get("type").and_then(Value::as_str) == Some("text")).filter_map(|c| c.get("text").and_then(Value::as_str)).map(str::to_owned).collect()))
}

/// The text deltas of a Messages event stream.
fn text_deltas(raw: &[u8]) -> Vec<String> {
    sse_data(raw)
        .into_iter()
        .filter_map(|(_, d)| crate::json::parse(d.as_bytes()).ok().flatten())
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("content_block_delta"))
        .filter_map(|e| e.pointer(&["delta", "text"]).and_then(Value::as_str).map(str::to_owned))
        .collect()
}

pub async fn stream_completion(cluster: &Cluster, ep: &Endpoint, req: &Request) -> EsResult<Vec<Value>> {
    let ts = effective(ep, req)?;
    let answer = send(cluster, ep, body(ep, &ts, &req.input, true), req.timeout).await?;
    let deltas: Vec<Value> = text_deltas(&answer.body).into_iter().map(|t| obj! { "delta" => t }).collect();
    Ok(if deltas.is_empty() { vec![] } else { vec![obj! { "completion" => Value::Arr(deltas) }] })
}

/// A unified chat completion as a Messages request: `system` messages become `system`, the
/// rest keep their role; `max_completion_tokens` or the endpoint's `max_tokens`.
pub async fn stream_chat(cluster: &Cluster, ep: &Endpoint, body: &Value, timeout: Duration) -> EsResult<Vec<Value>> {
    let req = super::openai::unified_request(body)?;
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for m in req.get("messages").and_then(Value::as_array).into_iter().flatten() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = match m.get("content") {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""),
            _ => String::new(),
        };
        if role == "system" || role == "developer" {
            system.push(content);
        } else {
            messages.push(obj! { "role" => if role == "assistant" { "assistant" } else { "user" }, "content" => content });
        }
    }
    let mut b = Map::new();
    b.insert("messages".into(), Value::Arr(messages));
    b.insert("model".into(), req.get("model").cloned().or_else(|| ep.service_settings.get("model_id").cloned()).unwrap_or(Value::Null));
    let max = req.get("max_completion_tokens").cloned().or_else(|| ep.task_settings.get("max_tokens").cloned()).unwrap_or(Value::from(1024));
    b.insert("max_tokens".into(), max);
    if !system.is_empty() {
        b.insert("system".into(), system.join("\n").into());
    }
    for (from, to) in [("temperature", "temperature"), ("top_p", "top_p"), ("stop", "stop_sequences")] {
        if let Some(v) = req.get(from) {
            b.insert(to.into(), v.clone());
        }
    }
    b.insert("stream".into(), true.into());
    let answer = send(cluster, ep, Value::Obj(b), timeout).await?;
    let mut out = Vec::new();
    let mut id = Value::Null;
    let mut model = ep.service_settings.get("model_id").cloned().unwrap_or(Value::Null);
    let (mut input_tokens, mut output_tokens) = (0i64, 0i64);
    let chunk = |id: &Value, model: &Value, delta: Value, finish: Option<Value>| {
        let mut choice = Map::new();
        choice.insert("delta".into(), delta);
        if let Some(f) = finish {
            choice.insert("finish_reason".into(), f);
        }
        choice.insert("index".into(), 0.into());
        obj! { "id" => id.clone(), "choices" => Value::Arr(vec![Value::Obj(choice)]), "model" => model.clone(), "object" => "chat.completion.chunk" }
    };
    for (_, d) in sse_data(&answer.body) {
        let Some(e) = crate::json::parse(d.as_bytes()).ok().flatten() else { continue };
        match e.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                id = e.pointer(&["message", "id"]).cloned().unwrap_or(Value::Null);
                if let Some(m) = e.pointer(&["message", "model"]) {
                    model = m.clone();
                }
                input_tokens = e.pointer(&["message", "usage", "input_tokens"]).and_then(Value::as_i64).unwrap_or(0);
                out.push(chunk(&id, &model, obj! { "role" => "assistant" }, None));
            }
            Some("content_block_delta") => {
                if let Some(t) = e.pointer(&["delta", "text"]).and_then(Value::as_str) {
                    out.push(chunk(&id, &model, obj! { "content" => t }, None));
                }
            }
            Some("message_delta") => {
                output_tokens = e.pointer(&["usage", "output_tokens"]).and_then(Value::as_i64).unwrap_or(output_tokens);
                let finish = e.pointer(&["delta", "stop_reason"]).and_then(Value::as_str).map(|r| match r {
                    "end_turn" | "stop_sequence" => "stop",
                    "max_tokens" => "length",
                    "tool_use" => "tool_calls",
                    other => other,
                });
                let mut c = chunk(&id, &model, Value::object(), finish.map(Value::from));
                if let Some(m) = c.as_object_mut() {
                    m.insert(
                        "usage".into(),
                        obj! { "completion_tokens" => output_tokens, "prompt_tokens" => input_tokens, "total_tokens" => input_tokens + output_tokens },
                    );
                }
                out.push(c);
            }
            Some("error") => {
                let msg = e.pointer(&["error", "message"]).and_then(Value::as_str).unwrap_or("stream error").to_string();
                return Err(EsError::status(400, format!("Received an error response for request from inference entity id [{}]. Error message: [{msg}]", ep.id)));
            }
            _ => {}
        }
    }
    Ok(out)
}
