//! The model behind roll's reviewer. A [`Provider`] takes one [`Request`] (a system prompt in
//! blocks, one user message, a JSON schema the answer must follow, an effort level) and returns
//! the parsed JSON plus why it stopped and what it used.
//!
//! Both implementations share [`RetryPolicy`]: 429, 529, 5xx and connection errors are retried
//! with jittered backoff, honouring `retry-after`; 400/401/403/404 are not retried.

pub mod anthropic;
pub mod openai;

pub use anthropic::Anthropic;
pub use openai::OpenAiCompatible;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// How hard the model thinks. Every request sets it explicitly (Claude Opus 5.5 and Haiku 5.5
/// default to `medium`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
    pub fn parse(s: &str) -> Option<Effort> {
        Some(match s {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            "xhigh" => Effort::Xhigh,
            "max" => Effort::Max,
            _ => return None,
        })
    }
}

/// One block of the system prompt. Blocks are sent in order; the last one marked `cache` ends
/// the cached prefix, so everything up to it must be byte-identical between reviews of a repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemBlock {
    pub text: String,
    pub cache: bool,
}

#[derive(Clone, Debug)]
pub struct Request {
    /// Empty: the provider's configured model.
    pub model: String,
    pub system: Vec<SystemBlock>,
    pub user: String,
    /// The answer's JSON schema (every object `additionalProperties: false`, all keys required).
    pub schema: Value,
    pub schema_name: &'static str,
    pub effort: Effort,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    /// The answer was cut off: the JSON is incomplete.
    MaxTokens,
    /// The model (and any fallback) declined.
    Refusal { category: Option<String> },
    Other(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    /// The answer, when it stopped normally and parsed.
    pub json: Option<Value>,
    pub stop: Stop,
    pub usage: Usage,
    /// The model that answered (a fallback model when the requested one declined).
    pub model: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// A request the provider rejected; retrying won't help (bad key, bad request, …).
    #[error("{provider} answered {status}: {message}")]
    Rejected { provider: &'static str, status: u16, message: String },
    /// Still failing after every retry (rate limited, overloaded, unreachable).
    #[error("{provider} kept failing ({attempts} attempts): {last}")]
    Exhausted { provider: &'static str, attempts: u32, last: String },
    /// The provider answered something that isn't what its API documents.
    #[error("{provider} sent an unexpected response: {0}", provider = .1)]
    Decode(String, &'static str),
    #[error("{0}")]
    Config(String),
}

/// A model provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// `anthropic` or `openai_compatible`.
    fn name(&self) -> &'static str;
    /// The default model.
    fn model(&self) -> &str;
    async fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
    /// Input tokens `req` would use, when the provider can count them.
    async fn count_tokens(&self, req: &Request) -> Result<Option<u64>, ProviderError>;
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Attempts in all (the first try included).
    pub attempts: u32,
    pub base: Duration,
    /// The longest wait between attempts, `retry-after` included.
    pub max_delay: Duration,
    pub timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { attempts: 5, base: Duration::from_millis(500), max_delay: Duration::from_secs(60), timeout: Duration::from_secs(600) }
    }
}

impl RetryPolicy {
    /// How long to wait before attempt `n + 1` (n ≥ 1): `retry-after` when the provider gave
    /// one, else exponential backoff with full jitter.
    pub fn delay(&self, n: u32, retry_after: Option<Duration>) -> Duration {
        if let Some(d) = retry_after {
            return d.min(self.max_delay);
        }
        let exp = self.base.saturating_mul(1u32 << n.min(16).saturating_sub(1));
        let cap = exp.min(self.max_delay);
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let frac = u32::from_le_bytes(b) as f64 / u32::MAX as f64;
        // Half fixed, half jitter: never zero, never synchronised.
        cap.mul_f64(0.5 + 0.5 * frac)
    }
}

/// The most of a provider's response roll reads: a model answer is far smaller, and an endpoint
/// that streams more (a bring-your-own `base_url` can point anywhere) is cut off, not buffered.
pub const MAX_RESPONSE_BYTES: usize = 16 << 20;

/// The HTTP client providers use by default: redirects aren't followed (a provider never needs
/// one, and following them would let a `base_url` bounce requests elsewhere).
pub fn default_http() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap_or_default()
}

/// The response body, up to [`MAX_RESPONSE_BYTES`] (`None` past that).
async fn read_bounded(mut resp: reqwest::Response) -> Result<Option<String>, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Is this HTTP status worth retrying?
pub fn retryable(status: u16) -> bool {
    status == 408 || status == 409 || status == 429 || status == 529 || status >= 500
}

/// `retry-after` in seconds (the only form either API sends), or `retry-after-ms`.
pub fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = h.get("retry-after-ms").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok()) {
        return Some(Duration::from_secs_f64((ms / 1000.0).max(0.0)));
    }
    h.get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok()).map(|s| Duration::from_secs_f64(s.max(0.0)))
}

