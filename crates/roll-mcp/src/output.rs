//! Output schemas (docs/mcp.md, "Output schemas"): a 2026-07-28 client, which may run code against
//! tools itself, gets each tool's `outputSchema`, so it knows the fields an answer has. roll's
//! routes have no response schemas, so these are learned from real answers: the MCP sweep records
//! what each tool answered on a fresh rolld and `scripts/mcp_output_schemas.py` widens
//! `output-schemas.json` with it. They're loose on purpose (every value may be null, more fields
//! are always allowed, nothing is required), and the sweep fails on an answer that no longer fits
//! the schema its tool publishes.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

static SCHEMAS: LazyLock<HashMap<String, Value>> = LazyLock::new(|| serde_json::from_str(include_str!("output-schemas.json")).unwrap_or_default());

/// The output schema a tool publishes, if the sweep has seen it answer.
pub fn schema(tool: &str) -> Option<&'static Value> {
    SCHEMAS.get(tool)
}

/// Each listed tool's `outputSchema`, where there is one.
pub fn attach(tools: &mut [Value]) {
    for t in tools {
        if let Some(s) = t["name"].as_str().and_then(schema) {
            t["outputSchema"] = s.clone();
        }
    }
}

/// Every name the schemas know, for the test that they name real tools.
pub fn names() -> impl Iterator<Item = &'static str> {
    SCHEMAS.keys().map(String::as_str)
}
