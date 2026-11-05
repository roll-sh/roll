//! The HTTP client for one `opencode serve` (T3's `OpenCode2Client.ts`): Basic auth as user
//! `opencode` (UTF-8), JSON bodies read with a bound, and the `GET /api/event` server-sent events
//! stream with bounded lines and a silence watchdog (heartbeats come every 10–15 s).
//!
//! The server is on loopback, so the client never uses a proxy. Errors never carry the password
//! or a request header.

use super::wire;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

/// The most of one answer read (a page of history, the model catalog).
pub const MAX_BODY: usize = 8 << 20;
/// The longest server-sent event accepted.
pub const MAX_EVENT: usize = 16 << 20;
/// T3's `SILENT_STREAM_TIMEOUT`: longer silence than this means stuck, not idle.
pub const SILENCE: Duration = Duration::from_secs(45);
/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of an error body goes in the error.
const ERROR_EXCERPT: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("OpenCode answered {what} with HTTP {status}{}", excerpt(.body))]
    Status { what: String, status: u16, body: String },
    #[error("couldn't reach the OpenCode server ({what}): {why}")]
    Transport { what: String, why: String },
    #[error("OpenCode's answer to {0} is over the size limit")]
    TooLong(String),
    #[error("OpenCode's answer to {what} isn't what roll expects: {why}")]
    Decode { what: String, why: String },
    #[error("OpenCode didn't answer {0} in time")]
    Timeout(String),
}

fn excerpt(body: &str) -> String {
    if body.trim().is_empty() { String::new() } else { format!(": {}", body.trim()) }
}

impl HttpError {
    pub fn status(&self) -> Option<u16> {
        match self {
            HttpError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    auth: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("base", &self.base).finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(base: &str, password: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().no_proxy().build()?;
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("opencode:{password}")));
        Ok(Client { http, base: base.trim_end_matches('/').to_string(), auth })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base)).header("authorization", &self.auth)
    }

    /// A request with an optional JSON body; `None` for an empty answer (204).
    pub async fn call<T: DeserializeOwned>(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<Option<T>, HttpError> {
        let what = format!("{method} {}", path.split('?').next().unwrap_or(path));
        let mut req = self.request(method, path).header("accept", "application/json");
        if let Some(body) = body {
            req = req.header("content-type", "application/json").body(serde_json::to_vec(body).expect("a JSON value serializes"));
        }
        let run = async {
            let mut resp = req.send().await.map_err(|e| HttpError::Transport { what: what.clone(), why: e.without_url().to_string() })?;
            let status = resp.status();
            let bytes = read_bounded(&mut resp, MAX_BODY, &what).await?;
            if !status.is_success() {
                let mut body = String::from_utf8_lossy(&bytes[..bytes.len().min(ERROR_EXCERPT * 4)]).into_owned();
                if let Some((cut, _)) = body.char_indices().nth(ERROR_EXCERPT) {
                    body.truncate(cut);
                }
                return Err(HttpError::Status { what: what.clone(), status: status.as_u16(), body });
            }
            if bytes.iter().all(u8::is_ascii_whitespace) {
                return Ok(None);
            }
            serde_json::from_slice(&bytes).map(Some).map_err(|e| HttpError::Decode { what: what.clone(), why: e.to_string() })
        };
        tokio::time::timeout(REQUEST_TIMEOUT, run).await.map_err(|_| HttpError::Timeout(what.clone()))?
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, HttpError> {
        self.call(reqwest::Method::GET, path, None).await?.ok_or_else(|| HttpError::Decode { what: format!("GET {path}"), why: "an empty answer".into() })
    }

    pub async fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<Option<T>, HttpError> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    /// A request whose answer roll doesn't read.
    pub async fn send(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> Result<(), HttpError> {
        self.call::<serde::de::IgnoredAny>(method, path, body).await.map(|_| ())
    }

    /// Subscribe to `GET /api/event`. Returns once the server accepted the subscription, so
    /// nothing that follows can be missed (the stream is volatile).
    pub async fn events(&self) -> Result<Events, HttpError> {
        let what = "GET /api/event".to_string();
        let send = self.request(reqwest::Method::GET, "/api/event").header("accept", "text/event-stream").send();
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, send)
            .await
            .map_err(|_| HttpError::Timeout(what.clone()))?
            .map_err(|e| HttpError::Transport { what: what.clone(), why: e.without_url().to_string() })?;
        if !resp.status().is_success() {
            return Err(HttpError::Status { what, status: resp.status().as_u16(), body: String::new() });
        }
        Ok(Events { resp, buf: Vec::new(), data: Vec::new() })
    }
}

async fn read_bounded(resp: &mut reqwest::Response, max: usize, what: &str) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| HttpError::Transport { what: what.into(), why: e.without_url().to_string() })? {
        if out.len() + chunk.len() > max {
            return Err(HttpError::TooLong(what.into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The server-sent events of `GET /api/event`.
pub struct Events {
    resp: reqwest::Response,
    /// Bytes of the line being read.
    buf: Vec<u8>,
    /// The `data:` lines of the event being read.
    data: Vec<u8>,
}

impl Events {
    /// The next event's data. `Ok(None)` when the server ends the stream; an error when it goes
    /// silent for [`SILENCE`], sends an event over [`MAX_EVENT`], or the connection breaks.
    pub async fn next(&mut self) -> Result<Option<String>, HttpError> {
        loop {
            while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.buf.drain(..=nl).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if let Some(event) = self.line(&line)? {
                    return Ok(Some(event));
                }
            }
            let chunk = tokio::time::timeout(SILENCE, self.resp.chunk())
                .await
                .map_err(|_| HttpError::Timeout("GET /api/event (the server stopped sending events)".into()))?
                .map_err(|e| HttpError::Transport { what: "GET /api/event".into(), why: e.without_url().to_string() })?;
            let Some(chunk) = chunk else { return Ok(None) };
            if self.buf.len() + chunk.len() > MAX_EVENT {
                return Err(HttpError::TooLong("GET /api/event".into()));
            }
            self.buf.extend_from_slice(&chunk);
        }
    }

    /// One line of the stream; `Some` when it ends an event with data.
    fn line(&mut self, line: &[u8]) -> Result<Option<String>, HttpError> {
        if line.is_empty() {
            if self.data.is_empty() {
                return Ok(None);
            }
            let data = std::mem::take(&mut self.data);
            return Ok(Some(String::from_utf8_lossy(&data).into_owned()));
        }
        if let Some(rest) = line.strip_prefix(b"data:") {
            let rest = rest.strip_prefix(b" ").unwrap_or(rest);
            if self.data.len() + rest.len() + 1 > MAX_EVENT {
                return Err(HttpError::TooLong("GET /api/event".into()));
            }
            if !self.data.is_empty() {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(rest);
        }
        // Comments, `event:`, `id:` and `retry:` lines carry nothing roll reads.
        Ok(None)
    }
}

/// Parse one event's data; `None` for one roll can't read (new event types appear often).
pub fn parse_event(data: &str) -> Option<wire::Event> {
    serde_json::from_str(data).ok()
}

/// Percent-encode a query value or a path segment (everything but RFC 3986's unreserved set).
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `location[directory]=<dir>`, the deepObject query every directory-scoped read takes.
pub fn location(dir: &str) -> String {
    format!("location%5Bdirectory%5D={}", encode(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_encoded() {
        assert_eq!(location("/w/a b"), "location%5Bdirectory%5D=%2Fw%2Fa%20b");
        assert_eq!(encode("msg_roll:1"), "msg_roll%3A1");
    }
}