/// POST `body` to `url` with `headers`, retrying per `policy`. Returns the response headers and
/// body of the first non-retryable answer.
pub(crate) async fn post_json(
    http: &reqwest::Client,
    provider: &'static str,
    policy: &RetryPolicy,
    url: &str,
    headers: &[(&str, String)],
    body: &Value,
) -> Result<Value, ProviderError> {
    let mut last = String::new();
    for attempt in 1..=policy.attempts.max(1) {
        let mut rb = http.post(url).timeout(policy.timeout).json(body);
        for (k, v) in headers {
            rb = rb.header(*k, v);
        }
        let wait = match rb.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let after = retry_after(resp.headers());
                let text = match read_bounded(resp).await {
                    Ok(Some(t)) => t,
                    Ok(None) => return Err(ProviderError::Decode(format!("the response is larger than {MAX_RESPONSE_BYTES} bytes"), provider)),
                    Err(_) => String::new(),
                };
                if (200..300).contains(&status) {
                    return serde_json::from_str(&text).map_err(|e| ProviderError::Decode(format!("{e}"), provider));
                }
                let message = error_message(&text);
                if !retryable(status) {
                    return Err(ProviderError::Rejected { provider, status, message });
                }
                last = format!("{status}: {message}");
                after
            }
            Err(e) => {
                last = if e.is_timeout() { "timed out".into() } else { format!("connection failed: {e}") };
                None
            }
        };
        if attempt < policy.attempts {
            let d = policy.delay(attempt, wait);
            tracing::debug!(provider, attempt, delay_ms = d.as_millis() as u64, error = %last, "retrying model request");
            tokio::time::sleep(d).await;
        }
    }
    Err(ProviderError::Exhausted { provider, attempts: policy.attempts.max(1), last })
}

/// The `message` of a JSON error body (`{"error": {"message": …}}`), else the body itself.
fn error_message(text: &str) -> String {
    let v: Option<Value> = serde_json::from_str(text).ok();
    let m = v
        .as_ref()
        .and_then(|v| v.pointer("/error/message").or_else(|| v.get("message")).or_else(|| v.get("error")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| text.to_string());
    m.chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_and_retry_after() {
        let p = RetryPolicy { attempts: 5, base: Duration::from_millis(100), max_delay: Duration::from_secs(2), timeout: Duration::from_secs(1) };
        for n in 1..6 {
            let d = p.delay(n, None);
            let cap = Duration::from_millis(100 * (1 << (n - 1))).min(Duration::from_secs(2));
            assert!(d >= cap / 2 && d <= cap, "{n}: {d:?}");
        }
        assert_eq!(p.delay(1, Some(Duration::from_secs(1))), Duration::from_secs(1));
        assert_eq!(p.delay(1, Some(Duration::from_secs(90))), Duration::from_secs(2));
        assert!(retryable(429) && retryable(529) && retryable(500) && retryable(503));
        assert!(!retryable(400) && !retryable(401) && !retryable(403) && !retryable(404));
        assert_eq!(error_message(r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#), "bad");
        assert_eq!(error_message("plain"), "plain");
    }
}
