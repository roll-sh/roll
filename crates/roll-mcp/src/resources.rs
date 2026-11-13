//! MCP resources (docs/mcp.md, "Resources"): what an agent can read before it acts.
//!
//! - `roll://guide`: how to use roll over MCP (ids, approvals, failures), a static page.
//! - `roll://access/me`: what this connection may do, in words, read from `GET /v1/access`
//!   through the gated router like any tool, so it can't drift from what the gate decides.

use roll_auth::scope::{Resources, Rule, Selector};
use serde_json::{Value, json};

pub const GUIDE: &str = "roll://guide";
pub const ME: &str = "roll://access/me";
/// The route `roll://access/me` reads.
pub const ME_ROUTE: &str = "/v1/access";

pub fn list() -> Value {
    json!([
        {
            "uri": GUIDE,
            "name": "guide",
            "title": "Using roll over MCP",
            "description": "Ids, approvals, and what to do when a call is refused. Read it first.",
            "mimeType": "text/markdown",
        },
        {
            "uri": ME,
            "name": "permissions",
            "title": "What this connection may do",
            "description": "This token's rules in words: products, actions, resources, environments, and what happens to the rest.",
            "mimeType": "text/markdown",
        },
    ])
}

pub fn guide() -> Value {
    json!({ "contents": [{ "uri": GUIDE, "mimeType": "text/markdown", "text": include_str!("guide.md") }] })
}

/// `roll://access/me` from `GET /v1/access`'s answer: words first, the grant as JSON after.
pub fn me(answer: &Value, now: i64) -> Value {
    json!({ "contents": [
        { "uri": ME, "mimeType": "text/markdown", "text": words(answer, now) },
        { "uri": ME, "mimeType": "application/json", "text": answer.to_string() },
    ] })
}

fn words(answer: &Value, now: i64) -> String {
    let g = &answer["grant"];
    if answer["kind"] == "operator" || g.is_null() {
        return "You're connected as roll's operator: every tool and every action is allowed.\n".into();
    }
    let rules = |v: &Value| serde_json::from_value::<Vec<Rule>>(v.clone()).unwrap_or_default();
    let mut out = format!("You're connected with the token \"{}\". It may:\n\n", g["name"].as_str().unwrap_or(""));
    let permanent = rules(&g["rules"]);
    if permanent.is_empty() {
        out.push_str("- nothing on its own\n");
    }
    for r in &permanent {
        out.push_str(&format!("- {}\n", rule(r)));
    }
    let temporary: Vec<_> = g["temporary"].as_array().into_iter().flatten().filter(|t| t["until"].as_i64().unwrap_or(0) > now).collect();
    if !temporary.is_empty() {
        out.push_str("\nFor a while, because a person approved it:\n\n");
        for t in temporary {
            if let Ok(r) = serde_json::from_value::<Rule>(t["rule"].clone()) {
                out.push_str(&format!("- {} (for {})\n", rule(&r), span(t["until"].as_i64().unwrap_or(now) - now)));
            }
        }
    }
    out.push_str(if g["ask_otherwise"] == true {
        "\nAnything else waits for a person's approval (the call returns `approval_pending`).\n"
    } else {
        "\nAnything else is refused: ask the user to widen the token rather than trying another way.\n"
    });
    let mut limits = Vec::new();
    if let Some(at) = g["expires_at"].as_i64() {
        limits.push(format!("it expires in {}", span(at - now)));
    }
    if let Some(n) = g["rate_per_minute"].as_u64() {
        limits.push(format!("it may make {n} requests a minute"));
    }
    if g["ip_allow"].as_array().is_some_and(|a| !a.is_empty()) {
        limits.push("it works only from the addresses its owner chose".into());
    }
    if !limits.is_empty() {
        out.push_str(&format!("\nAlso: {}.\n", limits.join("; ")));
    }
    out
}

/// One rule in words: "postgres: read; delete after a person approves, on project p1, only in production".
fn rule(r: &Rule) -> String {
    let product = if r.product == "*" { "every product".to_string() } else { r.product.clone() };
    let mut allowed = Vec::new();
    let mut asked = Vec::new();
    for (a, e) in &r.actions {
        let name = serde_json::to_value(a).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        match e {
            roll_auth::scope::Effect::Allow => allowed.push(name),
            roll_auth::scope::Effect::Ask => asked.push(name),
        }
    }
    let mut actions = Vec::new();
    if !allowed.is_empty() {
        actions.push(allowed.join(", "));
    }
    if !asked.is_empty() {
        actions.push(format!("{} after a person approves", asked.join(", ")));
    }
    let on = match &r.resources {
        Resources::All => "every resource, including new ones".to_string(),
        Resources::Some(s) => s
            .iter()
            .map(|s| match s {
                Selector::Project { project } => format!("project {project}"),
                Selector::Id { id } => format!("`{id}`"),
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    let mut out = format!("{product}: {}, on {on}", actions.join("; "));
    if let Some(envs) = r.environments.as_ref().filter(|e| !e.is_empty()) {
        out.push_str(&format!(", only in {}", envs.join(", ")));
    }
    if let Some(paths) = r.paths.as_ref().filter(|p| !p.is_empty()) {
        out.push_str(&format!(", only under {}", paths.join(", ")));
    }
    out
}

/// A duration in milliseconds, roughly: "40 minutes", "5 hours", "3 days".
fn span(ms: i64) -> String {
    let m = (ms.max(0) + 59_999) / 60_000;
    let (n, unit) = if m < 120 { (m, "minute") } else if m < 48 * 60 { (m / 60, "hour") } else { (m / (24 * 60), "day") };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tokens_rules_in_words() {
        let now = 1_000_000;
        let answer = json!({ "kind": "token", "grant": {
            "name": "cursor",
            "rules": [{ "product": "postgres", "resources": [{ "project": "p1" }], "environments": ["production"], "actions": { "read": "allow", "delete": "ask" } }],
            "temporary": [
                { "rule": { "product": "redis", "resources": [{ "id": "rdb_1" }], "actions": { "write": "allow" } }, "until": now + 3 * 3_600_000 },
                { "rule": { "product": "redis", "resources": "*", "actions": { "admin": "allow" } }, "until": now - 1 },
            ],
            "expires_at": now + 3 * 86_400_000,
            "ip_allow": [],
        } });
        let text = words(&answer, now);
        assert!(text.contains("- postgres: read; delete after a person approves, on project p1, only in production"), "{text}");
        assert!(text.contains("- redis: write, on `rdb_1` (for 3 hours)"), "{text}");
        assert!(!text.contains("admin"), "a lapsed rule isn't listed: {text}");
        assert!(text.contains("Anything else is refused") && text.contains("it expires in 3 days"), "{text}");
    }
}
