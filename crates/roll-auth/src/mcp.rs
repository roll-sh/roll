//! MCP 2026-07-28's edge, shared by roll's two MCP servers (`/v1/mcp`, roll-mcp; roll code's own
//! `/v1/code/mcp`, code-control): a request is *modern* when its `_meta` (or its
//! `MCP-Protocol-Version` header) names this revision, and is served on its own, with no
//! `initialize` and no session. This is what such a request must carry, and what every answer to
//! one carries, so the two servers can't disagree about the revision.

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
pub const CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
/// How long a client may keep a list (`ttlMs`): tools follow the caller's rights, which can
/// change, so a minute.
const LIST_TTL_MS: u64 = 60_000;

/// The version a modern request names in its `_meta`, if it's one.
pub fn version(msg: &Value) -> Option<&str> {
    msg["params"]["_meta"][VERSION].as_str()
}

/// Whether a POST is of the modern revision: a message names a version in its `_meta`, or the
/// header names this one. Such a request is served by [`check`]'s rules, never a session's.
pub fn is_modern(h: &HeaderMap, messages: &[Value]) -> bool {
    messages.iter().any(|m| version(m).is_some()) || h.get("mcp-protocol-version").and_then(|v| v.to_str().ok()) == Some(MODERN)
}

/// A header's value, decoded from the `=?base64?…?=` form when it's in it (non-ASCII names).
pub fn header_value(h: &HeaderMap, name: &str) -> Option<String> {
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

/// The versions a server speaks: this one, then its handshake revisions (newest first).
pub fn supported<'a>(legacy: &[&'a str]) -> Vec<&'a str> {
    std::iter::once(MODERN).chain(legacy.iter().copied()).collect()
}

/// A version the server doesn't speak, as this revision answers it (with the ones it does).
pub fn unsupported(id: &Value, asked: &str, legacy: &[&str]) -> (StatusCode, Value) {
    error(StatusCode::BAD_REQUEST, id, UNSUPPORTED_VERSION, "Unsupported protocol version".into(), Some(json!({ "supported": supported(legacy), "requested": asked })))
}

/// Whether a modern request may be served: a version this server speaks, and HTTP headers that
/// say what the body says (`MCP-Protocol-Version`, `Mcp-Method`, and `Mcp-Name` for a call, a
/// read or a prompt), so a proxy routing on the headers and roll running the body agree. Otherwise
/// the HTTP status and JSON-RPC error to answer (with the request's id).
pub fn check(h: &HeaderMap, msg: &Value, legacy: &[&str]) -> Result<(), (StatusCode, Value)> {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let mismatch = |what: String| Err(error(StatusCode::BAD_REQUEST, &id, HEADER_MISMATCH, format!("Header mismatch: {what}"), None));
    let invalid = |what: &str| Err(error(StatusCode::BAD_REQUEST, &id, INVALID_PARAMS, format!("Invalid params: {what}"), None));
    let header = header_value(h, "mcp-protocol-version");
    // The header and the body disagreeing is a mismatch, whatever either says (SEP-2575); a body
    // with no version, or no `_meta`, is a malformed request, not an unknown version.
    let Some(asked) = version(msg) else {
        return invalid(&format!("a 2026-07-28 request carries its version in params._meta[{VERSION:?}]"));
    };
    if let Some(v) = &header
        && v != asked
    {
        return mismatch(format!("MCP-Protocol-Version {v:?} isn't the body's {asked:?}"));
    }
    if asked != MODERN {
        return Err(unsupported(&id, asked, legacy));
    }
    if !msg["params"]["_meta"][CAPABILITIES].is_object() {
        return invalid(&format!("a 2026-07-28 request declares what it can in params._meta[{CAPABILITIES:?}] (an object, empty if nothing)"));
    }
    if header.is_none() {
        return mismatch("MCP-Protocol-Version is missing".into());
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

/// A modern answer: every result says it's complete (unless it already says otherwise) and names
/// the server (`server`: its `name`, `title`, `version`, and on discovery its icons); a list says
/// how long it may be kept, for this caller only (a caller's tools are its own).
pub fn finish(method: &str, mut resp: Value, server: Value) -> Value {
    // A resource that isn't there is Invalid Params since this revision (SEP-2164); the handshake
    // revisions keep their -32002.
    if method == "resources/read" && resp["error"]["code"] == RESOURCE_NOT_FOUND_LEGACY {
        resp["error"]["code"] = json!(INVALID_PARAMS);
    }
    let Some(result) = resp.get_mut("result").and_then(Value::as_object_mut) else { return resp };
    result.entry("resultType").or_insert_with(|| json!("complete"));
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    if let Some(m) = meta.as_object_mut() {
        m.insert(SERVER_INFO.into(), server);
    }
    if matches!(method, "tools/list" | "prompts/list" | "resources/list" | "resources/templates/list" | "resources/read" | "server/discover") {
        result.insert("ttlMs".into(), json!(LIST_TTL_MS));
        result.insert("cacheScope".into(), json!("private"));
    }
    resp
}

/// `server/discover`: the versions spoken, what the server offers, and how to use it.
pub fn discover(legacy: &[&str], capabilities: Value, instructions: Value) -> Value {
    json!({ "supportedVersions": supported(legacy), "capabilities": capabilities, "instructions": instructions })
}
