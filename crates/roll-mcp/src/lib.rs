//! roll's MCP server, `/v1/mcp` (docs/mcp.md): the Model Context Protocol's Streamable HTTP
//! transport, JSON-RPC 2.0, with tools for every product.
//!
//! Every tool is one request through rolld's API router, gate included, with the caller's own
//! `Authorization`: a scoped token's rules decide each call exactly as they decide the same request
//! on the HTTP API or from the CLI. Tools can't drift from the routes ([`tools`] names the route
//! each one calls; a test checks it's in the permission catalogue), and their annotations
//! (`readOnlyHint`, `destructiveHint`) come from the action the catalogue gives that route.
//!
//! - Stateless: no `Mcp-Session-Id` (the spec allows it), so any node answers any request.
//! - Responses are JSON, or server-sent events when the client accepts only `text/event-stream`
//!   (keep-alive comments while a call waits on an approval).
//! - Without a token on a daemon that needs one: 401 with `WWW-Authenticate` naming the protected
//!   resource metadata, so clients start the OAuth flow (`roll_access::oauth`).
//! - Requests from a browser (`Origin`) must come from the dashboard or the API (DNS rebinding).

pub mod tools;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Extensions, HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures::StreamExt;
use roll_access::evaluate::McpCall;
use roll_auth::scope::Action;
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tower::ServiceExt;

/// Protocol versions spoken, newest first.
pub const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const SSE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);
/// An answer read at most.
const MAX_ANSWER: usize = 32 << 20;
/// Text returned to the model at most; longer answers are cut (the structured content stays whole).
const MAX_TEXT: usize = 256 << 10;

/// Tools served by another crate (roll code's, `code_control::mcp`), fronted under new names.
pub trait ToolSet: Send + Sync + 'static {
    /// Each tool's MCP definition, named as this server lists it; `_meta["roll/product"]` says
    /// which product it belongs to.
    fn tools(&self) -> Vec<Value>;
    /// Run a tool by the name [`ToolSet::tools`] gave it: requests through `api`, with `headers`,
    /// and `extensions` on each.
    fn call(&self, api: Router, headers: HeaderMap, extensions: Extensions, name: String, args: Value) -> Pin<Box<dyn Future<Output = Value> + Send>>;
}

pub struct Config {
    /// The API's public origin: the protected resource metadata's base.
    pub api_url: String,
    /// The dashboard's origin: browsers may call from it.
    pub web_url: String,
    /// The operator's token is required (an anonymous call gets 401).
    pub locked: bool,
    /// Products that are on (`roll_auth::scope::PRODUCTS` names): only their tools are listed.
    pub products: Vec<String>,
    /// Where the operator's tool calls are audited (a grant's are audited by the gate).
    pub access: Option<Arc<roll_access::Access>>,
}

#[derive(Clone)]
struct Mcp {
    api: Router,
    config: Arc<Config>,
    sets: Arc<Vec<Arc<dyn ToolSet>>>,
}

/// The `/v1/mcp` route. `api` is rolld's gated API router: tool calls go through it.
pub fn routes(api: Router, config: Config, sets: Vec<Arc<dyn ToolSet>>) -> Router {
    let state = Mcp { api, config: Arc::new(config), sets: Arc::new(sets) };
    Router::new().route("/v1/mcp", post(handle).get(not_here).delete(not_here)).with_state(state)
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

/// Stateless: no server-initiated stream to open (GET) and no session to end (DELETE).
async fn not_here() -> Response {
    let mut r = (StatusCode::METHOD_NOT_ALLOWED, "this MCP server is stateless: POST JSON-RPC requests").into_response();
    r.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
    r
}

fn origin_ok(c: &Config, h: &HeaderMap) -> bool {
    let Some(origin) = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return true };
    let of = |u: &str| url::Url::parse(u).ok().map(|u| u.origin().ascii_serialization());
    let origin = of(origin);
    origin.is_some() && [of(&c.web_url), of(&c.api_url)].contains(&origin)
}

/// Who's calling, as the gate left it.
enum Principal {
    Operator,
    Grant(roll_auth::Grant),
}

fn unauthorized(c: &Config) -> Response {
    let mut r = http_error(StatusCode::UNAUTHORIZED, "this MCP server needs a token: connect with OAuth, or send Authorization: Bearer rlt_…");
    let meta = format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource/v1/mcp\"", c.api_url.trim_end_matches('/'));
    if let Ok(v) = HeaderValue::from_str(&meta) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    r
}

