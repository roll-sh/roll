//! `roll_find` (docs/mcp.md, "Finding a resource"): any roll resource by its name, part of its id,
//! or the dashboard link a person pasted. A name is looked up in each product's own list tool the
//! connection has, through the gate as the caller (quiet: a list that would ask a person is
//! skipped, never put to them); a link is read with the tool that reads that page's resource. Each
//! match names that read (`read: {tool, arguments}`), so the agent goes on with ordinary tools.
//! It calls no route of its own: everything it answers came from a tool the caller could call.

use futures::StreamExt;
use serde_json::{Value, json};

pub const NAME: &str = "roll_find";

/// A kind of resource: the tool that lists them, the tool that reads one (and the argument it
/// takes, filled from the first of `id` an item has), and its dashboard page, if it has one.
pub struct Kind {
    pub kind: &'static str,
    pub list: &'static str,
    pub read: &'static str,
    pub param: &'static str,
    pub id: &'static [&'static str],
    pub page: Option<&'static str>,
}

pub const KINDS: &[Kind] = &[
    Kind { kind: "project", list: "workspace_list_projects", read: "workspace_get_project", param: "id", id: &["id"], page: Some("/dashboard/projects/") },
    Kind { kind: "service", list: "apps_list_services", read: "apps_get_service", param: "service", id: &["id"], page: Some("/dashboard/apps/") },
    Kind { kind: "repo", list: "code_list_repos", read: "code_list_changes", param: "repo", id: &["full_name"], page: Some("/dashboard/code/") },
    Kind { kind: "postgres", list: "postgres_list_projects", read: "postgres_get_project", param: "project", id: &["id"], page: None },
    Kind { kind: "redis", list: "redis_list_databases", read: "redis_get_database", param: "database", id: &["id"], page: Some("/dashboard/redis/") },
    Kind { kind: "nosql", list: "nosql_list_databases", read: "nosql_get_database", param: "database", id: &["id"], page: Some("/dashboard/nosql/") },
    Kind { kind: "search", list: "search_list_clusters", read: "search_get_cluster", param: "cluster", id: &["id"], page: Some("/dashboard/search/") },
    Kind { kind: "bucket", list: "storage_list_buckets", read: "storage_get_bucket", param: "bucket", id: &["name"], page: Some("/dashboard/storage/") },
    Kind { kind: "stream", list: "streams_list", read: "streams_get", param: "stream", id: &["id"], page: None },
    Kind { kind: "vhost", list: "queues_list_vhosts", read: "queues_list_queues", param: "vhost", id: &["name"], page: None },
    Kind { kind: "function", list: "tasks_list_functions", read: "tasks_get_function", param: "id", id: &["id"], page: None },
    Kind { kind: "preview", list: "previews_list", read: "previews_get", param: "id", id: &["id"], page: None },
];

/// The fields a name is matched on.
const NAMES: &[&str] = &["name", "full_name", "title", "id"];
/// Matches answered at most.
const MAX: usize = 25;
/// Lists read at once.
const AT_ONCE: usize = 6;

pub fn definition() -> Value {
    json!({
        "name": NAME,
        "title": "Find a resource by name or link",
        "description": "Find any roll resource (project, service, repo, database, cluster, bucket, stream, function, preview) by its name or part of it (\"checkout-db\"), part of its id, or a roll dashboard link a person pasted. Each match says its kind, id and name, and the tool and arguments that read it (read). A link is read at once: the answer is that read's.",
        "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A name, part of one or of an id, or a roll dashboard URL" } }, "required": ["query"] },
        "annotations": { "readOnlyHint": true, "openWorldHint": false },
    })
}

/// What a dashboard link points at: the kind and the read's argument. A repo's page is
/// `/dashboard/code/<owner>/<repo>`, its changes' `…/changes/<n>` (read as the repo).
pub fn resolve(link: &str) -> Option<(&'static Kind, String)> {
    let url = url::Url::parse(link).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
    let path = url.path();
    KINDS.iter().find_map(|k| {
        let rest = path.strip_prefix(k.page?)?;
        let segments: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
        let id = if k.kind == "repo" { (segments.len() >= 2).then(|| format!("{}/{}", segments[0], segments[1]))? } else { segments.first()?.clone() };
        (!id.is_empty()).then_some((k, id))
    })
}

/// The first list of objects in an answer: the answer itself, or its first array field.
fn items(answer: &Value) -> Vec<&Value> {
    let list = if answer.is_array() { Some(answer) } else { answer.as_object().and_then(|o| o.values().find(|v| v.as_array().is_some_and(|a| a.first().is_some_and(Value::is_object)))) };
    list.and_then(Value::as_array).map(|a| a.iter().filter(|v| v.is_object()).collect()).unwrap_or_default()
}

/// How well `item` matches `q` (lowercase): 0 an exact name or id, 1 a prefix, 2 a part.
fn rank(item: &Value, q: &str) -> Option<u8> {
    NAMES.iter().filter_map(|f| item[*f].as_str()).filter_map(|v| {
        let v = v.to_lowercase();
        if v == q { Some(0) } else if v.starts_with(q) { Some(1) } else if v.contains(q) { Some(2) } else { None }
    }).min()
}

