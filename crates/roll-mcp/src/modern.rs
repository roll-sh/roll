//! MCP 2026-07-28, the stateless core, beside the handshake revisions (docs/mcp.md, "Protocol"): a
//! request is *modern* when its `_meta` names its protocol version, and is served on its own,
//! with no `initialize` and no session. roll was stateless underneath already (the session id only
//! carried what the client declared), so this is the edge: what a modern request must carry, what
//! its answers carry, and approvals as Multi Round-Trip Requests. A legacy client that opens with
//! `initialize` is served as before (a dual-era server, as the spec's compatibility matrix has it).

use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};

/// The stateless revision.
pub const MODERN: &str = "2026-07-28";
/// The HTTP headers don't match the body, or one it needs is missing.
pub const HEADER_MISMATCH: i64 = -32020;
/// A protocol version this server doesn't speak.
pub const UNSUPPORTED_VERSION: i64 = -32022;
/// "Resource not found" before this revision; Invalid Params (-32602) in it.
const RESOURCE_NOT_FOUND_LEGACY: i64 = -32002;
const INVALID_PARAMS: i64 = -32602;
const VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// How long a client may keep a list (`ttlMs`): tools follow the token's rules, which can change,
/// so a minute.
const LIST_TTL_MS: u64 = 60_000;

/// The version a modern request names in its `_meta`, if it's one.
pub fn version(msg: &Value) -> Option<&str> {
    msg["params"]["_meta"][VERSION].as_str()
}

/// Whether the request's client fills in forms for elicitation (`elicitation.form`, or an empty
/// `elicitation`, which is forms).
pub fn form_elicitation(msg: &Value) -> bool {
    let e = &msg["params"]["_meta"][CAPABILITIES]["elicitation"];
    e.get("form").is_some() || e.as_object().is_some_and(|o| o.is_empty())
}

/// What the client declared on this request: whether it opens URLs for elicitation, and its name.
pub fn client(msg: &Value) -> (bool, Option<String>) {
    let meta = &msg["params"]["_meta"];
    let url = meta[CAPABILITIES]["elicitation"].get("url").is_some();
    let info = &meta[CLIENT_INFO];
    let name = info["name"].as_str().and_then(|n| roll_access::evaluate::client_name(&format!("{n} {}", info["version"].as_str().unwrap_or(""))));
    (url, name)
}

/// A header's value, decoded from the `=?base64?…?=` form when it's in it (non-ASCII names).
/// The tool arguments a gateway may route on, mirrored into `Mcp-Param-{name}` headers for a
/// 2026-07-28 client (SEP-2243, `x-mcp-header`): a workspace project, an environment, a service.
const PARAM_HEADERS: &[(&str, &str)] = &[("project", "Project"), ("env", "Environment"), ("service", "Service")];

/// A tool's arguments that carry `x-mcp-header`, with their header names: string properties at the
/// schema's top level named in [`PARAM_HEADERS`].
pub fn param_headers(tool: &Value) -> Vec<(&'static str, &'static str)> {
    let props = &tool["inputSchema"]["properties"];
    PARAM_HEADERS.iter().copied().filter(|(p, _)| props[*p]["type"] == "string").collect()
}

/// Mark each listed tool's routable arguments with `x-mcp-header` (a 2026-07-28 client mirrors
/// them into headers, and the call is checked against them).
pub fn annotate(tools: &mut [Value]) {
    for t in tools {
        for (p, h) in param_headers(t) {
            t["inputSchema"]["properties"][p]["x-mcp-header"] = Value::String(h.into());
        }
    }
}

/// Whether a call's `Mcp-Param-*` headers say what its arguments do: a value given has its header
/// (decoded from the Base64 form), and a header names a value given. Else why not.
pub fn check_params(tool: &Value, args: &Value, h: &HeaderMap) -> Result<(), String> {
    for (p, name) in param_headers(tool) {
        let header = format!("mcp-param-{}", name.to_ascii_lowercase());
        let sent = h.contains_key(header.as_str()).then(|| header_value(h, &header));
        match (args[p].as_str(), sent) {
            (None, None) => {}
            (Some(v), Some(Some(hv))) if hv == v => {}
            (Some(_), None) => return Err(format!("Mcp-Param-{name} is missing for argument {p}")),
            (None, Some(_)) => return Err(format!("Mcp-Param-{name} names an argument {p} the call doesn't have")),
            _ => return Err(format!("Mcp-Param-{name} isn't the body's {p}")),
        }
    }
    Ok(())
}

fn header_value(h: &HeaderMap, name: &str) -> Option<String> {
    use base64::Engine;
    let v = h.get(name)?.to_str().ok()?;
    match v.strip_prefix("=?base64?").and_then(|r| r.strip_suffix("?=")) {
        Some(b64) => String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64).ok()?).ok(),
        None => Some(v.to_string()),
    }
}

fn error(status: StatusCode, id: &Value, code: i64, message: String, data: Option<Value>) -> (StatusCode, Value) {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    (status, json!({ "jsonrpc": "2.0", "id": id, "error": e }))
}