async fn handle(State(m): State<Mcp>, ext: Extensions, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !origin_ok(&m.config, &headers) {
        return http_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }
    let principal = match roll_auth::Grant::on(&ext) {
        Some(g) => Principal::Grant(g.clone()),
        None if roll_auth::Operator::on(&ext) || !m.config.locked => Principal::Operator,
        None => return unauthorized(&m.config),
    };
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(rpc_error(Value::Null, -32700, format!("parse error: {e}")))).into_response(),
    };
    let batch = msg.is_array();
    let messages: Vec<Value> = match msg {
        Value::Array(a) if a.is_empty() => return http_error(StatusCode::BAD_REQUEST, "empty batch"),
        Value::Array(a) => a,
        v => vec![v],
    };
    if let Some(v) = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok())
        && !PROTOCOL_VERSIONS.contains(&v)
    {
        return http_error(StatusCode::BAD_REQUEST, &format!("unsupported MCP-Protocol-Version {v}"));
    }
    let has_requests = messages.iter().any(|v| v.get("method").is_some() && v.get("id").is_some());
    if !has_requests {
        return StatusCode::ACCEPTED.into_response();
    }
    // The client's address as the gate sees it (behind roll's edge, the forwarded one): tool
    // calls carry it, for IP allowlists and the audit log.
    let trust = m.config.access.as_ref().is_some_and(|a| a.config.trust_forwarded_for);
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()).and_then(|v| v.split(',').next()).and_then(|v| v.trim().parse().ok());
    let ip = match (trust, forwarded) {
        (true, Some(ip)) => Some(ip),
        _ => ext.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip()),
    };
    // A user's own token (the gate signed them in): only the products their account reaches.
    let as_user = ext.get::<roll_auth::SignedIn>().is_some_and(|u| !u.admin);
    let ctx = Arc::new(Ctx { m, headers, principal, ip, as_user });
    if wants_sse(&ctx.headers) {
        let stream = futures::stream::once(async move { process(&ctx, messages).await }).flat_map(|responses| {
            futures::stream::iter(responses.into_iter().map(|r| Ok::<_, std::convert::Infallible>(Event::default().event("message").data(r.to_string()))))
        });
        return Sse::new(stream).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE)).into_response();
    }
    let mut responses = process(&ctx, messages).await;
    if batch {
        Json(Value::Array(responses)).into_response()
    } else {
        Json(responses.pop().unwrap_or(Value::Null)).into_response()
    }
}

fn wants_sse(h: &HeaderMap) -> bool {
    let accept = h.get_all(header::ACCEPT).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",").to_ascii_lowercase();
    accept.contains("text/event-stream") && !accept.contains("application/json")
}

struct Ctx {
    m: Mcp,
    headers: HeaderMap,
    principal: Principal,
    ip: Option<std::net::IpAddr>,
    /// A signed-in user's own token: tools only for what their account reaches.
    as_user: bool,
}

impl Ctx {
    /// Whether the caller might use a tool for this product and these actions at all.
    fn may(&self, product: &str, actions: &[Action]) -> bool {
        let on = product == "access" || product == "workspace" || self.m.config.products.iter().any(|p| p == product);
        let reached = !self.as_user || product == "access" || roll_access::USER_REACH.contains(&product);
        on && reached && match &self.principal {
            Principal::Operator => true,
            Principal::Grant(g) => actions.iter().any(|a| g.scope.may(product, *a)),
        }
    }

    fn usable(&self, t: &tools::Tool) -> bool {
        if t.open() {
            return true;
        }
        self.may(t.product, &t.actions())
    }

    /// Every tool this caller can use, as `tools/list` shows it.
    fn tools(&self) -> Vec<Value> {
        let mut out: Vec<Value> = tools::all().iter().filter(|t| self.usable(t)).map(tools::Tool::definition).collect();
        for set in self.m.sets.iter() {
            for t in set.tools() {
                let product = t["_meta"]["roll/product"].as_str().unwrap_or("code").to_string();
                let read_only = t["annotations"]["readOnlyHint"].as_bool() == Some(true);
                let actions: &[Action] = if read_only { &[Action::Read] } else { &[Action::Write, Action::Deploy, Action::Admin] };
                if self.may(&product, actions) {
                    out.push(t);
                }
            }
        }
        out
    }
}

