//! Structured authorization denials (SEP-2643, a draft extension of the Fine-Grained Authorization
//! working group: `io.modelcontextprotocol/structured-authorization-denials`). A tool call roll's
//! gate refused carries an envelope in `_meta["io.modelcontextprotocol/authorization"]` (a
//! JSON-RPC error in its `data._meta`) saying whether there's a way forward, so a client can tell
//! "a person can approve this" from "this is final" without reading the text. Clients that don't
//! know the key ignore it. A retry may echo the approval in
//! `_meta["io.modelcontextprotocol/authorization-context-id"]`, read as the `approval` argument.
//!
//! Only the gate's own refusals are classified (its `code`s); a product's own 403 (a deploy
//! freeze) says nothing here. Not yet: the out-of-band interaction (a task held while a person
//! decides; roll's tasks wait on approvals but don't say so in the envelope).

use serde_json::{Value, json};

/// The extension, advertised with no settings.
pub const EXT: &str = "io.modelcontextprotocol/structured-authorization-denials";
/// Where the envelope goes in a result's `_meta` (or an error's `data._meta`).
pub const KEY: &str = "io.modelcontextprotocol/authorization";
/// What a retry echoes in its `_meta`.
pub const ECHO: &str = "io.modelcontextprotocol/authorization-context-id";

/// The envelope for a refused call's `structuredContent` (roll's answer with `http_status` and the
/// gate's `code`), or `None` when it isn't the gate's authorization denial. `url`: the answer asks
/// a person through a URL the client opens (URL elicitation, or 2026-07-28's input request).
pub fn envelope(s: &Value, url: bool) -> Option<Value> {
    let code = s["code"].as_str()?;
    let remediation = match code {
        // A person is asked: the client can open the page, or the model sends it again.
        "approval_pending" | "pending_approval" => {
            if url {
                "available"
            } else {
                "undisclosed"
            }
        }
        // Sent again, it asks again: the model can, nothing here lets the client.
        "approval_used" | "approval_expired" | "approval_mismatch" => "undisclosed",
        "forbidden" if s["step_up"].is_string() => "available",
        "forbidden" if s["widen"].is_string() || s["request"].is_object() => "undisclosed",
        "forbidden" | "denied" | "operator_only" | "would_ask" | "unfilterable" | "rotated_secret" | "ip_not_allowed" => "unavailable",
        _ => return None,
    };
    if s["http_status"] != 403 && s["status"] != "approval_pending" {
        return None;
    }
    let mut e = json!({ "reason": "insufficient_authorization", "remediation": remediation });
    if let Some(a) = s["approval"].as_str().filter(|a| !a.is_empty()) {
        e["authorizationContextId"] = json!(a);
    }
    if remediation == "available" && url {
        e["remediationHints"] = json!([{ "type": "url" }]);
    }
    Some(e)
}

/// Puts the envelope on a `tools/call` result (from its `structuredContent`), if it's a denial.
pub fn on_result(mut r: Value, url: bool) -> Value {
    if let Some(e) = envelope(&r["structuredContent"], url) {
        r["_meta"][KEY] = e;
    }
    r
}

/// The same on a JSON-RPC error answering a call (`data._meta`), from the call's result.
pub fn on_error(mut resp: Value, result: &Value, url: bool) -> Value {
    if let Some(e) = envelope(&result["structuredContent"], url) {
        resp["error"]["data"]["_meta"][KEY] = e;
    }
    resp
}

/// A retry's echoed context id, as the call's `approval` when it names none (an approval id only:
/// the gate checks it's this grant's, for this very request).
pub fn echo(params: &Value, args: &mut Value) {
    let Some(id) = params["_meta"][ECHO].as_str() else { return };
    if id.starts_with("apr_") && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') && args.is_object() && args.get("approval").is_none() {
        args["approval"] = json!(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each of the gate's refusals says whether there's a way forward, and only the gate's do.
    #[test]
    fn denials_say_whether_theres_a_way_forward() {
        let at = |s: Value, url: bool| envelope(&s, url).map(|e| (e["remediation"].as_str().unwrap().to_string(), e["remediationHints"].clone(), e["authorizationContextId"].clone()));
        let pending = json!({ "code": "approval_pending", "status": "approval_pending", "http_status": 403, "approval": "apr_1" });
        assert_eq!(at(pending.clone(), true), Some(("available".into(), json!([{ "type": "url" }]), json!("apr_1"))));
        assert_eq!(at(pending, false), Some(("undisclosed".into(), Value::Null, json!("apr_1"))));
        assert_eq!(at(json!({ "code": "denied", "http_status": 403, "approval": "apr_2" }), true).unwrap().0, "unavailable");
        assert_eq!(at(json!({ "code": "approval_used", "http_status": 403 }), true).unwrap().0, "undisclosed");
        assert_eq!(at(json!({ "code": "forbidden", "http_status": 403, "step_up": "stp_1" }), false).unwrap().0, "available");
        assert_eq!(at(json!({ "code": "forbidden", "http_status": 403, "widen": "https://roll/x" }), false).unwrap().0, "undisclosed");
        assert_eq!(at(json!({ "code": "forbidden", "http_status": 403 }), false).unwrap().0, "unavailable");
        assert_eq!(at(json!({ "code": "frozen", "http_status": 403 }), false), None, "a product's own 403");
        assert_eq!(at(json!({ "code": "forbidden", "http_status": 404 }), false), None);
        let r = on_result(json!({ "isError": true, "structuredContent": { "code": "denied", "http_status": 403 } }), false);
        assert_eq!(r["_meta"][KEY]["remediation"], "unavailable");
        let mut args = json!({ "id": "pa" });
        echo(&json!({ "_meta": { ECHO: "apr_9" } }), &mut args);
        assert_eq!(args["approval"], "apr_9");
        let mut args = json!({ "approval": "apr_1" });
        echo(&json!({ "_meta": { ECHO: "apr_9" } }), &mut args);
        assert_eq!(args["approval"], "apr_1", "the call's own approval stands");
        let mut args = json!({});
        echo(&json!({ "_meta": { ECHO: "x\r\nevil" } }), &mut args);
        assert!(args.get("approval").is_none());
    }
}
