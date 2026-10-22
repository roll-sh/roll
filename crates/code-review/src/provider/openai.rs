//! Any OpenAI-compatible chat completions endpoint (OpenAI, OpenRouter, a self-hosted model
//! behind vLLM, …): `POST {base_url}/chat/completions` with a strict `json_schema` response
//! format. There's no default model: the operator or the owner names one.

use super::{Provider, ProviderError, Request, Response, RetryPolicy, Stop, Usage, post_json};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct OpenAiCompatible {
    pub base_url: String,
    api_key: String,
    pub model: String,
    pub retry: RetryPolicy,
    http: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatible").field("base_url", &self.base_url).field("model", &self.model).finish()
    }
}

impl OpenAiCompatible {
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<OpenAiCompatible, ProviderError> {
        if model.trim().is_empty() {
            return Err(ProviderError::Config("an OpenAI-compatible provider needs a model".into()));
        }
        let base_url = base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(ProviderError::Config("base_url must be an http(s) URL".into()));
        }
        Ok(OpenAiCompatible {
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.trim().to_string(),
            retry: RetryPolicy::default(),
            http: super::default_http(),
        })
    }

    /// Send requests through `http` (the server builds one that refuses private addresses for
    /// bring-your-own endpoints).
    pub fn with_http(mut self, http: reqwest::Client) -> OpenAiCompatible {
        self.http = http;
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> OpenAiCompatible {
        self.retry = retry;
        self
    }

    pub fn body(&self, req: &Request) -> Value {
        let system: String = req.system.iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join("\n\n");
        json!({
            "model": if req.model.is_empty() { &self.model } else { &req.model },
            "max_tokens": req.max_tokens,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": req.user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": req.schema_name, "strict": true, "schema": req.schema },
            },
        })
    }
}

pub fn parse_response(v: &Value) -> Result<Response, ProviderError> {
    let bad = |m: &str| ProviderError::Decode(m.to_string(), "openai_compatible");
    let choice = v.pointer("/choices/0").ok_or_else(|| bad("no choices"))?;
    let msg = choice.get("message").cloned().unwrap_or(Value::Null);
    let u = v.get("usage").cloned().unwrap_or(Value::Null);
    let cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
    let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let usage = Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        cache_write_tokens: 0,
        cache_read_tokens: cached,
    };
    let model = v.get("model").and_then(Value::as_str).unwrap_or_default().to_string();
    if let Some(r) = msg.get("refusal").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        return Ok(Response { json: None, stop: Stop::Refusal { category: Some(r.chars().take(200).collect()) }, usage, model });
    }
    let stop = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("stop") | None => Stop::EndTurn,
        Some("length") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal { category: Some("content_filter".into()) },
        Some(o) => Stop::Other(o.to_string()),
    };
    let json = if stop == Stop::EndTurn {
        let text = msg.get("content").and_then(Value::as_str).ok_or_else(|| bad("no message content"))?;
        Some(serde_json::from_str(text.trim()).map_err(|e| bad(&format!("the answer isn't the requested JSON: {e}")))?)
    } else {
        None
    };
    Ok(Response { json, stop, usage, model })
}

#[async_trait]
impl Provider for OpenAiCompatible {
    fn name(&self) -> &'static str {
        "openai_compatible"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let headers = [("authorization", format!("Bearer {}", self.api_key))];
        let v = post_json(&self.http, "openai_compatible", &self.retry, &format!("{}/chat/completions", self.base_url), &headers, &self.body(req)).await?;
        parse_response(&v)
    }

    /// Chat completions has no token counter: the engine estimates from bytes.
    async fn count_tokens(&self, _req: &Request) -> Result<Option<u64>, ProviderError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_parse() {
        assert!(OpenAiCompatible::new("https://x", "k", "").is_err());
        assert!(OpenAiCompatible::new("ftp://x", "k", "m").is_err());
        let ok = json!({"model": "m", "choices": [{"finish_reason": "stop", "message": {"content": "{\"a\":1}"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 60}}});
        let r = parse_response(&ok).unwrap();
        assert_eq!(r.json.unwrap()["a"], 1);
        assert_eq!(r.usage, Usage { input_tokens: 40, output_tokens: 7, cache_write_tokens: 0, cache_read_tokens: 60 });
        let refused = json!({"choices": [{"finish_reason": "stop", "message": {"content": null, "refusal": "no"}}]});
        assert!(matches!(parse_response(&refused).unwrap().stop, Stop::Refusal { .. }));
        let cut = json!({"choices": [{"finish_reason": "length", "message": {"content": "{"}}]});
        assert_eq!(parse_response(&cut).unwrap().stop, Stop::MaxTokens);
    }
}