async fn process(ctx: &Arc<Ctx>, messages: Vec<Value>) -> Vec<Value> {
    let mut responses = Vec::new();
    for msg in messages {
        if msg.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            responses.push(rpc_error(msg.get("id").cloned().unwrap_or(Value::Null), -32600, "jsonrpc must be \"2.0\""));
            continue;
        }
        let (Some(method), Some(id)) = (msg.get("method").and_then(Value::as_str), msg.get("id").cloned()) else { continue };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let resp = match method {
            "initialize" => rpc_result(id, initialize(ctx, &params)),
            "ping" => rpc_result(id, json!({})),
            "tools/list" => rpc_result(id, json!({ "tools": ctx.tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                if !args.is_object() {
                    rpc_error(id, -32602, "arguments must be an object")
                } else {
                    match call(ctx, &name, args).await {
                        Some(r) => rpc_result(id, r),
                        None => rpc_error(id, -32602, format!("unknown tool {name:?}")),
                    }
                }
            }
            "resources/list" => rpc_result(id, json!({ "resources": [] })),
            "resources/templates/list" => rpc_result(id, json!({ "resourceTemplates": [] })),
            "prompts/list" => rpc_result(id, json!({ "prompts": [] })),
            other => rpc_error(id, -32601, format!("method not found: {other}")),
        };
        responses.push(resp);
    }
    responses
}

fn initialize(ctx: &Ctx, params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
    let who = match &ctx.principal {
        Principal::Operator => "You're connected as roll's operator: every tool is available.".to_string(),
        Principal::Grant(g) => format!(
            "You're connected with the token \"{}\": it allows only what its owner chose, and tools/list shows only tools it can use. A call outside its rules fails with \"not allowed\": ask the user to widen the token rather than retrying.",
            g.name
        ),
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") },
        "instructions": format!(
            "roll: serverless Postgres, Redis, storage, streams, queues, tasks, NoSQL, source control (roll code) and coding agents, grouped into workspace projects. \
Tools are named <product>_<verb>. Ids are each product's own (Postgres projects prj_…, Redis rdb_…, streams st_…, buckets by name, repos owner/name). \
{who} Some actions ask a person first: such a call waits up to a minute, then returns status \"approval_pending\" with an approval id and a link. \
Tell the user, then call the same tool again with the same arguments plus \"approval\": \"apr_…\": it waits for the decision and runs once approved. \
postgres_query is read-only (postgres_execute changes data). Credentials come back as [redacted] unless the token may reveal them."
        ),
    })
}

/// Run a tool: one of ours, or a fronted set's.
async fn call(ctx: &Arc<Ctx>, name: &str, args: Value) -> Option<Value> {
    let redacted = roll_access::redact::value(&args);
    let mut ext = Extensions::new();
    ext.insert(McpCall { tool: name.into(), args: Arc::new(redacted), ip: ctx.ip });
    let started = std::time::Instant::now();
    // The gate decides (and audits) every call a grant makes; the MCP layer refuses nothing itself.
    let (result, route) = if let Some(t) = tools::all().iter().find(|t| t.name == name) {
        (run(ctx, t, &args, ext).await, format!("{} {}", t.method, t.path))
    } else {
        let set = ctx.m.sets.iter().find(|s| s.tools().iter().any(|t| t["name"] == name))?;
        let headers = forward_headers(&ctx.headers, &args);
        (set.call(ctx.m.api.clone(), headers, ext, name.to_string(), args.clone()).await, "tool".to_string())
    };
    // The operator's calls don't pass the evaluator: the audit row is written here.
    if let (Principal::Operator, Some(access)) = (&ctx.principal, &ctx.m.config.access) {
        let status = result["structuredContent"]["http_status"].as_u64().unwrap_or(if result["isError"] == true { 400 } else { 200 }) as u16;
        let e = roll_access::store::AuditEvent {
            id: roll_access::new_id("aud"),
            at: code_sql_now(),
            grant_id: None,
            grant_name: None,
            source: "mcp".into(),
            tool: Some(name.to_string()),
            method: "POST".into(),
            route,
            product: name.split('_').next().map(str::to_string),
            action: None,
            resource: None,
            decision: "allow".into(),
            approval: None,
            status,
            args: Some(roll_access::redact::value(&args)),
            ip: ctx.ip.map(|i| i.to_string()),
            duration_ms: started.elapsed().as_millis() as i64,
        };
        let access = access.clone();
        let _ = tokio::task::spawn_blocking(move || access.store.audit(&e)).await;
    }
    Some(result)
}

