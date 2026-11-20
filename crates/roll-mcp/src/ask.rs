//! A tool called without an id it needs (a project, a service, an environment) asks the person to
//! pick one instead of failing (docs/mcp.md, "Asking for a missing id"): a 2026-07-28 client that
//! declared form elicitation gets an `input_required` result whose form lists what the connection
//! may read (through the gate, as completions are), and its retry carries the choice in
//! `inputResponses`. Nothing is kept between: the retry is the same call with the id filled in, so
//! the gate decides it as if the client had sent the id itself, and there's no `requestState` to
//! trust.

use crate::prompts::{Source, is_choice, workspace_projects};
use serde_json::{Value, json};

/// The key of the form for a missing `param` in `inputRequests` and `inputResponses`.
const KEY_PREFIX: &str = "missing:";

/// Where an id a tool of `product` takes as `param` gets its choices.
pub fn source(product: &str, param: &str) -> Option<Source> {
    match (product, param) {
        // A Postgres project is Postgres's own, not a workspace project.
        ("postgres", "project") => Some(Source { tool: "postgres_list_projects", args: |_| json!({}), fields: &["id"] }),
        // A workspace project: the workspace's own tools name it `id`, the others `project`.
        ("workspace", "id") | ("workspace" | "secrets" | "previews" | "apps" | "access", "project") => Some(workspace_projects()),
        ("access", "id") => Some(Source { tool: "access_list_tokens", args: |_| json!({}), fields: &["id"] }),
        ("apps", "service") => Some(Source { tool: "apps_list_services", args: |_| json!({}), fields: &["id"] }),
        ("apps", "env") => Some(Source { tool: "apps_list_environments", args: |a| json!({ "service": a["service"] }), fields: &["name"] }),
        // A secrets environment by its slug (`development`), as its tools take it; `name` is for people.
        ("secrets", "env") => Some(Source { tool: "secrets_list_environments", args: |a| json!({ "project": a["project"] }), fields: &["slug"] }),
        ("postgres", "branch") => Some(Source { tool: "postgres_get_project", args: |a| json!({ "project": a["project"] }), fields: &["id"] }),
        ("previews", "id") => Some(Source { tool: "previews_list", args: |_| json!({}), fields: &["id"] }),
        // The rest are named by their names, as their tools take them.
        ("storage", "bucket") => Some(Source { tool: "storage_list_buckets", args: |_| json!({}), fields: &["name"] }),
        ("streams", "stream") => Some(Source { tool: "streams_list", args: |_| json!({}), fields: &["name"] }),
        ("streams", "topic") => Some(Source { tool: "streams_list_topics", args: |a| json!({ "stream": a["stream"] }), fields: &["name"] }),
        ("nosql", "database") => Some(Source { tool: "nosql_list_databases", args: |_| json!({}), fields: &["name"] }),
        ("redis", "database") => Some(Source { tool: "redis_list_databases", args: |_| json!({}), fields: &["name"] }),
        ("search", "cluster") => Some(Source { tool: "search_list_clusters", args: |_| json!({}), fields: &["name"] }),
        ("search", "index") => Some(Source { tool: "search_list_indices", args: |a| json!({ "cluster": a["cluster"] }), fields: &["index", "name"] }),
        ("queues", "vhost") => Some(Source { tool: "queues_list_vhosts", args: |_| json!({}), fields: &["name"] }),
        ("queues", "queue") => Some(Source { tool: "queues_list_queues", args: |a| json!({ "vhost": a["vhost"] }), fields: &["name"] }),
        // roll code's (fronted from `code_control::mcp`): a repo as `owner/name`, an owner of one.
        ("code", "repo") => Some(Source { tool: "code_list_repos", args: |_| json!({}), fields: &["full_name"] }),
        ("code" | "agents", "owner") => Some(Source { tool: "code_list_repos", args: |_| json!({}), fields: &["owner"] }),
        _ => None,
    }
}

/// The first id a tool requires that `args` lacks and that has choices to offer, from its
/// definition (roll's own tools and the ones fronted from another crate alike).
pub fn missing(def: &Value, args: &Value) -> Option<(String, Source)> {
    let product = def["_meta"]["roll/product"].as_str()?;
    def["inputSchema"]["required"].as_array()?.iter().filter_map(Value::as_str).find_map(|p| {
        let blank = args.get(p).is_none_or(|v| v.is_null() || v.as_str().is_some_and(str::is_empty));
        if blank { source_at(product, p, def["_meta"]["roll/route"].as_str().unwrap_or("")).map(|s| (p.to_string(), s)) } else { None }
    })
}

/// As [`source`], for a tool whose route (`GET /v1/tasks/runs/{id}`) says what an ambiguous name
/// is: tasks' `id` is a message, a run, a schedule, a function, an event or a dead letter by the
/// path's segment before it.
fn source_at(product: &str, param: &str, route: &str) -> Option<Source> {
    if (product, param) != ("tasks", "id") {
        return source(product, param);
    }
    let kind = route.split_once("/v1/tasks/")?.1.split_once("/{id}")?.0;
    let tool = match kind {
        "messages" => "tasks_list_messages",
        "runs" => "tasks_list_runs",
        "schedules" => "tasks_list_schedules",
        "functions" => "tasks_list_functions",
        "events" => "tasks_list_events",
        "dlq" => "tasks_list_dead_letters",
        _ => return None,
    };
    Some(Source { tool, args: |_| json!({}), fields: &["id"] })
}

/// The person's choices from a retry's `inputResponses`, into `args`: an accepted id fills its
/// param; a declined or cancelled form, or a value that isn't an id, is why the call stops.
pub fn fill(args: &mut Value, responses: &Value) -> Result<(), String> {
    for (key, r) in responses.as_object().into_iter().flatten() {
        let Some(param) = key.strip_prefix(KEY_PREFIX) else { continue };
        match (r["action"].as_str(), r["content"]["value"].as_str()) {
            (Some("accept"), Some(v)) if is_choice(v) => args[param] = json!(v),
            (Some("accept"), _) => return Err(format!("the {param} chosen isn't an id (letters, digits, _ . -; at most 64) or a repo's owner/name")),
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
        assert!(fill(&mut json!({}), &json!({ "missing:repo": { "action": "accept", "content": { "value": "../.." } } })).is_err());
        fill(&mut args, &json!({ "missing:repo": { "action": "accept", "content": { "value": "acme/app" } } })).unwrap();
        assert_eq!(args["repo"], "acme/app");
        let def = |name: &str| crate::tools::all().iter().find(|t| t.name == name).unwrap().definition_ref();
        let missed = |name: &str, args: Value| missing(def(name), &args).map(|(p, s)| (p, s.tool));
        assert_eq!(missed("postgres_get_project", json!({})), Some(("project".into(), "postgres_list_projects")));
        assert!(missed("postgres_get_project", json!({ "project": "prj_a1" })).is_none());
        assert_eq!(missed("workspace_get_project", json!({})), Some(("id".into(), "workspace_list_projects")));
        // Tasks' `id` is what its route names.
        assert_eq!(missed("tasks_cancel_run", json!({})), Some(("id".into(), "tasks_list_runs")));
        assert_eq!(missed("tasks_retry_message", json!({})), Some(("id".into(), "tasks_list_messages")));
    }
}
