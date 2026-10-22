//! An MCP server for agents at `/v1/code/mcp` (docs/code/mcp.md): the Model Context Protocol's
//! Streamable HTTP transport, JSON-RPC 2.0. Responses are plain JSON, or server-sent events when
//! the client accepts only `text/event-stream` (each response one `message` event, keep-alive
//! comments while a long call runs). There's no server-initiated stream: `GET` answers 405.
//!
//! Every tool is a call to the HTTP API on the caller's behalf: the request goes through the same
//! router with the caller's `Authorization` (and `X-Roll-Agent`/`X-Roll-User`) headers, so tools
//! enforce exactly what the routes do, return the same shapes, and can't drift from them.
//!
//! - `initialize` negotiates the protocol version and starts a session (`Mcp-Session-Id`). Every
//!   later request needs the session id (400 without one, 404 for an unknown or ended one) and
//!   the same token the session started with.
//! - Tool failures (a 4xx from the API) are tool results with `isError: true`; protocol problems
//!   are JSON-RPC errors. A land that conflicts or is blocked is a normal result
//!   (`status: "conflict" | "blocked"`): it's an outcome agents act on, not a failure.
//! - Requests with an `Origin` header (browsers) must come from the web app or the API itself,
//!   so a page can't drive a local daemon (DNS rebinding).

use crate::Code;
use axum::body::Body as AxumBody;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

/// Protocol versions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const MAX_SESSIONS: usize = 10_000;
/// Lines `read_file` returns without a range.
const READ_LINES: usize = 2_000;
const MAX_RESPONSE: usize = 64 << 20;
/// Keep-alive comments on an event stream while a tool call runs.
const SSE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

struct Session {
    version: String,
    /// sha256 of the token the session started with ("" for none).
    token: String,
    last_seen: i64,
}