/// Whether a modern request may be served: a version this server speaks, and HTTP headers that
/// say what the body says (`MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name` for a call, a
/// read or a prompt), so a proxy routing on the headers and roll running the body agree. Otherwise
/// the HTTP status and JSON-RPC error to answer.
pub fn check(h: &HeaderMap, msg: &Value, legacy: &[&str]) -> Result<(), (StatusCode, Value)> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let asked = version(msg).unwrap_or_default();
    if asked != MODERN {
        let supported: Vec<&str> = std::iter::once(MODERN).chain(legacy.iter().copied()).collect();
        return Err(error(StatusCode::BAD_REQUEST, &id, UNSUPPORTED_VERSION, "Unsupported protocol version".into(), Some(json!({ "supported": supported, "requested": asked }))));
    }
    let mismatch = |what: String| Err(error(StatusCode::BAD_REQUEST, &id, HEADER_MISMATCH, format!("Header mismatch: {what}"), None));
    match header_value(h, "mcp-protocol-version") {
        Some(v) if v == asked => {}
        Some(v) => return mismatch(format!("MCP-Protocol-Version {v:?} isn't the body's {asked:?}")),
        None => return mismatch("MCP-Protocol-Version is missing".into()),
    }
    let method = msg["method"].as_str().unwrap_or_default();
    // A notification (no id) may come without it: the TypeScript SDK sends `notifications/cancelled`
    // so, and there's no answer for a proxy to route.
    match header_value(h, "mcp-method") {
        Some(v) if v == method => {}
        Some(v) => return mismatch(format!("Mcp-Method {v:?} isn't the body's {method:?}")),
        None if msg.get("id").is_none() => {}
        None => return mismatch("Mcp-Method is missing".into()),
    }
    let named = match method {
        "tools/call" | "prompts/get" | "events/poll" | "events/stream" | "events/subscribe" | "events/unsubscribe" => msg["params"]["name"].as_str(),
        "resources/read" => msg["params"]["uri"].as_str(),
        // The tasks extension's methods name their task (SEP-2663, over Streamable HTTP).
        "tasks/get" | "tasks/update" | "tasks/cancel" => msg["params"]["taskId"].as_str(),
        _ => None,
    };
    if let Some(name) = named {
        match header_value(h, "mcp-name") {
            Some(v) if v == name => {}
            Some(v) => return mismatch(format!("Mcp-Name {v:?} isn't the body's {name:?}")),
            None => return mismatch("Mcp-Name is missing".into()),
        }
    }
    Ok(())
}

/// A modern answer: every result says it's complete (or, an approval's, that it needs input), and
/// names the server; a list says how long it may be kept, for this caller only (a token's tools are
/// its own). A method the revision doesn't have is answered 404 by the caller.
pub fn finish(method: &str, mut resp: Value) -> Value {
    // A resource that isn't there is Invalid Params since this revision (SEP-2164); the handshake
    // revisions keep their -32002.
    if method == "resources/read" && resp["error"]["code"] == RESOURCE_NOT_FOUND_LEGACY {
        resp["error"]["code"] = json!(INVALID_PARAMS);
    }
    let Some(result) = resp.get_mut("result").and_then(Value::as_object_mut) else { return resp };
    result.entry("resultType").or_insert_with(|| json!("complete"));
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    if let Some(m) = meta.as_object_mut() {
        let mut info = json!({ "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") });
        // Discovery is where a client gets what to show for the server (its icon, as `initialize`
        // gives a handshake client); every other answer names it without the image.
        if method == "server/discover" {
            info["icons"] = json!([crate::server_icon()]);
        }
        m.insert(SERVER_INFO.into(), info);
    }
    if matches!(method, "tools/list" | "prompts/list" | "resources/list" | "resources/templates/list" | "resources/read" | "server/discover") {
        result.insert("ttlMs".into(), json!(LIST_TTL_MS));
        result.insert("cacheScope".into(), json!("private"));
    }
    resp
}

/// `server/discover`: the versions spoken, what the server offers, and how to use it.
pub fn discover(legacy: &[&str], capabilities: Value, instructions: Value) -> Value {
    let supported: Vec<&str> = std::iter::once(MODERN).chain(legacy.iter().copied()).collect();
    json!({ "supportedVersions": supported, "capabilities": capabilities, "instructions": instructions })
}

/// A call waiting on a person, as a Multi Round-Trip Request: the approval page for the client to
/// open, and the same call to send again (the gate finds its approval: one per request, so no
/// `requestState` is needed, and nothing the client echoes can change what runs).
pub fn approval_input_required(result: &Value) -> Value {
    let s = &result["structuredContent"];
    let url = s["url"].as_str().unwrap_or_default();
    let message = format!(
        "roll needs a person to approve this ({}). Open the page to approve or deny it; the call runs when it's sent again after approval.",
        s["error"].as_str().unwrap_or("approval")
    );
    json!({
        "resultType": "input_required",
        "inputRequests": { "approval": { "method": "elicitation/create", "params": { "mode": "url", "message": message, "url": url } } },
    })
}
