//! MCP Apps (`io.modelcontextprotocol/ui`, SEP-1865; docs/mcp.md, "Apps"): for a host that
//! declares it, read-only tools that answer rows show them in [`TABLE`], a view the host renders
//! in a sandboxed iframe. The view gets the tool's result from its host only, has no network
//! (its resource declares no domains), and writes every value as text. It calls no tools: what
//! a person decides (an approval) is decided on roll's own pages, never from a view an agent's
//! token could drive.

use serde_json::{Value, json};

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

/// Whether a tool's result is rows a table shows: read-only tools that list, query, read or
/// report.
pub fn shows(name: &str, read_only: bool) -> bool {
    read_only
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
        assert!(declared(&json!({ "extensions": { EXT: { "mimeTypes": [MIME] } } })) && !declared(&json!({})));
    }
}
