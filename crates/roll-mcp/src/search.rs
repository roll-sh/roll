//! Search mode, `/v1/mcp?tools=search` (docs/mcp.md, "Search mode"): instead of every tool (~240
//! with every product on, ~25k tokens, past VS Code's 128-tool limit), the connection lists the
//! access tools and three of these: [`FIND`] searches the tools it may use and answers their
//! schemas, [`RUN_READ`] runs a read-only one and [`RUN`] runs any. Two run tools, so a client
//! that runs read-only tools without asking can still tell them apart (`readOnlyHint`,
//! `destructiveHint`), which one generic call tool would hide.
//!
//! A run is unwrapped into the tool it names before anything else looks at it ([`unwrap`]): the
//! read-only connection's check, the gate, approvals and the audit log see that tool, exactly as
//! if the client had called it by name. Search mode only changes what `tools/list` answers.

use serde_json::{Value, json};

pub const FIND: &str = "roll_find_tools";
pub const RUN_READ: &str = "roll_run_read";
pub const RUN: &str = "roll_run";
/// Tools [`FIND`] answers with their schemas at most.
const MAX_FOUND: usize = 25;
const DEFAULT_FOUND: usize = 8;

/// The connection's `?tools=` (or `X-MCP-Tools`): `search`, or `all` (the default). Anything else
/// is an error, as `?readonly=` is.
pub fn asked(uri: &axum::http::Uri, headers: &axum::http::HeaderMap) -> Result<bool, String> {
    let from_query = uri.query().and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "tools").map(|(_, v)| v.into_owned()));
    let Some(v) = from_query.or_else(|| headers.get("x-mcp-tools").and_then(|v| v.to_str().ok()).map(str::to_string)) else { return Ok(false) };
    match v.trim().to_ascii_lowercase().as_str() {
        "search" => Ok(true),
        "all" | "" => Ok(false),
        other => Err(format!("tools={other:?}: search (list a search tool instead of every tool) or all")),
    }
}

fn is_read_only(t: &Value) -> bool {
    t["annotations"]["readOnlyHint"].as_bool() == Some(true)
}

fn product(t: &Value) -> &str {
    t["_meta"]["roll/product"].as_str().unwrap_or("code")
}

/// What `tools/list` answers in search mode, from every tool the connection may use (`all`): the
/// access tools as they are, the search tool, and each run tool only when it has something to run.
pub fn listed(all: &[Value]) -> Vec<Value> {
    let (access, rest): (Vec<&Value>, Vec<&Value>) = all.iter().partition(|t| product(t) == "access");
    let mut products: Vec<&str> = rest.iter().map(|t| product(t)).collect();
    products.sort_unstable();
    products.dedup();
    let mut out: Vec<Value> = access.into_iter().cloned().collect();
    if rest.is_empty() {
        return out;
    }
    out.push(json!({
        "name": FIND,
        "title": "Find roll tools",
        "description": format!(
            "Search the tools this connection may use (products: {}) by what you want to do, e.g. \"create a postgres branch\" or \"redis keys\". Answers each match's name, description, input schema and whether it only reads. Run one with {RUN_READ} (read-only tools) or {RUN}. With no query, answers every tool's name and title.",
            products.join(", ")
        ),
        "inputSchema": { "type": "object", "properties": {
            "query": { "type": "string", "description": "What you want to do, in words, or a tool's name" },
            "product": { "type": "string", "description": "Only this product's tools", "enum": products },
            "limit": { "type": "integer", "description": format!("Matches answered with their schemas, 1–{MAX_FOUND} (default {DEFAULT_FOUND})") },
        }, "additionalProperties": false },
        "annotations": { "title": "Find roll tools", "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false },
        "_meta": { "roll/product": "access" },
    }));
    let run = |name: &str, title: &str, what: &str, read_only: bool| {
        json!({
            "name": name,
            "title": title,
            "description": format!("Run {what} that {FIND} found: {{\"tool\": its name, \"arguments\": its arguments}}. The answer is that tool's own; approvals and refusals work as they do for it."),
            "inputSchema": { "type": "object", "properties": {
                "tool": { "type": "string", "description": format!("The tool's name, as {FIND} answered it") },
                "arguments": { "type": "object", "description": "The tool's arguments, as its input schema asks" },
            }, "required": ["tool"], "additionalProperties": false },
            "annotations": { "title": title, "readOnlyHint": read_only, "destructiveHint": !read_only, "idempotentHint": read_only, "openWorldHint": false },
            "_meta": { "roll/product": "access" },
        })
    };
    if rest.iter().any(|t| is_read_only(t)) {
        out.push(run(RUN_READ, "Run a read-only roll tool", "a read-only tool", true));
    }
    if rest.iter().any(|t| !is_read_only(t)) {
        out.push(run(RUN, "Run a roll tool", "any tool", false));
    }
    out
}

/// Lower-case words of `s`, a plural's `s` dropped (`branches` finds `branch`… close enough).
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let w = w.to_ascii_lowercase();
            match w.strip_suffix("es").filter(|r| r.len() > 3 && (r.ends_with("ch") || r.ends_with("sh") || r.ends_with('x'))) {
                Some(r) => r.to_string(),
                None => w.strip_suffix('s').filter(|r| r.len() > 2 && !r.ends_with('s')).map_or(w.clone(), str::to_string),
            }
        })
        .collect()
}

