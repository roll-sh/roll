//! A tool called without an id it needs (a project, a service, an environment) asks the person to
//! pick one instead of failing (docs/mcp.md, "Asking for a missing id"): a 2026-07-28 client that
//! declared form elicitation gets an `input_required` result whose form lists what the connection
//! may read (through the gate, as completions are), and its retry carries the choice in
//! `inputResponses`. Nothing is kept between: the retry is the same call with the id filled in, so
//! the gate decides it as if the client had sent the id itself, and there's no `requestState` to
//! trust.

use crate::prompts::{Source, is_id, workspace_projects};
use serde_json::{Value, json};

/// The key of the form for a missing `param` in `inputRequests` and `inputResponses`.
const KEY_PREFIX: &str = "missing:";

/// Where an id a tool of `product` takes as `param` gets its choices.
pub fn source(product: &str, param: &str) -> Option<Source> {
    match (product, param) {
        // A Postgres project is Postgres's own, not a workspace project.
        ("postgres", "project") => Some(Source { tool: "postgres_list_projects", args: |_| json!({}), fields: &["id"] }),
        // A workspace project: the workspace's own tools name it `id`, the others `project`.
        ("workspace", "id") | ("workspace" | "secrets" | "previews" | "apps", "project") => Some(workspace_projects()),
        ("apps", "service") => Some(Source { tool: "apps_list_services", args: |_| json!({}), fields: &["id"] }),
        ("apps", "env") => Some(Source { tool: "apps_list_environments", args: |a| json!({ "service": a["service"] }), fields: &["name"] }),
        _ => None,
    }
}

/// The first id `tool` requires that `args` lacks and that has choices to offer.
pub fn missing(tool: &crate::tools::Tool, args: &Value) -> Option<(&'static str, Source)> {
    tool.params.iter().filter(|p| p.required).find_map(|p| {
        let blank = args.get(p.name).is_none_or(|v| v.is_null() || v.as_str().is_some_and(str::is_empty));
        if blank { source(tool.product, p.name).map(|s| (p.name, s)) } else { None }
    })
}

/// The person's choices from a retry's `inputResponses`, into `args`: an accepted id fills its
/// param; a declined or cancelled form, or a value that isn't an id, is why the call stops.
pub fn fill(args: &mut Value, responses: &Value) -> Result<(), String> {
    for (key, r) in responses.as_object().into_iter().flatten() {
        let Some(param) = key.strip_prefix(KEY_PREFIX) else { continue };
        match (r["action"].as_str(), r["content"]["value"].as_str()) {
            (Some("accept"), Some(v)) if is_id(v) => args[param] = json!(v),
            (Some("accept"), _) => return Err(format!("the {param} chosen isn't an id (letters, digits, _ . -; at most 64)")),
            _ => return Err(format!("no {param} was chosen, so nothing ran")),
        }
    }
    Ok(())
}

/// The form asking for `param` of `title`, offering `values` (each an id the connection may read).
pub fn input_required(title: &str, param: &str, values: &[String], more: bool) -> Value {
    // What a person calls it: the workspace's tools name a project `id`.
    let label = match param {
        "id" => "project",
        "env" => "environment",
        p => p,
    };
    let mut field = json!({ "type": "string", "title": label });
    if !values.is_empty() && !more {
        field["enum"] = json!(values);
    } else {
        field["description"] = json!(format!("The {label}'s id"));
    }
    json!({
        "resultType": "input_required",
        "inputRequests": {
            format!("{KEY_PREFIX}{param}"): {
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "message": format!("{title}: which {label}?"),
                    "requestedSchema": { "type": "object", "properties": { "value": field }, "required": ["value"] },
                },
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_fills_the_call_and_a_refusal_stops_it() {
        let mut args = json!({ "sql": "select 1" });
        fill(&mut args, &json!({ "missing:project": { "action": "accept", "content": { "value": "prj_a1" } } })).unwrap();
        assert_eq!(args["project"], "prj_a1");
        assert!(fill(&mut json!({}), &json!({ "missing:project": { "action": "decline" } })).is_err());
        assert!(fill(&mut json!({}), &json!({ "missing:project": { "action": "accept", "content": { "value": "../x" } } })).is_err());
        let postgres = crate::tools::all().iter().find(|t| t.name == "postgres_get_project").unwrap();
        assert_eq!(missing(postgres, &json!({})).map(|(p, s)| (p, s.tool)), Some(("project", "postgres_list_projects")));
        assert!(missing(postgres, &json!({ "project": "prj_a1" })).is_none());
        let workspace = crate::tools::all().iter().find(|t| t.name == "workspace_get_project").unwrap();
        assert_eq!(missing(workspace, &json!({})).map(|(p, s)| (p, s.tool)), Some(("id", "workspace_list_projects")));
    }
}