#[derive(Clone)]
struct Mcp {
    code: Arc<Code>,
    api: Router,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

/// The MCP route. `api` is the rest of the API, which tools call into.
pub fn routes(code: Arc<Code>, api: Router) -> Router {
    let state = Mcp { code, api, sessions: Arc::new(Mutex::new(HashMap::new())) };
    Router::new()
        .route("/v1/code/mcp", post(handle).get(no_stream).delete(end_session))
        .with_state(state)
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn http_error(status: StatusCode, message: &str) -> Response {
    (status, Json(rpc_error(Value::Null, -32600, message))).into_response()
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = header_str(h, "authorization")?;
    let (scheme, t) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| t.trim().to_string())
}

fn token_key(h: &HeaderMap) -> String {
    bearer(h).map(|t| crate::hash_token(&t)).unwrap_or_default()
}

/// Browsers send `Origin`; only the web app's and the API's own origins may call.
fn origin_ok(code: &Code, h: &HeaderMap) -> bool {
    let Some(origin) = header_str(h, "origin") else { return true };
    let of = |u: &str| url::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
    let origin = of(origin);
    origin.is_some() && [of(&code.config.web_url), of(&code.config.api_url)].contains(&origin)
}

async fn no_stream() -> Response {
    let mut r = (StatusCode::METHOD_NOT_ALLOWED, "this MCP server doesn't open server-sent event streams; POST requests").into_response();
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    r
}

async fn end_session(State(m): State<Mcp>, headers: HeaderMap) -> Response {
    let Some(id) = header_str(&headers, SESSION_HEADER) else { return http_error(StatusCode::BAD_REQUEST, "Mcp-Session-Id is required") };
    let mut s = m.sessions.lock();
    match s.get(id) {
        Some(sess) if sess.token == token_key(&headers) => {
            s.remove(id);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => http_error(StatusCode::NOT_FOUND, "unknown session"),
    }
}

async fn handle(State(m): State<Mcp>, ext: axum::http::Extensions, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !origin_ok(&m.code, &headers) {
        return http_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(rpc_error(Value::Null, -32700, format!("parse error: {e}")))).into_response(),
    };
    let batch = msg.is_array();
    let messages: Vec<Value> = match msg {
        Value::Array(a) if a.is_empty() => return (StatusCode::BAD_REQUEST, Json(rpc_error(Value::Null, -32600, "empty batch"))).into_response(),
        Value::Array(a) => a,
        v => vec![v],
    };
    let initializing = messages.iter().any(|v| v.get("method").and_then(Value::as_str) == Some("initialize"));
    if initializing {
        if messages.len() != 1 {
            return http_error(StatusCode::BAD_REQUEST, "initialize must be sent on its own");
        }
        if bearer(&headers).is_some() && matches!(m.code.caller(bearer(&headers).as_deref()), Err(crate::Error::Unauthorized)) {
            return http_error(StatusCode::UNAUTHORIZED, "the token isn't valid (revoked?)");
        }
    } else {
        let Some(id) = header_str(&headers, SESSION_HEADER) else {
            return http_error(StatusCode::BAD_REQUEST, "Mcp-Session-Id is required (send initialize first)");
        };
        // A revoked (or otherwise invalid) token ends its session at once.
        if bearer(&headers).is_some() && matches!(m.code.caller(bearer(&headers).as_deref()), Err(crate::Error::Unauthorized)) {
            m.sessions.lock().remove(id);
            return http_error(StatusCode::UNAUTHORIZED, "the token isn't valid (revoked?)");
        }
        let mut sessions = m.sessions.lock();
        let Some(s) = sessions.get_mut(id).filter(|s| s.token == token_key(&headers)) else {
            return http_error(StatusCode::NOT_FOUND, "unknown or ended session: initialize again");
        };
        if let Some(v) = header_str(&headers, VERSION_HEADER)
            && v != s.version
            && !PROTOCOL_VERSIONS.contains(&v)
        {
            return http_error(StatusCode::BAD_REQUEST, &format!("unsupported MCP-Protocol-Version {v}"));
        }
        s.last_seen = crate::db::now();
    }
    // `Accept: text/event-stream` without `application/json`: the answer comes as server-sent
    // events (one `message` event per response, keep-alive comments while a long tool call
    // runs). An initialize always answers in JSON, since its session id goes in a header.
    if !initializing && wants_sse(&headers) {
        let has_requests = messages.iter().any(|v| v.get("method").is_some() && v.get("id").is_some());
        if !has_requests {
            return StatusCode::ACCEPTED.into_response();
        }
        // An event stream holds one of the caller's stream slots while it's open.
        let caller = m.code.caller(bearer(&headers).as_deref()).unwrap_or(crate::Caller::Anonymous);
        let peer = ext.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0);
        let key = crate::streams::key(&caller, bearer(&headers).as_deref(), &headers, peer, m.code.config.trust_forwarded_for);
        let permit = match m.code.stream_permit(key) {
            Ok(p) => p,
            Err(resp) => return *resp,
        };
        let stream = futures::stream::once(async move {
            let out = process(&m, &headers, messages).await.0;
            drop(permit);
            out
        })
        .flat_map(|responses| {
            futures::stream::iter(
                responses.into_iter().map(|r| Ok::<_, std::convert::Infallible>(Event::default().event("message").data(r.to_string()))),
            )
        });
        return Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE)).into_response();
    }
    let (mut responses, new_session) = process(&m, &headers, messages).await;
    let mut resp = if responses.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if batch {
        Json(Value::Array(responses)).into_response()
    } else {
        Json(responses.pop().unwrap_or(Value::Null)).into_response()
    };
    if let Some(sid) = new_session
        && let Ok(v) = HeaderValue::from_str(&sid)
    {
        resp.headers_mut().insert(SESSION_HEADER, v);
    }
    resp
}

/// The client takes only an event stream.
fn wants_sse(h: &HeaderMap) -> bool {
    let accept = h.get_all(header::ACCEPT).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",").to_ascii_lowercase();
    accept.contains("text/event-stream") && !accept.contains("application/json")
}

/// Answer each request in `messages` (notifications get none); the new session id when one
/// of them was `initialize`.
async fn process(m: &Mcp, headers: &HeaderMap, messages: Vec<Value>) -> (Vec<Value>, Option<String>) {
    let mut new_session = None;
    let mut responses = Vec::new();
    for msg in messages {
        if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            responses.push(rpc_error(msg.get("id").cloned().unwrap_or(Value::Null), -32600, "jsonrpc must be \"2.0\""));
            continue;
        }
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to something we never asked: nothing to do.
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            // A notification (`notifications/initialized`, `notifications/cancelled`, …).
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let resp = match method {
            "initialize" => {
                let (result, sid) = initialize(m, headers, &params);
                new_session = Some(sid);
                rpc_result(id, result)
            }
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if !tools().iter().any(|t| t["name"] == name) {
                    rpc_error(id, -32602, format!("unknown tool {name:?}"))
                } else if !args.is_object() {
                    rpc_error(id, -32602, "arguments must be an object")
                } else {
                    rpc_result(id, call_tool(m, headers, name, &args).await)
                }
            }
            "resources/list" => rpc_result(id, json!({ "resources": [] })),
            "prompts/list" => rpc_result(id, json!({ "prompts": [] })),
            other => rpc_error(id, -32601, format!("method not found: {other}")),
        };
        responses.push(resp);
    }
    (responses, new_session)
}

