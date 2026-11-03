//! Anthropic's Messages API over raw HTTP (`POST /v1/messages`). There's no official Rust SDK,
//! so the request rules are spelled out here:
//!
//! - Depth is `output_config.effort`, always sent. Thinking is never configured: on Claude
//!   Opus 5.5 it's always on, and `{type: "disabled"}` or `budget_tokens` is a 400.
//! - The answer's shape is `output_config.format = {type: "json_schema", schema}`. No forced
//!   `tool_choice` (a 400 on current models) and no assistant prefill (a 400).
//! - The system prompt is a list of text blocks; `cache_control: {type: "ephemeral"}` goes on
//!   the last stable one, so reviews of the same repo reuse the cached prefix.
//! - `fallbacks: "default"` (beta `server-side-fallback-2026-07-01`) lets the API re-run a
//!   declined request on another model, for the models that support it.
//! - `stop_reason` is checked before the content is read: `refusal` and `max_tokens` mean
//!   there's no complete answer.

use super::{Provider, ProviderError, Request, Response, RetryPolicy, Stop, Usage, post_json};
use async_trait::async_trait;
use serde_json::{Value, json};

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
/// The cheaper model for `fast` reviews and the `auto` classifier.
pub const DEFAULT_SECONDARY_MODEL: &str = "claude-haiku-5-5";
pub const API_VERSION: &str = "2023-06-01";
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

/// Models that accept `fallbacks: "default"` (Claude Haiku 5.5 has no server-side fallback).
const FALLBACK_MODELS: [&str; 4] = ["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"];

#[derive(Clone)]
pub struct Anthropic {
    pub base_url: String,
    api_key: String,
    pub model: String,
    /// Send `fallbacks: "default"` (on models that support it).
    pub fallbacks: bool,
    pub retry: RetryPolicy,
    http: reqwest::Client,
}

impl std::fmt::Debug for Anthropic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Anthropic").field("base_url", &self.base_url).field("model", &self.model).finish()
    }
}

