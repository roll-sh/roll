//! MCP 2026-07-28, the stateless core, beside the handshake revisions (docs/mcp.md, "Protocol"): a
//! request is *modern* when its `_meta` names its protocol version, and is served on its own,
//! with no `initialize` and no session. roll was stateless underneath already (the session id only
//! carried what the client declared), so this is the edge: what a modern request must carry, what
//! its answers carry, and approvals as Multi Round-Trip Requests. A legacy client that opens with
//! `initialize` is served as before (a dual-era server, as the spec's compatibility matrix has it).
//!
//! The rules every roll MCP server shares (the request's checks, the answers' fields, discovery)
//! are `roll_auth::mcp`, which roll code's own `/v1/code/mcp` serves too; this adds what only
//! `/v1/mcp` has: elicitation, routable arguments and approvals.

use axum::http::HeaderMap;
use serde_json::{Value, json};

pub use roll_auth::mcp::{CAPABILITIES, HEADER_MISMATCH, MODERN, UNSUPPORTED_VERSION, check, discover, version};
use roll_auth::mcp::{CLIENT_INFO, header_value};

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

/// A modern answer (`roll_auth::mcp::finish`), naming roll; discovery is where a client gets what
/// to show for the server (its icon, as `initialize` gives a handshake client), so every other
/// answer names it without the image.
pub fn finish(method: &str, resp: Value) -> Value {
    let mut info = json!({ "name": "roll", "title": "roll", "version": env!("CARGO_PKG_VERSION") });
    if method == "server/discover" {
        info["icons"] = json!([crate::server_icon()]);
    }
    roll_auth::mcp::finish(method, resp, info)
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