fn initialize(m: &Mcp, headers: &HeaderMap, params: &Value) -> (Value, String) {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
    let sid = crate::new_id("mcp");
    {
        let mut s = m.sessions.lock();
        if s.len() >= MAX_SESSIONS {
            // Forget the least recently used.
            if let Some(oldest) = s.iter().min_by_key(|(_, v)| v.last_seen).map(|(k, _)| k.clone()) {
                s.remove(&oldest);
            }
        }
        s.insert(sid.clone(), Session { version: version.to_string(), token: token_key(headers), last_seen: crate::db::now() });
    }
    let result = json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "roll-code", "title": "roll code", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "roll code: source control for agents. Repos are \"owner/repo\". Read with search_code, read_file, \
list_tree, log, blame, compare. Take work with claim_work (keep it alive with heartbeat_work), make a change with \
create_change (files map: {path: {content} | {content_b64} | {delete: true}}), then land it. A land can come back \
queued (merge queue), blocked (review or checks rules; the blockers say what's missing) or conflict (the paths \
changed underneath you: re-read them, revise, land again). Changes are \"chg_…\" or \"owner/repo#N\"; work items \
\"wi_…\" or \"owner/repo#N\".",
    });
    (result, sid)
}

// ── tools ──

fn s(desc: &str) -> Value {
    json!({ "type": "string", "description": desc })
}

fn int(desc: &str) -> Value {
    json!({ "type": "integer", "description": desc })
}

fn boolean(desc: &str) -> Value {
    json!({ "type": "boolean", "description": desc })
}

fn tool(name: &str, title: &str, description: &str, props: Value, required: &[&str], read_only: bool) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
        "annotations": { "readOnlyHint": read_only, "destructiveHint": false, "openWorldHint": false },
    })
}

fn files_schema() -> Value {
    json!({
        "type": "object",
        "description": "Files to write or delete, by path: {\"src/a.rs\": {\"content\": \"…\"}, \"img.png\": {\"content_b64\": \"…\"}, \"old.txt\": {\"delete\": true}}; optional \"mode\": \"100644\" | \"100755\" | \"120000\".",
        "additionalProperties": {
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "content_b64": { "type": "string" },
                "delete": { "type": "boolean" },
                "mode": { "type": "string", "enum": ["100644", "100755", "120000"] }
            },
            "additionalProperties": false
        }
    })
}

