//! Requests to inference services.
//!
//! An endpoint's URL is chosen by the tenant (OpenAI's `url`, and every service's host when the
//! node overrides it), so roll's rule for such URLs applies (AGENTS.md): a client that resolves
//! names to public addresses only ([`roll_net::PublicOnly`]), refuses private IP literals and
//! `localhost` ([`roll_net::literal_private_host`]), follows no redirects, uses no proxy and reads
//! at most [`MAX_RESPONSE`] bytes. The node setting [`ALLOW_PRIVATE`] lifts the address check, for
//! test clusters whose endpoints call a mock on the same host; it is off unless set.
//!
//! Like Elasticsearch's `RetryingHttpSender`, a request that fails to connect or gets a
//! retryable status (the service's handler decides: 429, 500, 503) is sent again, here at most
//! twice, within the request's timeout. Answers are untrusted data: services parse them with
//! [`crate::json::parse`] and check their shape.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use std::time::{Duration, Instant};

/// Lets inference endpoints reach private addresses (loopback, private ranges, `localhost`).
pub const ALLOW_PRIVATE: &str = "roll.inference.allow_private_hosts";

/// The most an answer may hold (a batch of 3072-dimension embeddings is a few MiB).
pub const MAX_RESPONSE: usize = 32 * 1024 * 1024;

const RETRIES: usize = 2;

/// A request to send.
pub struct Call<'a> {
    pub inference_id: &'a str,
    pub url: String,
    /// Header names and values; secrets are opened by the caller just for this request.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    /// Which statuses to send again.
    pub retry: fn(u16) -> bool,
    /// The endpoint's `requests_per_minute` ([`super::rate`]).
    pub per_minute: Option<i64>,
}

/// What came back.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Option<crate::json::Value> {
        crate::json::parse(&self.body).ok().flatten()
    }
}

/// `Inference endpoint [id]: request timed out` (504).
pub fn timed_out(id: &str) -> EsError {
    EsError::status(504, format!("Inference endpoint [{id}]: request timed out"))
}

fn failed(id: &str, detail: String) -> EsError {
    EsError::new(500, "exception", format!("Failed to send request from inference entity id [{id}]")).caused_by(EsError::java(500, "connect_exception", detail))
}

fn innermost(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(x) = src {
        s = x.to_string();
        src = x.source();
    }
    s
}

/// Sends a POST, retrying as described above.
pub async fn post(cluster: &Cluster, call: &Call<'_>) -> EsResult<Answer> {
    let allow_private = cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let url = reqwest::Url::parse(&call.url).map_err(|_| EsError::status(400, format!("Invalid host [{}], please check that the URL is correct.", call.url)))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(EsError::status(400, format!("Invalid host [{}], please check that the URL is correct.", call.url)));
    }
    if !allow_private && roll_net::literal_private_host(&url) {
        return Err(EsError::status(
            400,
            format!(
                "host [{}] is a private address, which inference endpoints may not connect to unless [{ALLOW_PRIVATE}] is set",
                url.host_str().unwrap_or_default()
            ),
        ));
    }
    let mut b = reqwest::Client::builder()
        .connect_timeout(call.timeout.min(Duration::from_secs(10)))
        .timeout(call.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if !allow_private {
        b = b.dns_resolver(std::sync::Arc::new(roll_net::PublicOnly));
    }
    let client = b.build().map_err(|e| EsError::illegal_state(e.to_string()))?;
    let deadline = Instant::now() + call.timeout;
    super::rate::acquire(&cluster.config.name, call.inference_id, call.per_minute, call.timeout).await?;
    let mut attempt = 0;
    loop {
        let mut rb = client.post(url.clone());
        for (k, v) in &call.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let sent = rb.body(call.body.clone()).send().await;
        // A retryable answer, or a connection error.
        let outcome = match sent {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let mut body = Vec::new();
                loop {
                    match resp.chunk().await {
                        Ok(Some(c)) => {
                            if body.len() + c.len() > MAX_RESPONSE {
                                return Err(EsError::status(400, format!("the answer of inference endpoint [{}] exceeds [{MAX_RESPONSE}] bytes", call.inference_id)));
                            }
                            body.extend_from_slice(&c);
                        }
                        Ok(None) => break,
                        Err(e) if e.is_timeout() => return Err(timed_out(call.inference_id)),
                        Err(e) => return Err(failed(call.inference_id, innermost(&e))),
                    }
                }
                let answer = Answer { status, body };
                if !(call.retry)(status) {
                    return Ok(answer);
                }
                Ok(answer)
            }
            Err(e) if e.is_timeout() => return Err(timed_out(call.inference_id)),
            Err(e) => Err(e),
        };
        attempt += 1;
        let pause = Duration::from_millis(100 * 4u64.pow(attempt as u32 - 1));
        if attempt > RETRIES || Instant::now() + pause >= deadline {
            return match outcome {
                Ok(answer) => Ok(answer),
                Err(e) => Err(failed(call.inference_id, innermost(&e))),
            };
        }
        tokio::time::sleep(pause).await;
    }
}

/// The parts of an upstream error answer services quote: the `message` its error object holds
/// (OpenAI's `error.message`, Cohere's `message`, Anthropic's `error.message`).
pub fn error_message(answer: &Answer) -> Option<String> {
    let v = answer.json()?;
    let m = v.pointer(&["error", "message"]).or_else(|| v.get("message")).or_else(|| v.get("error").filter(|e| e.as_str().is_some()))?;
    m.as_str().map(str::to_owned)
}

/// Elasticsearch's `BaseResponseHandler.constructErrorMessage` and `toRestStatus`: the message
/// of an unsuccessful answer, with the upstream's own message when it has one.
pub fn status_error(prefix: &str, id: &str, answer: &Answer) -> EsError {
    let msg = match error_message(answer).filter(|m| !m.is_empty()) {
        Some(m) => format!("{prefix} for request from inference entity id [{id}] status [{}]. Error message: [{m}]", answer.status),
        None => format!("{prefix} for request from inference entity id [{id}] status [{}]", answer.status),
    };
    let status = if (100..500).contains(&answer.status) { answer.status } else { 400 };
    EsError::status(status, msg)
}

/// The common status mapping (`OpenAiResponseHandler.checkForFailureStatusCode`, which
/// Cohere's and Anthropic's handlers share in all but wording).
pub fn check(id: &str, url: &str, answer: &Answer) -> EsResult<()> {
    if answer.ok() {
        return Ok(());
    }
    let prefix = match answer.status {
        500..=599 => "Received a server error status code",
        429 => "Received a rate limit status code",
        413 => "Received a content too large status code",
        401 => "Received an authentication error status code",
        403 => "Received a permission denied error status code",
        300..=399 => "Unhandled redirection",
        400 => "Received a bad request status code",
        404 => return Err(status_error(&format!("Resource not found at [{url}]"), id, answer)),
        _ => "Received an unsuccessful status code",
    };
    Err(status_error(prefix, id, answer))
}

/// Statuses Elasticsearch's handlers send again.
pub fn retry_server_errors(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}