fn code_sql_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The caller's headers a tool call carries: its token, who it reviews as, and an approval.
fn forward_headers(h: &HeaderMap, args: &Value) -> HeaderMap {
    let mut out = HeaderMap::new();
    for name in ["authorization", "x-roll-agent", "x-roll-user"] {
        if let Some(v) = h.get(name) {
            out.insert(name, v.clone());
        }
    }
    if let Some(a) = args.get("approval").and_then(Value::as_str)
        && let Ok(v) = HeaderValue::from_str(a)
    {
        out.insert(roll_access::evaluate::APPROVAL_HEADER, v);
    }
    out
}

fn tool_error(text: String, data: Value) -> Value {
    let mut d = if data.is_object() { data } else { json!({}) };
    d["error"] = json!(text);
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": d, "isError": true })
}

fn cut(text: String) -> String {
    if text.len() <= MAX_TEXT {
        return text;
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… ({} bytes; the structured content has all of it)", &text[..end], text.len())
}

async fn run(ctx: &Arc<Ctx>, t: &tools::Tool, args: &Value, ext: Extensions) -> Value {
    let built = match t.request(args) {
        Ok(b) => b,
        Err(e) => return tool_error(e, json!({ "code": "invalid_arguments" })),
    };
    let mut req = Request::builder().method(t.method).uri(&built.uri);
    for (k, v) in forward_headers(&ctx.headers, args).iter() {
        req = req.header(k, v);
    }
    for (k, v) in &built.headers {
        req = req.header(*k, v);
    }
    let req = match built.body {
        tools::Built::None => req.body(Body::empty()),
        tools::Built::Json(v) => req.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())),
        tools::Built::Raw(b, ct) => req.header(header::CONTENT_TYPE, ct).body(Body::from(b)),
    };
    let mut req = match req {
        Ok(r) => r,
        Err(e) => return tool_error(format!("couldn't build the request: {e}"), json!({})),
    };
    req.extensions_mut().extend(ext);
    let resp = match ctx.m.api.clone().oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    };
    outcome(t.name, resp).await
}

/// A tool's MCP result from its route's answer.
async fn outcome(tool: &str, resp: Response) -> Value {
    let status = resp.status();
    let ct = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_ANSWER).await.unwrap_or_default();
    let json: Option<Value> = if ct.starts_with("application/json") || ct.is_empty() { serde_json::from_slice(&bytes).ok() } else { None };
    if !status.is_success() {
        let body = json.unwrap_or_else(|| json!({ "error": String::from_utf8_lossy(&bytes).trim() }));
        let code = body.get("code").and_then(Value::as_str).unwrap_or("");
        let msg = body.get("error").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| status.to_string());
        let mut data = if body.is_object() { body.clone() } else { json!({}) };
        data["http_status"] = json!(status.as_u16());
        // Waiting on a person is an outcome the agent acts on, not a failure.
        if code == "approval_pending" {
            data["status"] = json!("approval_pending");
            let text = format!(
                "This needs the user's approval ({}). Tell them, then call {tool} again with the same arguments and \"approval\": \"{}\" to wait for the decision.",
                body["url"].as_str().unwrap_or("the roll dashboard"),
                body["approval"].as_str().unwrap_or("")
            );
            return json!({ "content": [{ "type": "text", "text": text }], "structuredContent": data, "isError": false });
        }
        let text = match status {
            StatusCode::UNAUTHORIZED => format!("{msg}: the token isn't valid any more (reconnect)"),
            StatusCode::FORBIDDEN => format!("not allowed: {msg}"),
            StatusCode::NOT_FOUND => format!("not found: {msg}"),
            _ => msg,
        };
        return tool_error(text, data);
    }
    let (text, structured) = match json {
        Some(v) => (cut(serde_json::to_string_pretty(&v).unwrap_or_default()), if v.is_object() { v } else { json!({ "items": v }) }),
        None if bytes.is_empty() => ("done".to_string(), json!({ "ok": true })),
        None => match std::str::from_utf8(&bytes) {
            Ok(s) => (cut(s.to_string()), json!({ "content_type": ct, "bytes": bytes.len() })),
            Err(_) => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                (format!("{} bytes of {ct} (base64 in structuredContent.content_b64)", bytes.len()), json!({ "content_type": ct, "bytes": bytes.len(), "content_b64": b64 }))
            }
        },
    };
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
}