/// Every tool, with its JSON Schema.
pub fn tools() -> Vec<Value> {
    let repo = s("Repository, \"owner/repo\"");
    let rev = s("Revision: a branch, tag, changeset (cs_…) or git commit; default: the default branch");
    let change = s("Change: \"chg_…\" or \"owner/repo#N\"");
    let work = s("Work item: \"wi_…\" or \"owner/repo#N\"");
    vec![
        tool(
            "search_code",
            "Search code",
            "Search a repository's code (literal or regex). With rev, files that differ from the default branch are searched at that revision.",
            json!({ "repo": repo, "query": s("Text or regex"), "regex": boolean("Treat query as a regex"), "case_sensitive": boolean("Match case"), "path": s("Glob to narrow files, e.g. src/**/*.rs"), "rev": rev, "limit": int("Most matches (default 100)") }),
            &["repo", "query"],
            true,
        ),
        tool(
            "read_file",
            "Read a file",
            "A file's content at a revision. Long files come back in ranges: pass start_line/end_line (1-based, inclusive) for more.",
            json!({ "repo": repo, "path": s("File path"), "rev": rev, "start_line": int("First line (1-based)"), "end_line": int("Last line (inclusive)") }),
            &["repo", "path"],
            true,
        ),
        tool(
            "list_tree",
            "List a directory",
            "Entries of a directory at a revision, with each entry's last change.",
            json!({ "repo": repo, "path": s("Directory; default the root"), "rev": rev }),
            &["repo"],
            true,
        ),
        tool(
            "log",
            "History",
            "First-parent history, newest first, optionally only changesets that touch a path.",
            json!({ "repo": repo, "rev": rev, "path": s("Only changesets touching this path"), "limit": int("Most entries (default 30)"), "cursor": s("Continue after this changeset (from the last page)") }),
            &["repo"],
            true,
        ),
        tool(
            "blame",
            "Blame",
            "Which changeset last changed each line of a file (first-parent history), as line ranges.",
            json!({ "repo": repo, "path": s("File path"), "rev": rev }),
            &["repo", "path"],
            true,
        ),
        tool(
            "get_changeset",
            "Get a changeset",
            "A changeset's metadata and per-file diff.",
            json!({ "repo": repo, "id": s("Changeset id (cs_… or a prefix) or git commit") }),
            &["repo", "id"],
            true,
        ),
        tool(
            "compare",
            "Compare revisions",
            "Changesets and file diffs between two revisions.",
            json!({ "repo": repo, "base": s("Base revision"), "head": s("Head revision") }),
            &["repo", "base", "head"],
            true,
        ),
        tool(
            "create_change",
            "Create a change",
            "Open a change (like a pull request) from file edits on a base revision.",
            json!({ "repo": repo, "title": s("Title"), "body": s("Description (markdown)"), "base": s("Revision the edits are based on"), "message": s("Commit message (default: the title)"), "files": files_schema(), "work_item": s("Work item this change does (wi_… or a number)"), "draft": boolean("Open as a draft") }),
            &["repo", "title", "base", "files"],
            false,
        ),
        tool(
            "revise_change",
            "Revise a change",
            "Replace a change's content with new edits on a base revision (a new revision).",
            json!({ "change": change, "base": s("Revision the edits are based on (usually the change's head)"), "message": s("Commit message"), "files": files_schema() }),
            &["change", "base", "files"],
            false,
        ),
        tool(
            "land",
            "Land",
            "Land a change, or edits directly. Returns status landed, queued (merge queue), blocked (with blockers) or conflict (with paths).",
            json!({ "change": change, "repo": s("For a direct land: repository, \"owner/repo\""), "base": s("Direct land: base revision"), "message": s("Direct land: commit message"), "files": files_schema(), "ref": s("Direct land: branch (default: the default branch)"), "lane": json!({ "type": "string", "enum": ["normal", "urgent"], "description": "Merge queue lane" }) }),
            &[],
            false,
        ),
        tool(
            "get_change",
            "Get a change",
            "A change with its blockers, checks, review status and inline threads.",
            json!({ "change": change }),
            &["change"],
            true,
        ),
        tool(
            "list_changes",
            "List changes",
            "A repository's changes, newest first.",
            json!({ "repo": repo, "state": json!({ "type": "string", "enum": ["open", "queued", "landed", "conflict", "closed"] }) }),
            &["repo"],
            true,
        ),
        tool(
            "claim_work",
            "Claim work",
            "Claim the highest-priority open work item (a lease; keep it with heartbeat_work). Returns work: null when there's nothing to do.",
            json!({ "repo": repo, "agent": s("Who's claiming (your agent name)"), "labels": json!({ "type": "array", "items": { "type": "string" }, "description": "Only items with all these labels" }), "lease_s": int("Lease length in seconds (default 900)") }),
            &["repo", "agent"],
            false,
        ),
        tool("heartbeat_work", "Extend a lease", "Keep a claimed work item's lease alive.", json!({ "work": work, "agent": s("Your agent name") }), &["work"], false),
        tool("complete_work", "Complete work", "Mark a claimed work item done.", json!({ "work": work, "agent": s("Your agent name") }), &["work"], false),
        tool("release_work", "Release work", "Give a claimed work item back to the queue.", json!({ "work": work, "agent": s("Your agent name") }), &["work"], false),
        tool(
            "list_work",
            "List work",
            "A repository's work items.",
            json!({ "repo": repo, "state": json!({ "type": "string", "enum": ["open", "claimed", "review", "done", "closed"] }), "label": s("Only items with this label") }),
            &["repo"],
            true,
        ),
        tool(
            "create_work",
            "Create work",
            "Open a work item (like an issue).",
            json!({ "repo": repo, "title": s("Title"), "body": s("Description (markdown)"), "labels": json!({ "type": "array", "items": { "type": "string" } }), "priority": int("0 (P0, most urgent) to 3") }),
            &["repo", "title"],
            false,
        ),
        tool(
            "comment",
            "Comment",
            "Reply in a thread (thread), or start one on a line of a change (change + path + line).",
            json!({ "thread": s("Thread id (th_…) to reply in"), "change": change, "path": s("File (new thread)"), "line": int("Line (new thread)"), "start_line": int("First line of a range (new thread)"), "side": json!({ "type": "string", "enum": ["new", "old"] }), "body": s("Comment (markdown; ```suggestion blocks allowed)") }),
            &["body"],
            false,
        ),
        tool(
            "review",
            "Review",
            "Submit a review: approve, request_changes or comment, with optional inline comments.",
            json!({
                "change": change,
                "verdict": json!({ "type": "string", "enum": ["approve", "request_changes", "comment"] }),
                "body": s("Summary"),
                "comments": json!({ "type": "array", "items": { "type": "object", "properties": { "path": { "type": "string" }, "line": { "type": "integer" }, "start_line": { "type": "integer" }, "side": { "type": "string", "enum": ["new", "old"] }, "body": { "type": "string" } }, "required": ["path", "line", "body"], "additionalProperties": false } })
            }),
            &["change", "verdict"],
            false,
        ),
        tool("queue_status", "Merge queue", "The merge queue: the batch being tested and who's waiting.", json!({ "repo": repo }), &["repo"], true),
        tool(
            "request_ai_review",
            "Request an AI review",
            "Ask roll's reviewer to review a change.",
            json!({ "change": change, "mode": json!({ "type": "string", "enum": ["auto", "standard", "deep", "max"] }), "full": boolean("Review everything, not just what changed since the last review") }),
            &["change"],
            false,
        ),
    ]
}