impl Anthropic {
    pub fn new(api_key: &str, model: Option<&str>) -> Anthropic {
        Anthropic {
            base_url: "https://api.anthropic.com".into(),
            api_key: api_key.to_string(),
            model: model.filter(|m| !m.is_empty()).unwrap_or(DEFAULT_MODEL).to_string(),
            fallbacks: true,
            retry: RetryPolicy::default(),
            http: super::default_http(),
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Anthropic {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    /// Send requests through `http` (the server builds one that refuses private addresses for
    /// bring-your-own endpoints).
    pub fn with_http(mut self, http: reqwest::Client) -> Anthropic {
        self.http = http;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Anthropic {
        self.retry = retry;
        self
    }

    fn model_for<'a>(&'a self, req: &'a Request) -> &'a str {
        if req.model.is_empty() { &self.model } else { &req.model }
    }

    fn uses_fallbacks(&self, model: &str) -> bool {
        self.fallbacks && FALLBACK_MODELS.contains(&model)
    }

    fn headers(&self, beta: bool) -> Vec<(&'static str, String)> {
        let mut h = vec![("x-api-key", self.api_key.clone()), ("anthropic-version", API_VERSION.to_string())];
        if beta {
            h.push(("anthropic-beta", FALLBACK_BETA.to_string()));
        }
        h
    }

    /// The `system` blocks and `messages` (shared with `count_tokens`).
    fn prompt(req: &Request) -> (Value, Value) {
        let system: Vec<Value> = req
            .system
            .iter()
            .map(|b| {
                let mut v = json!({ "type": "text", "text": b.text });
                if b.cache {
                    v["cache_control"] = json!({ "type": "ephemeral" });
                }
                v
            })
            .collect();
        let messages = json!([{ "role": "user", "content": [{ "type": "text", "text": req.user }] }]);
        (Value::Array(system), messages)
    }

    /// The request body for `POST /v1/messages`.
    pub fn body(&self, req: &Request) -> Value {
        let model = self.model_for(req);
        let (system, messages) = Self::prompt(req);
        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "system": system,
            "messages": messages,
            "output_config": {
                "effort": req.effort.as_str(),
                "format": { "type": "json_schema", "schema": req.schema },
            },
        });
        if self.uses_fallbacks(model) {
            body["fallbacks"] = json!("default");
        }
        body
    }
}

/// Read a Messages API response.
pub fn parse_response(v: &Value) -> Result<Response, ProviderError> {
    let bad = |m: &str| ProviderError::Decode(m.to_string(), "anthropic");
    let stop = match v.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") | Some("stop_sequence") => Stop::EndTurn,
        Some("max_tokens") => Stop::MaxTokens,
        Some("refusal") => Stop::Refusal { category: v.pointer("/stop_details/category").and_then(Value::as_str).map(str::to_string) },
        Some(other) => Stop::Other(other.to_string()),
        None => return Err(bad("no stop_reason")),
    };
    let u = v.get("usage").cloned().unwrap_or(Value::Null);
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let usage = Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_write_tokens: n("cache_creation_input_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
    };
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    let content = v.get("content").and_then(Value::as_array).ok_or_else(|| bad("no content"))?;
    let json = if stop == Stop::EndTurn {
        // The answer is the text after the last switch to a fallback model; thinking blocks
        // (empty or summarized) and fallback markers aren't part of it.
        let start = content.iter().rposition(|b| b.get("type").and_then(Value::as_str) == Some("fallback")).map_or(0, |i| i + 1);
        let text: String = content[start..]
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect();
        Some(serde_json::from_str(text.trim()).map_err(|e| bad(&format!("the answer isn't the requested JSON: {e}")))?)
    } else {
        None
    };
    Ok(Response { json, stop, usage, model })
}

#[async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = self.body(req);
        let beta = body.get("fallbacks").is_some();
        let v = post_json(&self.http, "anthropic", &self.retry, &format!("{}/v1/messages", self.base_url), &self.headers(beta), &body).await?;
        parse_response(&v)
    }

    async fn count_tokens(&self, req: &Request) -> Result<Option<u64>, ProviderError> {
        let (system, messages) = Self::prompt(req);
        let body = json!({ "model": self.model_for(req), "system": system, "messages": messages });
        let v = post_json(&self.http, "anthropic", &self.retry, &format!("{}/v1/messages/count_tokens", self.base_url), &self.headers(false), &body).await?;
        Ok(v.get("input_tokens").and_then(Value::as_u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Effort, SystemBlock};

    fn req(model: &str) -> Request {
        Request {
            model: model.into(),
            system: vec![SystemBlock { text: "a".into(), cache: false }, SystemBlock { text: "b".into(), cache: true }],
            user: "diff".into(),
            schema: json!({"type":"object","additionalProperties":false,"required":[],"properties":{}}),
            schema_name: "review",
            effort: Effort::High,
            max_tokens: 16000,
        }
    }

    #[test]
    fn body_follows_the_rules() {
        let a = Anthropic::new("sk-ant-secret", None);
        let b = a.body(&req(""));
        assert_eq!(b["model"], DEFAULT_MODEL);
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("thinking").is_none() && b.get("tool_choice").is_none() && b.get("tools").is_none());
        assert!(b["system"][0].get("cache_control").is_none());
        assert_eq!(b["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"].as_array().unwrap().len(), 1);
        assert_eq!(b["messages"][0]["role"], "user");
        // Haiku has no server-side fallback; and it can be turned off.
        assert!(a.body(&req(DEFAULT_SECONDARY_MODEL)).get("fallbacks").is_none());
        let off = Anthropic { fallbacks: false, ..a.clone() };
        assert!(off.body(&req("")).get("fallbacks").is_none());
        assert!(!format!("{a:?}").contains("sk-ant-secret"));
    }

    #[test]
    fn responses() {
        let ok = json!({
            "model": "claude-opus-5-5", "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": "{\"a\": 1}"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 1000}
        });
        let r = parse_response(&ok).unwrap();
        assert_eq!(r.json.unwrap()["a"], 1);
        assert_eq!(r.usage, Usage { input_tokens: 10, output_tokens: 5, cache_write_tokens: 100, cache_read_tokens: 1000 });
        // Only the text after the last fallback switch.
        let fb = json!({"model": "claude-opus-4-8", "stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"partial"}, {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-4-8"}},
            {"type": "text", "text": "{\"b\": 2}"}], "usage": {}});
        assert_eq!(parse_response(&fb).unwrap().json.unwrap()["b"], 2);
        let refused = json!({"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}, "content": [], "usage": {}});
        assert_eq!(parse_response(&refused).unwrap().stop, Stop::Refusal { category: Some("cyber".into()) });
        let cut = json!({"stop_reason": "max_tokens", "content": [{"type": "text", "text": "{\"a\":"}], "usage": {}});
        let r = parse_response(&cut).unwrap();
        assert_eq!(r.stop, Stop::MaxTokens);
        assert!(r.json.is_none());
        let garbage = json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "not json"}], "usage": {}});
        assert!(matches!(parse_response(&garbage), Err(ProviderError::Decode(..))));
    }
}
