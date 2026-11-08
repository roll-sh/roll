//! roll's orchestrator MCP server: agents delegate work to subagents that show up on the canvas
//! (docs/code/agents.md, "Delegation").
//!
//! Ported from T3 Code's `apps/server/src/mcp/McpHttpServer.ts` (Streamable HTTP with bearer
//! credentials), `McpSessionRegistry.ts` (credentials scoped to a thread) and
//! `toolkits/orchestrator/tools.ts` (the tool surface, in [`tools`]). Like T3's, the tools are a
//! command ingress into the orchestrator (`orchestrator/tasks.rs`); they never touch a provider.
//!
//! - One server per daemon, on the IPv4 loopback address and a free port, started by `Host::open`.
//! - Every provider session gets it as server `roll` with its thread's bearer token. A token
//!   names one thread, which sees and controls only its own delegated tasks and their
//!   descendants. Tokens live in memory only (a restarted daemon mints new ones for the sessions
//!   it starts) and die with their thread.
//! - Streamable HTTP without server-initiated streams: every POST gets one JSON body (the spec
//!   allows that instead of SSE), GET is 405, and no session id is issued.

pub(crate) mod tools;

use crate::Host;
use crate::provider::McpServer;
use agent_proto::ThreadId;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, Weak};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The server name agents see (`mcp__roll__delegate_task`).
pub const SERVER_NAME: &str = "roll";

/// Protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// A request body larger than this is refused (a task prompt is the biggest thing sent).
const BODY_LIMIT: usize = 1 << 20;

pub struct Server {
    addr: SocketAddr,
    tokens: Mutex<Tokens>,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
struct Tokens {
    /// sha256(token) → thread, so a lookup never compares secrets byte by byte.
    by_hash: HashMap<[u8; 32], ThreadId>,
    by_thread: HashMap<ThreadId, String>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Server {
    /// Binds the IPv4 loopback address on a free port; [`Server::start`] serves it once the host exists.
    pub(crate) async fn bind() -> anyhow::Result<(Server, TcpListener)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let server =
            Server { addr: listener.local_addr()?, tokens: Mutex::new(Tokens::default()), task: Mutex::new(None) };
        Ok((server, listener))
    }

    pub(crate) fn start(&self, host: Weak<Host>, listener: TcpListener) {
        let app = Router::new()
            .route("/mcp", post(handle).get(not_allowed).delete(not_allowed))
            .layer(DefaultBodyLimit::max(BODY_LIMIT))
            .with_state(host);
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::warn!("the orchestrator MCP server stopped: {e}");
            }
        });
        *self.task.lock().unwrap() = Some(task);
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// The server as a session of `thread` gets it, with the thread's token (minted on first use).
    pub(crate) fn session_server(&self, thread: &ThreadId) -> McpServer {
        let mut tokens = self.tokens.lock().unwrap();
        let token = match tokens.by_thread.get(thread) {
            Some(token) => token.clone(),
            None => {
                let token = format!("rlx_{}", hex::encode(rand::random::<[u8; 32]>()));
                tokens.by_hash.insert(digest(&token), thread.clone());
                tokens.by_thread.insert(thread.clone(), token.clone());
                token
            }
        };
        McpServer {
            name: SERVER_NAME.into(),
            url: self.url(),
            headers: vec![("Authorization".into(), format!("Bearer {token}"))],
        }
    }

    /// The thread is gone: its token stops working at once.
    pub(crate) fn revoke(&self, thread: &ThreadId) {
        let mut tokens = self.tokens.lock().unwrap();
        if let Some(token) = tokens.by_thread.remove(thread) {
            tokens.by_hash.remove(&digest(&token));
        }
    }

    fn caller(&self, token: &str) -> Option<ThreadId> {
        self.tokens.lock().unwrap().by_hash.get(&digest(token)).cloned()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

async fn not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

/// Browsers may only reach a local server from a local page (the spec's DNS-rebinding guard);
/// agents send no `Origin` at all.
fn origin_ok(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let Ok(origin) = origin.to_str() else { return false };
    let host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let host = host.rsplit_once(':').map_or(host, |(h, port)| if port.chars().all(|c| c.is_ascii_digit()) { h } else { host });
    host == "localhost" || host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token.trim())
}

async fn handle(State(host): State<Weak<Host>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(host) = host.upgrade() else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    if !origin_ok(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(caller) = bearer(&headers).and_then(|t| host.mcp.caller(t)) else {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")]).into_response();
    };
    let message: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return reply(StatusCode::BAD_REQUEST, error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    match message {
        // Batches (2025-03-26): answer the requests in one array.
        Value::Array(items) => {
            let mut answers = Vec::new();
            for item in items {
                if let Some(answer) = dispatch(&host, &caller, item).await {
                    answers.push(answer);
                }
            }
            if answers.is_empty() { StatusCode::ACCEPTED.into_response() } else { reply(StatusCode::OK, Value::Array(answers)) }
        }
        item => match dispatch(&host, &caller, item).await {
            Some(answer) => reply(StatusCode::OK, answer),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn reply(status: StatusCode, body: Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// One JSON-RPC message; `None` for notifications and responses (nothing to answer).
async fn dispatch(host: &std::sync::Arc<Host>, caller: &ThreadId, message: Value) -> Option<Value> {
    let Value::Object(mut message) = message else {
        return Some(error(Value::Null, -32600, "invalid request"));
    };
    let id = message.remove("id");
    let Some(Value::String(method)) = message.remove("method") else {
        // A response to a request we never send, or garbage without an id.
        return id.map(|id| error(id, -32600, "invalid request"));
    };
    let id = id?;
    let params = message.remove("params").unwrap_or(Value::Null);
    Some(match method.as_str() {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
            result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "title": "roll", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": tools::INSTRUCTIONS,
                }),
            )
        }
        "ping" => result(id, json!({})),
        "tools/list" => result(id, json!({ "tools": tools::list() })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs a tool name"));
            };
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match tools::call(host, caller, name, arguments).await {
                Some(answer) => result(id, answer),
                None => error(id, -32602, &format!("unknown tool {name}")),
            }
        }
        _ => error(id, -32601, &format!("method {method} not found")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_origins_pass() {
        let with = |origin: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            origin_ok(&headers)
        };
        assert!(origin_ok(&HeaderMap::new()));
        assert!(with("http://localhost:3000") && with(&format!("http://{}", Ipv4Addr::LOCALHOST)) && with("http://[::1]:8080"));
        assert!(!with("https://evil.example") && !with("http://localhost.evil.example:80"));
    }
}
