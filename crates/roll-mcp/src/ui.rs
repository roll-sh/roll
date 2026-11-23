//! MCP Apps (`io.modelcontextprotocol/ui`, SEP-1865; docs/mcp.md, "Apps"): for a host that
//! declares it, read-only tools that answer rows show them in [`TABLE`], a view the host renders
//! in a sandboxed iframe, and `access_wait_approval` shows an approval waiting as a card. The view
//! gets the tool's result from its host only, has no network (its resource declares no domains),
//! and writes every value as text. What a person decides (an approval) is decided on roll's own
//! pages, which the view only asks the host to open (`ui/open-link`), never from a view an agent's
//! token could drive. While an answer is still changing (a deploy building, a job running, an
//! approval pending) roll marks it ([`PENDING`]: a read-only tool and its arguments) and the view
//! reads that again through the host, so through the gate, until it settles. Nothing else.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const EXT: &str = "io.modelcontextprotocol/ui";
pub const MIME: &str = "text/html;profile=mcp-app";
/// The table view.
pub const TABLE: &str = "ui://roll/table";
/// What a session id carries (after its prefix) for a host that declared the extension in
/// `initialize`: later requests don't say it again, and roll keeps no session.
pub const SESSION_FLAG: &str = "~ui.";

/// Whether these capabilities declare MCP Apps with HTML views.
pub fn declared(capabilities: &Value) -> bool {
    capabilities["extensions"][EXT]["mimeTypes"].as_array().is_some_and(|m| m.iter().any(|t| t == MIME))
}

/// The key of a result's `_meta` that says it's still changing: `{tool, arguments}`, the read-only
/// call that reads it again.
pub const PENDING: &str = "roll/pending";

/// Whether an answer settled (a tool's `until`).
type Settled = fn(&Value) -> bool;

/// How each tool's answer says it settled: a waiting tool's own `until`, or the one of the waiting
/// tool on the same `GET` route (`apps_get_deploy` is `apps_wait_deploy` without the wait). Only
/// `GET` tools, so a view only ever reads.
static SETTLES: LazyLock<HashMap<&'static str, Settled>> = LazyLock::new(|| {
    let all = crate::tools::all();
    let waits = |path: &str| all.iter().filter(|w| w.method == "GET" && w.path == path).find_map(|w| w.until);
    all.iter().filter(|t| t.method == "GET").filter_map(|t| Some((t.name, t.until.or_else(|| waits(t.path))?))).collect()
});

/// Whether `name`'s answer can still be changing ([`SETTLES`]).
pub fn settles(name: &str) -> Option<Settled> {
    SETTLES.get(name).copied()
}

/// Mark a result that's still changing ([`PENDING`]) for the view to read again: the tool's own
/// call with a short wait (a wait the view holds open would stall it), or, for a call waiting for
/// a person, `access_wait_approval` on its approval while the connection has it (`has`).
pub fn mark_pending(name: &str, args: &Value, result: &mut Value, has: impl Fn(&str) -> bool) {
    if result["isError"] == true {
        return;
    }
    let s = &result["structuredContent"];
    let again = if let Some(approval) = crate::waiting_approval(s) {
        has("access_wait_approval").then(|| ("access_wait_approval", json!({ "approval_id": approval, "wait": 20 })))
    } else {
        settles(name).filter(|over| !over(s)).map(|_| (name, short_wait(name, args)))
    };
    if let Some((tool, arguments)) = again {
        if !result["_meta"].is_object() {
            result["_meta"] = json!({});
        }
        result["_meta"][PENDING] = json!({ "tool": tool, "arguments": arguments });
    }
}

/// `args` with any wait the tool takes cut short: 20 s for `wait`, `10s` for `timeout`.
fn short_wait(name: &str, args: &Value) -> Value {
    let mut a = if args.is_object() { args.clone() } else { json!({}) };
    for p in crate::tools::all().iter().find(|t| t.name == name).map_or(&[][..], |t| t.params) {
        match p.name {
            "wait" => a["wait"] = json!(20),
            "timeout" => a["timeout"] = json!("10s"),
            _ => {}
        }
    }
    a
}

