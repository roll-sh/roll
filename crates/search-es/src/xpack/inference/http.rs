//! Requests to inference services.
//!
//! An endpoint's URL is chosen by the tenant (OpenAI's `url`, and every service's host when the
//! node overrides it), so roll's rule for such URLs applies (AGENTS.md): a client that resolves
//! names to public addresses only ([`roll_net::PublicOnly`]), refuses private IP literals and
//! `localhost` ([`roll_net::literal_private_host`]), follows no redirects, uses no proxy and reads
//! at most [`MAX_RESPONSE`] bytes. The node setting [`ALLOW_PRIVATE`] lifts the address check, for
//! test clusters whose endpoints call a mock on the same host; it is off unless set.
//!
//! A request waits at most [`max_timeout`] (the node setting [`MAX_TIMEOUT`], 120s unless set),
//! whatever `timeout` its caller asked for: retries, the rate limiter's wait and reading the
//! answer included. Elasticsearch takes any `timeout` and waits that long, so one tenant could
//! hold a shared node's connection and worker for as long as an endpoint they run stays silent.
//!
//! Like Elasticsearch's `RetryingHttpSender`, a request that fails to connect or gets a
//! retryable status (the service's handler decides: 429, 500, 503) is sent again, here at most
//! twice, within the request's timeout. Answers are untrusted data: services parse them with
//! [`crate::json::parse`] and check their shape.
//!
//! A streamed answer ([`post_stream`]) is retried and checked the same way until its status
//! arrives; a successful one then comes back open ([`Answer::open`]) for
//! [`super::stream`] to read as it comes. As in Elasticsearch, the request's `timeout` covers
//! the wait for that status; the whole stream may last the node's [`max_timeout`].

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use std::time::{Duration, Instant};

/// Lets inference endpoints reach private addresses (loopback, private ranges, `localhost`).
pub const ALLOW_PRIVATE: &str = "roll.inference.allow_private_hosts";

/// The longest any inference request may wait, as a time value (`2m`).
pub const MAX_TIMEOUT: &str = "roll.inference.max_timeout";

/// [`MAX_TIMEOUT`]'s default: four times the API's default `timeout` (30s), room for a long
/// completion.
const DEFAULT_MAX_TIMEOUT: Duration = Duration::from_secs(120);

/// The node's cap on a request's `timeout` ([`MAX_TIMEOUT`]).
pub fn max_timeout(cluster: &Cluster) -> Duration {
    match cluster.config.node_setting(MAX_TIMEOUT).and_then(|v| crate::units::TimeValue::parse(v, MAX_TIMEOUT).ok()) {
        Some(t) => Duration::from_millis(t.as_millis().max(1) as u64),
        None => DEFAULT_MAX_TIMEOUT,
    }
}

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

impl<'a> Call<'a> {
    /// A JSON POST with the usual retries (429, 500, 503).
    pub fn json(inference_id: &'a str, url: String, headers: Vec<(String, String)>, body: &crate::json::Value, timeout: Duration, per_minute: Option<i64>) -> Call<'a> {
        Call { inference_id, url, headers, body: body.to_json().into_bytes(), timeout, retry: retry_server_errors, per_minute }
    }
}

/// A URL as errors may print it: without its query (Google AI Studio's key travels there) or
/// user info.
pub fn redact(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => url.split('?').next().unwrap_or_default().to_string(),
    }
}

/// What came back.
pub struct Answer {
    pub status: u16,
    /// Response headers, names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// A successful streamed answer, its body not read yet ([`post_stream`]).
    pub open: Option<Opened>,
}

/// A streamed answer whose body is still coming.
pub struct Opened {
    pub response: reqwest::Response,
    /// When the whole stream must have ended (the node's [`max_timeout`] after it was sent).
    pub deadline: Instant,
    /// The longest wait for its next piece: the request's `timeout` (within the node's cap).
    pub idle: Duration,
}