/// A tool's outcome: an API failure or structured data.
struct Outcome {
    ok: bool,
    data: Value,
    text: Option<String>,
}

fn ok(data: Value) -> Outcome {
    Outcome { ok: true, data, text: None }
}

fn fail(msg: impl Into<String>) -> Outcome {
    let msg = msg.into();
    Outcome { ok: false, data: json!({ "error": msg }), text: Some(msg) }
}

/// One API call's result.
struct Api {
    status: StatusCode,
    body: Value,
    raw: Vec<u8>,
    content_type: String,
}

impl Api {
    fn ok(&self) -> bool {
        self.status.is_success()
    }

    fn error(&self) -> Outcome {
        let msg = self.body.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| self.status.to_string());
        let msg = match self.status {
            StatusCode::UNAUTHORIZED => format!("{msg}: this needs a token (Authorization: Bearer …)"),
            StatusCode::FORBIDDEN => format!("not allowed: {msg}"),
            StatusCode::NOT_FOUND => format!("not found: {msg}"),
            _ => msg,
        };
        let mut data = if self.body.is_object() { self.body.clone() } else { json!({}) };
        data["error"] = json!(msg);
        data["http_status"] = json!(self.status.as_u16());
        Outcome { ok: false, text: Some(msg), data }
    }
}

/// Percent-encode a path segment.
fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encode a repo path, keeping its slashes.
fn path_segs(p: &str) -> String {
    p.trim_matches('/').split('/').map(seg).collect::<Vec<_>>().join("/")
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        if let Some(v) = v {
            q.append_pair(k, v);
        }
    }
    let q = q.finish();
    if q.is_empty() { q } else { format!("?{q}") }
}

struct Call<'a> {
    m: &'a Mcp,
    headers: &'a HeaderMap,
}