/// Whether a tool's result is rows a table shows: read-only tools that list, query, read or
/// report.
pub fn shows(name: &str, read_only: bool) -> bool {
    // Waiting for a person: the card with roll's approval page, then what the call answered.
    name == "access_wait_approval"
        // Something still changing, waited on or read (a deploy and its steps, a job, a run, a
        // preview): its fields, its list as a table, read again until it settles.
        || (read_only && settles(name).is_some())
        || read_only
        && ["_list_", "_query", "_logs", "_read", "_peek", "_usage", "_activity", "_metrics", "_events", "_history"]
            .iter()
            .any(|w| name.contains(w) || name.ends_with(w.trim_end_matches('_')))
}

/// Point the tools that show rows at the table (`_meta.ui.resourceUri`).
pub fn attach(tools: &mut [Value]) {
    for t in tools {
        let name = t["name"].as_str().unwrap_or_default().to_string();
        let read_only = t["annotations"]["readOnlyHint"].as_bool() == Some(true);
        if shows(&name, read_only) {
            if !t["_meta"].is_object() {
                t["_meta"] = json!({});
            }
            t["_meta"]["ui"] = json!({ "resourceUri": TABLE });
        }
    }
}

/// The table view, as `resources/list` lists it.
pub fn resource() -> Value {
    json!({ "uri": TABLE, "name": "roll table", "title": "A tool's rows", "description": "Shows a roll tool's rows as a table.", "mimeType": MIME })
}

/// `resources/read` for the table view: its HTML, with no domains to reach (no network).
pub fn read() -> Value {
    json!({ "contents": [{ "uri": TABLE, "mimeType": MIME, "text": include_str!("ui/table.html"), "_meta": { "ui": { "csp": { "connectDomains": [], "resourceDomains": [] }, "prefersBorder": true } } }] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_go_on_tools_that_read_rows() {
        assert!(shows("postgres_query", true) && shows("apps_list_services", true) && shows("apps_logs", true));
        assert!(!shows("postgres_execute", false) && !shows("postgres_get_project", true));
        assert!(shows("access_wait_approval", false));
        assert!(shows("apps_wait_deploy", true) && shows("apps_get_job", true) && !shows("apps_get_service", true));
        assert!(shows("previews_wait", true) && shows("tasks_wait_run", true));
        assert!(declared(&json!({ "extensions": { EXT: { "mimeTypes": [MIME] } } })) && !declared(&json!({})));
    }

    #[test]
    fn a_changing_answer_names_the_read_that_refreshes_it() {
        let args = json!({ "service": "svc_1", "env": "production", "deploy": "dep_1" });
        let mut building = json!({ "structuredContent": { "status": "building" } });
        mark_pending("apps_get_deploy", &args, &mut building, |_| true);
        assert_eq!(building["_meta"][PENDING], json!({ "tool": "apps_get_deploy", "arguments": args }));
        let mut live = json!({ "structuredContent": { "status": "active" } });
        mark_pending("apps_wait_deploy", &args, &mut live, |_| true);
        assert!(live.get("_meta").is_none());
        // A wait the view would hold open is cut short.
        let mut run = json!({ "structuredContent": { "status": "running" } });
        mark_pending("tasks_wait_run", &json!({ "id": "run_1", "timeout": "5m" }), &mut run, |_| true);
        assert_eq!(run["_meta"][PENDING]["arguments"], json!({ "id": "run_1", "timeout": "10s" }));
        // A call waiting for a person: its approval, read again while the connection can.
        let pending = json!({ "structuredContent": { "status": "approval_pending", "approval": "apr_1", "http_status": 202 } });
        let (mut with, mut without) = (pending.clone(), pending);
        mark_pending("apps_deploy", &json!({}), &mut with, |_| true);
        assert_eq!(with["_meta"][PENDING], json!({ "tool": "access_wait_approval", "arguments": { "approval_id": "apr_1", "wait": 20 } }));
        mark_pending("apps_deploy", &json!({}), &mut without, |t| t != "access_wait_approval");
        assert!(without.get("_meta").is_none());
        // Only reads are ever named.
        for name in SETTLES.keys() {
            assert_eq!(crate::tools::all().iter().find(|t| t.name == *name).unwrap().method, "GET", "{name}");
        }
    }
}