/// One match: what it is, its id and name, and the read that goes on from it.
fn found(k: &Kind, item: &Value) -> Option<Value> {
    let id = k.id.iter().find_map(|f| item[*f].as_str())?;
    let name = ["name", "full_name", "title"].iter().find_map(|f| item[*f].as_str()).unwrap_or(id);
    Some(json!({ "kind": k.kind, "id": id, "name": name, "read": { "tool": k.read, "arguments": { k.param: id } } }))
}

/// Run `roll_find`: `call(tool, args, quiet)` calls a tool as the caller, `has(tool)` says whether
/// the connection has it.
pub async fn run<C, F>(args: &Value, has: impl Fn(&str) -> bool, call: C) -> Value
where
    C: Fn(&'static str, Value, bool) -> F,
    F: std::future::Future<Output = Option<Value>>,
{
    let query = args["query"].as_str().map(str::trim).unwrap_or("");
    if query.is_empty() {
        return error("query is required: a name, part of an id, or a roll dashboard link");
    }
    if query.starts_with("http://") || query.starts_with("https://") {
        let Some((k, id)) = resolve(query) else {
            let pages: Vec<String> = KINDS.iter().filter_map(|k| k.page.map(|p| format!("{p}<{}>", if k.kind == "repo" { "owner>/<repo" } else { "id" }))).collect();
            return error(&format!("not a roll page this knows: {query} (it reads {})", pages.join(", ")));
        };
        if !has(k.read) {
            return error(&format!("{query} is a {}, and this connection has no {} to read it", k.kind, k.read));
        }
        // The read the agent would make itself: its answer as it is, approvals included.
        return call(k.read, json!({ k.param: id }), false).await.unwrap_or_else(|| error(&format!("{} isn't available", k.read)));
    }
    let q = query.to_lowercase();
    // The calls made up front: a stream over a closure trips the Send check of the request's future.
    let mut reads = Vec::new();
    for k in KINDS.iter().filter(|k| has(k.list)) {
        let f = call(k.list, json!({}), true);
        reads.push(async move { (k, f.await) });
    }
    let answers: Vec<(&'static Kind, Option<Value>)> = futures::stream::iter(reads).buffer_unordered(AT_ONCE).collect().await;
    let (mut matches, mut skipped) = (Vec::new(), Vec::new());
    for (k, answer) in answers {
        match answer {
            Some(r) if r["isError"] != true => {
                for item in items(&r["structuredContent"]) {
                    if let (Some(rank), Some(m)) = (rank(item, &q), found(k, item)) {
                        matches.push((rank, m));
                    }
                }
            }
            Some(r) => skipped.push(json!({ "tool": k.list, "why": r["structuredContent"]["error"].as_str().or_else(|| r["content"][0]["text"].as_str()).unwrap_or("refused") })),
            None => skipped.push(json!({ "tool": k.list, "why": "unavailable" })),
        }
    }
    // Best first, then by kind as listed, then by name.
    matches.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x["name"].as_str().cmp(&y["name"].as_str())));
    let more = matches.len() > MAX;
    let matches: Vec<Value> = matches.into_iter().take(MAX).map(|(_, m)| m).collect();
    skipped.sort_by(|a, b| a["tool"].as_str().cmp(&b["tool"].as_str()));
    let text = if matches.is_empty() {
        format!("nothing named like {query:?} in what this connection can list")
    } else {
        matches.iter().map(|m| format!("{} {} ({}): {} {}", m["kind"].as_str().unwrap_or(""), m["name"].as_str().unwrap_or(""), m["id"].as_str().unwrap_or(""), m["read"]["tool"].as_str().unwrap_or(""), m["read"]["arguments"])).collect::<Vec<_>>().join("\n")
    };
    let structured = json!({ "matches": matches, "more": more, "skipped": skipped });
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured, "isError": false })
}

fn error(text: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": { "error": text }, "isError": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_lists_and_reads_with_tools_that_exist() {
        let all = crate::tools::all();
        for k in KINDS.iter().filter(|k| !k.list.starts_with("code_")) {
            let list = all.iter().find(|t| t.name == k.list).unwrap_or_else(|| panic!("{}", k.list));
            let read = all.iter().find(|t| t.name == k.read).unwrap_or_else(|| panic!("{}", k.read));
            assert_eq!((list.method, read.method), ("GET", "GET"), "{}", k.kind);
            assert!(read.params.iter().any(|p| p.name == k.param && p.required), "{} takes {}", k.read, k.param);
        }
    }

    #[test]
    fn a_dashboard_link_names_its_resource() {
        let (k, id) = resolve("https://roll.sh/dashboard/apps/svc_1?env=production").unwrap();
        assert_eq!((k.read, id.as_str()), ("apps_get_service", "svc_1"));
        let (k, id) = resolve("http://127.0.0.1:3000/dashboard/code/acme/api/changes/12").unwrap();
        assert_eq!((k.read, id.as_str()), ("code_list_changes", "acme/api"));
        assert_eq!(resolve("https://roll.sh/dashboard/storage/my%20bucket").unwrap().1, "my bucket");
        assert!(resolve("https://roll.sh/dashboard/code/acme").is_none() && resolve("https://roll.sh/dashboard/access").is_none() && resolve("file:///dashboard/apps/x").is_none());
    }
}