/// Words that say nothing about which tool (`a`, `the`, `roll`…).
const STOP: &[&str] = &["a", "an", "the", "to", "of", "in", "on", "for", "and", "or", "my", "me", "i", "it", "roll", "with", "from", "by", "is", "all", "what", "how", "do"];

/// A tool's words by field: name, title, description.
struct Doc<'a> {
    tool: &'a Value,
    fields: [Vec<String>; 3],
}

impl<'a> Doc<'a> {
    fn of(tool: &'a Value) -> Self {
        let f = |k: &str| words(tool[k].as_str().unwrap_or(""));
        Doc { tool, fields: [f("name"), f("title"), f("description")] }
    }

    /// How well `field` holds `q`: the word itself, or one starting with it (or it with that
    /// word: `deploy`, `deployment`) at half.
    fn has(&self, field: usize, q: &str) -> f64 {
        let words = &self.fields[field];
        if words.iter().any(|w| w == q) {
            1.0
        } else if q.len() >= 4 && words.iter().any(|w| w.len() >= 4 && (w.starts_with(q) || q.starts_with(w.as_str()))) {
            0.5
        } else {
            0.0
        }
    }
}

/// Each query word counts by how rare it is among the tools (`run` is in dozens, `sql` in a few),
/// more in the name than the title than the description.
fn rank<'a>(pool: &[&'a Value], query: &[String]) -> Vec<(f64, &'a Value)> {
    const FIELD: [f64; 3] = [6.0, 3.0, 1.0];
    let docs: Vec<Doc> = pool.iter().map(|t| Doc::of(t)).collect();
    let n = docs.len() as f64;
    let weight: Vec<f64> = query
        .iter()
        .map(|q| {
            let df = docs.iter().filter(|d| (0..3).any(|f| d.has(f, q) > 0.0)).count() as f64;
            if df == 0.0 { 0.0 } else { (1.0 + n / df).ln() }
        })
        .collect();
    docs.iter()
        .map(|d| {
            let mut s = 0.0;
            for (q, w) in query.iter().zip(&weight) {
                s += w * (0..3).map(|f| FIELD[f] * d.has(f, q)).fold(0.0, f64::max);
            }
            // Every word of the query in the name: what was asked for, by name.
            if !query.is_empty() && query.iter().all(|q| d.has(0, q) == 1.0) {
                s *= 1.5;
            }
            // Of two names holding the query, the one it covers more of: `apps_deploy` before
            // `apps_cancel_deploy` for "deploy my app".
            let name = &d.fields[0];
            if !name.is_empty() {
                let covered = name.iter().filter(|w| query.iter().any(|q| *w == q || (q.len() >= 4 && w.len() >= 4 && (w.starts_with(q.as_str()) || q.starts_with(w.as_str()))))).count();
                s *= 1.0 + 0.5 * covered as f64 / name.len() as f64;
            }
            (s, d.tool)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect()
}

/// [`FIND`]'s answer over the tools the connection may use (`all`, the run tools' targets).
pub fn find(all: &[Value], args: &Value) -> Value {
    let only = args["product"].as_str();
    let pool: Vec<&Value> = all.iter().filter(|t| product(t) != "access" && only.is_none_or(|p| product(t) == p)).collect();
    let query: String = args["query"].as_str().unwrap_or("").chars().take(300).collect();
    let limit = args["limit"].as_u64().map_or(DEFAULT_FOUND, |n| (n as usize).clamp(1, MAX_FOUND));
    let entry = |t: &Value| json!({ "name": t["name"], "title": t["title"], "read_only": is_read_only(t), "run_with": if is_read_only(t) { RUN_READ } else { RUN } });
    let structured = if query.trim().is_empty() {
        json!({ "tools": pool.iter().map(|t| entry(t)).collect::<Vec<_>>(), "total": pool.len() })
    } else {
        let exact = pool.iter().find(|t| t["name"].as_str() == Some(query.trim()));
        let q: Vec<String> = words(&query).into_iter().filter(|w| !STOP.contains(&w.as_str())).collect();
        let mut scored: Vec<(f64, &Value)> = match exact {
            Some(t) => vec![(f64::MAX, *t)],
            None => rank(&pool, &q),
        };
        // Best first; a tie by name, so the same query answers the same list.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1["name"].as_str().cmp(&b.1["name"].as_str())));
        let more = scored.len().saturating_sub(limit);
        let found: Vec<Value> = scored
            .into_iter()
            .take(limit)
            .map(|(_, t)| {
                let mut e = entry(t);
                e["description"] = t["description"].clone();
                e["inputSchema"] = t["inputSchema"].clone();
                e
            })
            .collect();
        json!({ "tools": found, "more": more })
    };
    let text = if structured["tools"].as_array().is_some_and(Vec::is_empty) {
        format!("No tool matches {query:?}{}. Try other words, or call {FIND} with no query for every tool's name.", only.map(|p| format!(" in {p}")).unwrap_or_default())
    } else {
        structured.to_string()
    };
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
}

/// A run tool's call as the tool it names: `Ok((name, arguments))`, or the error result to answer.
/// `all` is what the connection may use: a run never reaches a tool it isn't offered, and
/// [`RUN_READ`] only a read-only one.
pub fn unwrap<T: std::borrow::Borrow<Value>>(all: &[T], run: &str, args: &Value) -> Result<(String, Value), Value> {
    let fail = |text: String| json!({ "content": [{ "type": "text", "text": text }], "isError": true });
    let Some(name) = args["tool"].as_str() else { return Err(fail(format!("{run} needs \"tool\": a name {FIND} answered"))) };
    let inner = match args.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(fail("\"arguments\" must be an object".into())),
    };
    // `all` is the connection's own tools: never these three, so a run never runs a run.
    let Some(t) = all.iter().map(std::borrow::Borrow::borrow).find(|t: &&Value| t["name"].as_str() == Some(name)) else {
        return Err(fail(format!("no tool {name:?} on this connection: call {FIND} to see what there is")));
    };
    if run == RUN_READ && !is_read_only(t) {
        return Err(fail(format!("{name} changes things: run it with {RUN}")));
    }
    Ok((name.to_string(), inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, product: &str, read_only: bool, desc: &str) -> Value {
        json!({ "name": name, "title": name.replace('_', " "), "description": desc, "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": read_only }, "_meta": { "roll/product": product } })
    }

    fn sample() -> Vec<Value> {
        vec![
            tool("access_check", "access", true, "What this token may do"),
            tool("postgres_create_branch", "postgres", false, "Create a branch of a database"),
            tool("postgres_list_branches", "postgres", true, "List a project's branches"),
            tool("postgres_query", "postgres", true, "Run read-only SQL"),
            tool("redis_keys", "redis", true, "List keys matching a pattern"),
        ]
    }

    #[test]
    fn finds_by_words_best_first() {
        let found = find(&sample(), &json!({ "query": "create a postgres branch" }));
        let names: Vec<&str> = found["structuredContent"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names[0], "postgres_create_branch");
        assert!(!names.contains(&"access_check"), "the access tools are listed on their own");
        assert_eq!(found["structuredContent"]["tools"][0]["run_with"], RUN);
        let by_name = find(&sample(), &json!({ "query": "redis_keys" }));
        assert_eq!(by_name["structuredContent"]["tools"].as_array().unwrap().len(), 1);
        let none = find(&sample(), &json!({ "query": "kafka" }));
        assert!(none["content"][0]["text"].as_str().unwrap().starts_with("No tool matches"));
    }

    #[test]
    fn runs_only_what_the_connection_has() {
        let all = sample();
        assert_eq!(unwrap(&all, RUN_READ, &json!({ "tool": "postgres_query", "arguments": { "sql": "select 1" } })).unwrap().0, "postgres_query");
        assert!(unwrap(&all, RUN_READ, &json!({ "tool": "postgres_create_branch" })).is_err(), "read-only run refuses a write");
        assert!(unwrap(&all, RUN, &json!({ "tool": "postgres_delete_project" })).is_err(), "not offered");
        assert!(unwrap(&all, RUN, &json!({ "tool": RUN, "arguments": {} })).is_err(), "no run of a run");
        assert_eq!(unwrap(&all, RUN, &json!({ "tool": "access_check" })).unwrap().0, "access_check");
        let shown = listed(&all);
        let names: Vec<&str> = shown.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["access_check", FIND, RUN_READ, RUN]);
        assert_eq!(listed(&all[..1]).len(), 1, "nothing to find, nothing listed but access");
    }
}