/// The most of an unsuccessful streamed answer read (its error message).
const MAX_ERROR_BODY: usize = 1024 * 1024;

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Option<crate::json::Value> {
        crate::json::parse(&self.body).ok().flatten()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// `Inference endpoint [id]: request timed out` (504).
pub fn timed_out(id: &str) -> EsError {
    EsError::status(504, format!("Inference endpoint [{id}]: request timed out"))
}

fn failed(id: &str, detail: String) -> EsError {
    EsError::new(500, "exception", format!("Failed to send request from inference entity id [{id}]")).caused_by(EsError::java(500, "connect_exception", detail))
}

pub(crate) fn innermost(e: &reqwest::Error) -> String {
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
    post_limited(cluster, call, MAX_RESPONSE).await
}

/// [`post`], reading at most `max` bytes of the answer.
pub async fn post_limited(cluster: &Cluster, call: &Call<'_>, max: usize) -> EsResult<Answer> {
    exchange(cluster, call, max, false).await
}

/// [`post`] for a streamed answer: a successful one comes back with its body unread
/// ([`Answer::open`]); any other is read whole (at most 1 MiB) for its error.
pub async fn post_stream(cluster: &Cluster, call: &Call<'_>) -> EsResult<Answer> {
    exchange(cluster, call, MAX_ERROR_BODY, true).await
}

async fn exchange(cluster: &Cluster, call: &Call<'_>, max: usize, stream: bool) -> EsResult<Answer> {
    let allow_private = cluster.config.node_setting(ALLOW_PRIVATE).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let invalid = || EsError::status(400, format!("Invalid host [{}], please check that the URL is correct.", redact(&call.url)));
    let url = reqwest::Url::parse(&call.url).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(invalid());
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
    let cap = max_timeout(cluster);
    let timeout = call.timeout.min(cap);
    let mut b = reqwest::Client::builder().connect_timeout(timeout.min(Duration::from_secs(10))).redirect(reqwest::redirect::Policy::none()).no_proxy();
    // A stream's body outlives the request's timeout: its waits are bounded below instead.
    if !stream {
        b = b.timeout(timeout);
    }
    if !allow_private {
        b = b.dns_resolver(std::sync::Arc::new(roll_net::PublicOnly));
    }
    let client = b.build().map_err(|e| EsError::illegal_state(e.to_string()))?;
    let started = Instant::now();
    let deadline = started + timeout;
    let stream_deadline = started + cap;
    let until = tokio::time::Instant::from_std(deadline);
    super::rate::acquire(&cluster.config.name, call.inference_id, call.per_minute, timeout).await?;
    let mut attempt = 0;
    loop {
        let mut rb = client.post(url.clone());
        for (k, v) in &call.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        let sent = match tokio::time::timeout_at(until, rb.body(call.body.clone()).send()).await {
            Ok(sent) => sent,
            Err(_) => return Err(timed_out(call.inference_id)),
        };
        // A retryable answer, or a connection error.
        let outcome = match sent {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let headers: Vec<(String, String)> = resp.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))).collect();
                if stream && (200..300).contains(&status) {
                    return Ok(Answer { status, headers, body: Vec::new(), open: Some(Opened { response: resp, deadline: stream_deadline, idle: timeout }) });
                }
                let mut body = Vec::new();
                loop {
                    let next = match tokio::time::timeout_at(until, resp.chunk()).await {
                        Ok(next) => next,
                        Err(_) => return Err(timed_out(call.inference_id)),
                    };
                    match next {
                        Ok(Some(c)) => {
                            if body.len() + c.len() > max {
                                return Err(EsError::status(400, format!("the answer of inference endpoint [{}] exceeds [{max}] bytes", call.inference_id)));
                            }
                            body.extend_from_slice(&c);
                        }
                        Ok(None) => break,
                        Err(e) if e.is_timeout() => return Err(timed_out(call.inference_id)),
                        Err(e) => return Err(failed(call.inference_id, innermost(&e))),
                    }
                }
                let answer = Answer { status, headers, body, open: None };
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
        404 => return Err(status_error(&format!("Resource not found at [{}]", redact(url)), id, answer)),
        _ => "Received an unsuccessful status code",
    };
    Err(status_error(prefix, id, answer))
}

/// Google's handlers (`GoogleVertexAiResponseHandler`, `GoogleAiStudioResponseHandler`): no
/// special 400 or 401, 503 says the service is overloaded.
pub fn check_google(id: &str, url: &str, answer: &Answer, service: &str) -> EsResult<()> {
    if answer.ok() {
        return Ok(());
    }
    let unavailable = format!("The {service} service may be temporarily overloaded or down");
    let prefix = match answer.status {
        500 => "Received a server error status code",
        503 => unavailable.as_str(),
        501..=599 => "Received a server error status code",
        429 => "Received a rate limit status code",
        404 => return Err(status_error(&format!("Resource not found at [{}]", redact(url)), id, answer)),
        403 => "Received a permission denied error status code",
        300..=399 => "Unhandled redirection",
        _ => "Received an unsuccessful status code",
    };
    Err(status_error(prefix, id, answer))
}

/// Statuses Elasticsearch's handlers send again.
pub fn retry_server_errors(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}