impl Call<'_> {
    async fn api(&self, method: Method, path: &str, body: Option<Value>) -> Api {
        let mut b = Request::builder().method(method).uri(path);
        for name in ["authorization", "x-roll-agent", "x-roll-user"] {
            if let Some(v) = self.headers.get(name) {
                b = b.header(name, v.clone());
            }
        }
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(AxumBody::from(v.to_string())),
            None => b.body(AxumBody::empty()),
        };
        let req = match req {
            Ok(r) => r,
            Err(e) => return Api { status: StatusCode::BAD_REQUEST, body: json!({ "error": e.to_string() }), raw: Vec::new(), content_type: String::new() },
        };
        let resp = match self.m.api.clone().oneshot(req).await {
            Ok(r) => r,
            Err(e) => match e {},
        };
        let status = resp.status();
        let content_type = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let raw = axum::body::to_bytes(resp.into_body(), MAX_RESPONSE).await.map(|b| b.to_vec()).unwrap_or_default();
        let body = if content_type.starts_with("application/json") { serde_json::from_slice(&raw).unwrap_or(Value::Null) } else { Value::Null };
        Api { status, body, raw, content_type }
    }

    async fn get(&self, path: &str) -> Api {
        self.api(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> Api {
        self.api(Method::POST, path, Some(body)).await
    }

    /// `/v1/code/repos/{owner}/{repo}` for "owner/repo".
    fn repo(&self, args: &Value) -> std::result::Result<String, Outcome> {
        let r = str_arg(args, "repo").ok_or_else(|| fail("repo is required (\"owner/repo\")"))?;
        repo_base(r)
    }

    async fn default_rev(&self, base: &str, args: &Value) -> std::result::Result<String, Outcome> {
        if let Some(r) = str_arg(args, "rev") {
            return Ok(r.to_string());
        }
        let repo = self.get(base).await;
        if !repo.ok() {
            return Err(repo.error());
        }
        Ok(repo.body["default_branch"].as_str().unwrap_or("main").to_string())
    }

    /// A change's id from "chg_…" or "owner/repo#N".
    async fn change_id(&self, args: &Value) -> std::result::Result<String, Outcome> {
        let c = str_arg(args, "change").ok_or_else(|| fail("change is required (\"chg_…\" or \"owner/repo#N\")"))?;
        self.by_number(c, "changes").await
    }

    async fn work_id(&self, args: &Value) -> std::result::Result<String, Outcome> {
        let w = str_arg(args, "work").ok_or_else(|| fail("work is required (\"wi_…\" or \"owner/repo#N\")"))?;
        self.by_number(w, "work").await
    }

    async fn by_number(&self, s: &str, kind: &str) -> std::result::Result<String, Outcome> {
        let Some((repo, n)) = s.split_once('#') else { return Ok(s.to_string()) };
        let n: u64 = n.trim().parse().map_err(|_| fail(format!("{s}: expected owner/repo#number")))?;
        let base = repo_base(repo)?;
        let r = self.get(&format!("{base}/{kind}/{n}")).await;
        if !r.ok() {
            return Err(r.error());
        }
        r.body["id"].as_str().map(str::to_string).ok_or_else(|| fail(format!("{s} has no id")))
    }
}

fn repo_base(r: &str) -> std::result::Result<String, Outcome> {
    let r = r.trim().trim_end_matches(".git");
    let (o, n) = r.split_once('/').filter(|(o, n)| !o.is_empty() && !n.is_empty() && !n.contains('/')).ok_or_else(|| fail(format!("repo {r:?}: expected \"owner/repo\"")))?;
    Ok(format!("/v1/code/repos/{}/{}", seg(o), seg(n)))
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn opt(args: &Value, k: &str) -> Option<String> {
    match args.get(k) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// `args` minus some keys (what's left is the request body).
fn body_of(args: &Value, drop: &[&str]) -> Value {
    let mut b = args.clone();
    if let Some(o) = b.as_object_mut() {
        for k in drop {
            o.remove(*k);
        }
    }
    b
}

async fn call_tool(m: &Mcp, headers: &HeaderMap, name: &str, args: &Value) -> Value {
    let c = Call { m, headers };
    let outcome = match run(&c, name, args).await {
        Ok(o) | Err(o) => o,
    };
    let text = outcome.text.clone().unwrap_or_else(|| serde_json::to_string_pretty(&outcome.data).unwrap_or_default());
    let structured = if outcome.data.is_object() { outcome.data } else { json!({ "result": outcome.data }) };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": !outcome.ok,
    })
}

/// Pass a call's JSON straight through.
fn pass(r: Api) -> Outcome {
    if !r.ok() {
        return r.error();
    }
    ok(if r.body.is_array() { json!({ "items": r.body }) } else { r.body })
}

async fn run(c: &Call<'_>, name: &str, args: &Value) -> std::result::Result<Outcome, Outcome> {
    Ok(match name {
        "search_code" => {
            let base = c.repo(args)?;
            let q = str_arg(args, "query").ok_or_else(|| fail("query is required"))?;
            let qs = query(&[
                ("q", Some(q.to_string())),
                ("regex", opt(args, "regex")),
                ("case", opt(args, "case_sensitive")),
                ("path", opt(args, "path")),
                ("rev", opt(args, "rev")),
                ("limit", opt(args, "limit")),
            ]);
            pass(c.get(&format!("{base}/search{qs}")).await)
        }
        "read_file" => {
            let base = c.repo(args)?;
            let path = str_arg(args, "path").ok_or_else(|| fail("path is required"))?;
            let rev = c.default_rev(&base, args).await?;
            let r = c.get(&format!("{base}/blob/{}/{}", seg(&rev), path_segs(path))).await;
            if !r.ok() {
                return Err(r.error());
            }
            read_file_outcome(&r, path, &rev, args)
        }
        "list_tree" => {
            let base = c.repo(args)?;
            let rev = c.default_rev(&base, args).await?;
            let path = str_arg(args, "path").map(path_segs).unwrap_or_default();
            pass(c.get(&format!("{base}/tree/{}/{path}", seg(&rev))).await)
        }
        "log" => {
            let base = c.repo(args)?;
            let limit = opt(args, "limit").or(Some("30".into()));
            pass(c.get(&format!("{base}/log{}", query(&[("rev", opt(args, "rev")), ("path", opt(args, "path")), ("limit", limit), ("cursor", opt(args, "cursor"))]))).await)
        }
        "blame" => {
            let base = c.repo(args)?;
            let path = str_arg(args, "path").ok_or_else(|| fail("path is required"))?;
            let rev = c.default_rev(&base, args).await?;
            pass(c.get(&format!("{base}/blame/{}/{}", seg(&rev), path_segs(path))).await)
        }
        "get_changeset" => {
            let base = c.repo(args)?;
            let id = str_arg(args, "id").ok_or_else(|| fail("id is required"))?;
            pass(c.get(&format!("{base}/changesets/{}", seg(id))).await)
        }
        "compare" => {
            let base = c.repo(args)?;
            let (a, b) = (str_arg(args, "base").ok_or_else(|| fail("base is required"))?, str_arg(args, "head").ok_or_else(|| fail("head is required"))?);
            pass(c.get(&format!("{base}/compare/{}...{}", seg(a), seg(b))).await)
        }
        "create_change" => {
            let base = c.repo(args)?;
            let mut body = body_of(args, &["repo"]);
            if body.get("message").is_none() {
                body["message"] = args["title"].clone();
            }
            pass(c.post(&format!("{base}/changes"), body).await)
        }
        "revise_change" => {
            let id = c.change_id(args).await?;
            pass(c.post(&format!("/v1/code/changes/{}/revise", seg(&id)), body_of(args, &["change"])).await)
        }
        "land" => {
            let r = if args.get("change").is_some() {
                let id = c.change_id(args).await?;
                c.post(&format!("/v1/code/changes/{}/land", seg(&id)), body_of(args, &["change"])).await
            } else {
                let base = c.repo(args).map_err(|_| fail("land needs a change, or a repo with base, message and files"))?;
                c.post(&format!("{base}/land"), body_of(args, &["repo", "change"])).await
            };
            land_outcome(r)
        }
        "get_change" => {
            let id = c.change_id(args).await?;
            let ch = c.get(&format!("/v1/code/changes/{}", seg(&id))).await;
            if !ch.ok() {
                return Err(ch.error());
            }
            let status = c.get(&format!("/v1/code/changes/{}/review-status", seg(&id))).await;
            let threads = c.get(&format!("/v1/code/changes/{}/threads", seg(&id))).await;
            ok(json!({
                "change": ch.body,
                "review_status": if status.ok() { status.body } else { Value::Null },
                "threads": if threads.ok() { threads.body } else { json!([]) },
            }))
        }
        "list_changes" => {
            let base = c.repo(args)?;
            pass(c.get(&format!("{base}/changes{}", query(&[("state", opt(args, "state"))]))).await)
        }
        "claim_work" => {
            let base = c.repo(args)?;
            let r = c.post(&format!("{base}/work/claim"), body_of(args, &["repo"])).await;
            if r.status == StatusCode::NO_CONTENT {
                ok(json!({ "work": null, "message": "no open work matches" }))
            } else if r.ok() {
                ok(json!({ "work": r.body }))
            } else {
                r.error()
            }
        }
        "heartbeat_work" | "complete_work" | "release_work" => {
            let id = c.work_id(args).await?;
            let action = name.trim_end_matches("_work");
            pass(c.post(&format!("/v1/code/work/{}/{action}", seg(&id)), body_of(args, &["work"])).await)
        }
        "list_work" => {
            let base = c.repo(args)?;
            pass(c.get(&format!("{base}/work{}", query(&[("state", opt(args, "state")), ("label", opt(args, "label"))]))).await)
        }
        "create_work" => {
            let base = c.repo(args)?;
            pass(c.post(&format!("{base}/work"), body_of(args, &["repo"])).await)
        }
        "comment" => {
            if let Some(th) = str_arg(args, "thread") {
                pass(c.post(&format!("/v1/code/threads/{}/comments", seg(th)), json!({ "body": args.get("body") })).await)
            } else {
                let Ok(id) = c.change_id(args).await else { return Err(fail("comment needs a thread to reply in, or a change, path and line")) };
                pass(c.post(&format!("/v1/code/changes/{}/threads", seg(&id)), body_of(args, &["change", "thread"])).await)
            }
        }
        "review" => {
            let id = c.change_id(args).await?;
            pass(c.post(&format!("/v1/code/changes/{}/reviews", seg(&id)), body_of(args, &["change"])).await)
        }
        "queue_status" => {
            let base = c.repo(args)?;
            pass(c.get(&format!("{base}/queue")).await)
        }
        "request_ai_review" => {
            let id = c.change_id(args).await?;
            pass(c.post(&format!("/v1/code/changes/{}/ai-review", seg(&id)), body_of(args, &["change"])).await)
        }
        other => fail(format!("unknown tool {other}")),
    })
}

/// Land results: landed and queued pass through; a conflict or blockers are outcomes too.
fn land_outcome(r: Api) -> Outcome {
    match r.status {
        s if s.is_success() => ok(r.body),
        StatusCode::CONFLICT if r.body.get("paths").is_some() => {
            let mut b = r.body;
            b["status"] = json!("conflict");
            Outcome { ok: true, text: None, data: b }
        }
        StatusCode::UNPROCESSABLE_ENTITY if r.body.get("blockers").is_some() => {
            let mut b = r.body;
            b["status"] = json!("blocked");
            Outcome { ok: true, text: None, data: b }
        }
        _ => r.error(),
    }
}

fn read_file_outcome(r: &Api, path: &str, rev: &str, args: &Value) -> Outcome {
    let binary = r.raw.contains(&0) || std::str::from_utf8(&r.raw).is_err();
    if binary {
        return ok(json!({ "path": path, "rev": rev, "binary": true, "content_type": r.content_type, "size": r.raw.len(), "content": null }));
    }
    let text = String::from_utf8_lossy(&r.raw);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = args.get("start_line").and_then(Value::as_u64).map(|v| v.max(1) as usize).unwrap_or(1);
    let asked_end = args.get("end_line").and_then(Value::as_u64).map(|v| v as usize);
    let end = asked_end.unwrap_or(start + READ_LINES - 1).min(total).min(start + 20 * READ_LINES - 1);
    let content = if start == 1 && end >= total { text.to_string() } else { lines.get(start.saturating_sub(1)..end.max(start - 1)).map(|l| l.join("\n")).unwrap_or_default() };
    let truncated = end < total;
    let mut data = json!({
        "path": path,
        "rev": rev,
        "binary": false,
        "content_type": r.content_type,
        "size": r.raw.len(),
        "total_lines": total,
        "start_line": start,
        "end_line": end,
        "truncated": truncated,
        "content": content,
    });
    if truncated {
        data["next"] = json!(format!("more: start_line={}", end + 1));
    }
    let header = if truncated { format!("{path} lines {start}-{end} of {total} (pass start_line={} for more)\n", end + 1) } else { String::new() };
    Outcome { ok: true, text: Some(format!("{header}{content}")), data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_segments() {
        assert_eq!(seg("a b/c"), "a%20b%2Fc");
        assert_eq!(path_segs("/src/a b.rs/"), "src/a%20b.rs");
        assert_eq!(query(&[("q", Some("a&b".into())), ("x", None)]), "?q=a%26b");
        assert_eq!(query(&[("x", None)]), "");
        assert!(repo_base("acme/api").is_ok());
        assert!(repo_base("acme").is_err());
        assert!(repo_base("a/b/c").is_err());
    }

    #[test]
    fn every_tool_has_a_closed_schema() {
        let tools = tools();
        assert_eq!(tools.len(), 22);
        for t in &tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
            for r in t["inputSchema"]["required"].as_array().unwrap() {
                assert!(t["inputSchema"]["properties"].get(r.as_str().unwrap()).is_some(), "{}: required {r} isn't a property", t["name"]);
            }
        }
    }
}