/// Search mode works only if `roll_find_tools` puts the tool a person means first: these are
/// queries an agent sends, across every product, against the real catalogue (roll code's tools
/// are another crate's). A new tool that takes one of these over needs a title that says what it's
/// for, not a lower bar here.
#[cfg(test)]
mod finds {
    #[test]
    fn everyday_queries_find_their_tool_first() {
        let all: Vec<serde_json::Value> = crate::tools::all().iter().map(crate::tools::Tool::definition).collect();
        let wrong: Vec<String> = [
            ("create a postgres branch", "postgres_create_branch"),
            ("run a sql query", "postgres_query"),
            ("change data with sql", "postgres_execute"),
            ("explain a query plan", "postgres_explain"),
            ("slow queries", "postgres_slow_queries"),
            ("delete a redis database", "redis_delete_database"),
            ("upload a file to a bucket", "storage_write_object"),
            ("presign a url", "storage_presign"),
            ("delete objects under a prefix", "storage_delete_objects"),
            ("deploy my app", "apps_deploy"),
            ("roll back a deployment", "apps_rollback"),
            ("set an environment variable", "apps_set_variables"),
            ("app logs", "apps_logs"),
            ("restart my service", "apps_restart"),
            ("add a log drain", "apps_add_drain"),
            ("notify slack on deploy", "apps_add_notification"),
            ("read a secret", "secrets_reveal"),
            ("encrypt with a key", "secrets_transit_encrypt"),
            ("an old version of a secret", "secrets_version_value"),
            ("produce to a kafka topic", "streams_produce"),
            ("read records from a topic", "streams_read"),
            ("publish a message to a queue", "queues_publish"),
            ("pause a schedule", "tasks_pause_schedule"),
            ("send an event", "tasks_send_event"),
            ("wait for a run", "tasks_wait_run"),
            ("create an index", "search_create_index"),
            ("cql query", "nosql_cql"),
            ("nosql activity", "nosql_activity"),
            ("share a preview", "previews_share"),
            ("create a workspace project", "workspace_create_project"),
            ("why is my service failing", "apps_errors"),
            ("slow request traces", "apps_traces"),
            ("preview errors", "previews_errors"),
            ("unmasked personal data", "previews_mask_suggestions"),
            ("queue bindings", "queues_list_bindings"),
            ("search cluster audit log", "search_audit"),
            ("import a .env file", "secrets_import"),
            ("roll back secrets", "secrets_rollback"),
            ("which secrets nobody reads", "secrets_insights"),
            ("was the secrets audit log tampered with", "secrets_verify_audit"),
            ("one-time code", "secrets_totp"),
            ("vulnerabilities in my image", "apps_deploy_vulnerabilities"),
            ("slo error budget", "apps_environment_slo"),
            ("contact form submissions", "apps_form_submissions"),
            ("why is my queue client refused", "queues_list_users"),
            ("is my nosql import done", "nosql_list_imports"),
            ("is the gateway to my private network online", "secrets_list_gateways"),
            ("acme external account binding", "secrets_ca_acme"),
            ("why is assume role refused", "storage_list_roles"),
            ("list s3 access keys", "storage_list_keys"),
        ]
        .iter()
        .filter_map(|(q, want)| {
            let r = super::find(&all, &serde_json::json!({ "query": q, "limit": 3 }));
            let got: Vec<&str> = r["structuredContent"]["tools"].as_array().map(|a| a.iter().filter_map(|t| t["name"].as_str()).collect()).unwrap_or_default();
            (got.first() != Some(want)).then(|| format!("{q:?}: wanted {want}, got {got:?}"))
        })
        .collect();
        assert!(wrong.is_empty(), "roll_find_tools ranks these wrong:\n{}", wrong.join("\n"));
    }
}
