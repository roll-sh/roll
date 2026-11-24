//! `roll_find` (docs/mcp.md, "Finding a resource"): any roll resource by its name, part of its id,
//! or the dashboard link a person pasted. A name is looked up in each product's own list tool the
//! connection has, through the gate as the caller (quiet: a list that would ask a person is
//! skipped, never put to them); a link is read with the tool that reads that page's resource. Each
//! match names that read (`read: {tool, arguments}`), so the agent goes on with ordinary tools.
//! It calls no route of its own: everything it answers came from a tool the caller could call.

use futures::StreamExt;
use serde_json::{Value, json};

pub const NAME: &str = "roll_find";

/// A kind of resource: the tool that lists them, the tool that reads one (with the argument it
/// takes, filled from the first of `id` an item has), and its dashboard page (`{}` is the id).
pub struct Kind {
    pub kind: &'static str,
    pub list: &'static str,
    pub read: &'static str,
    pub param: &'static str,
    pub id: &'static [&'static str],
    pub page: Option<&'static str>,
}

pub const KINDS: &[Kind] = &[
    Kind { kind: "project", list: "workspace_list_projects", read: "workspace_get_project", param: "id", id: &["id"], page: Some("/dashboard/projects/{}") },
    Kind { kind: "service", list: "apps_list_services", read: "apps_get_service", param: "service", id: &["id"], page: Some("/dashboard/apps/{}") },
    Kind { kind: "repo", list: "code_list_repos", read: "code_list_changes", param: "repo", id: &["full_name"], page: Some("/dashboard/code/{}") },
    Kind { kind: "postgres", list: "postgres_list_projects", read: "postgres_get_project", param: "project", id: &["id"], page: Some("/dashboard/databases?project={}") },
    Kind { kind: "redis", list: "redis_list_databases", read: "redis_get_database", param: "database", id: &["id"], page: Some("/dashboard/redis/{}") },
    Kind { kind: "nosql", list: "nosql_list_databases", read: "nosql_get_database", param: "database", id: &["id"], page: Some("/dashboard/nosql/{}") },
    Kind { kind: "search", list: "search_list_clusters", read: "search_get_cluster", param: "cluster", id: &["id"], page: Some("/dashboard/search/{}") },
    Kind { kind: "bucket", list: "storage_list_buckets", read: "storage_get_bucket", param: "bucket", id: &["name"], page: Some("/dashboard/storage/{}") },
    Kind { kind: "stream", list: "streams_list", read: "streams_get", param: "stream", id: &["id"], page: Some("/dashboard/streams?stream={}") },
    Kind { kind: "vhost", list: "queues_list_vhosts", read: "queues_list_queues", param: "vhost", id: &["name"], page: Some("/dashboard/queues/queues?vhost={}") },
    Kind { kind: "function", list: "tasks_list_functions", read: "tasks_get_function", param: "id", id: &["id"], page: None },
    Kind { kind: "task queue", list: "tasks_list_queues", read: "tasks_get_queue", param: "name", id: &["name"], page: None },
    Kind { kind: "topic", list: "tasks_list_topics", read: "tasks_get_topic", param: "name", id: &["name"], page: None },
    Kind { kind: "preview", list: "previews_list", read: "previews_get", param: "id", id: &["id"], page: None },
    Kind { kind: "token", list: "access_list_tokens", read: "access_get_token", param: "id", id: &["id"], page: Some("/dashboard/access/tokens/{}") },
];

/// A dashboard page and the read that shows what it shows: `path` is the page's path up to its
/// first value (exact when `exact`), `args` fills the read's arguments from the path's segments
/// after it and the query (`None`: not a page of one thing).
pub struct Page {
    pub path: &'static str,
    pub exact: bool,
    pub kind: &'static str,
    pub read: &'static str,
    pub args: fn(&[String], &Query) -> Option<Value>,
    /// How the page reads, for a person (the error that lists the pages it knows).
    pub shape: &'static str,
    /// First segments that are the dashboard's own pages there, never an id (`/dashboard/apps/visit`).
    pub not: &'static [&'static str],
}

/// A link's query, by name.
pub type Query = std::collections::HashMap<String, String>;

pub static PAGES: &[Page] = &[
    Page { path: "/dashboard/projects/", exact: false, kind: "project", read: "workspace_get_project", args: |s, _| Some(json!({ "id": s.first()? })), shape: "/dashboard/projects/<id>", not: &[] },
    Page { path: "/dashboard/apps/", exact: false, kind: "service", read: "apps_get_service", args: |s, _| Some(json!({ "service": s.first()? })), shape: "/dashboard/apps/<id>", not: &["visit"] },
    // A change's page reads the change (`owner/repo#N`); a repo's, its changes.
    Page {
        path: "/dashboard/code/",
        exact: false,
        kind: "change",
        read: "code_get_change",
        args: |s, _| (s.len() >= 4 && s[2] == "changes" && s[3].bytes().all(|b| b.is_ascii_digit())).then(|| json!({ "change": format!("{}/{}#{}", s[0], s[1], s[3]) })),
        shape: "/dashboard/code/<owner>/<repo>/changes/<n>", not: &[],
    },
    Page { path: "/dashboard/code/", exact: false, kind: "repo", read: "code_list_changes", args: |s, _| (s.len() >= 2).then(|| json!({ "repo": format!("{}/{}", s[0], s[1]) })), shape: "/dashboard/code/<owner>/<repo>", not: &[] },
    Page { path: "/dashboard/redis/", exact: false, kind: "redis", read: "redis_get_database", args: |s, _| Some(json!({ "database": s.first()? })), shape: "/dashboard/redis/<id>", not: &[] },
    Page { path: "/dashboard/nosql/", exact: false, kind: "nosql", read: "nosql_get_database", args: |s, _| Some(json!({ "database": s.first()? })), shape: "/dashboard/nosql/<id>", not: &[] },
    Page { path: "/dashboard/search/", exact: false, kind: "search", read: "search_get_cluster", args: |s, _| Some(json!({ "cluster": s.first()? })), shape: "/dashboard/search/<id>", not: &[] },
    Page { path: "/dashboard/storage/", exact: false, kind: "bucket", read: "storage_get_bucket", args: |s, _| Some(json!({ "bucket": s.first()? })), shape: "/dashboard/storage/<bucket>", not: &["-"] },
    Page { path: "/dashboard/access/tokens/", exact: false, kind: "token", read: "access_get_token", args: |s, _| Some(json!({ "id": s.first()? })), shape: "/dashboard/access/tokens/<id>", not: &[] },
    // A token's audit names it in the query; an approval's page too (read now, no wait).
    Page { path: "/dashboard/access/audit", exact: true, kind: "token", read: "access_get_token", args: |_, q| Some(json!({ "id": q.get("grant")? })), shape: "/dashboard/access/audit?grant=<id>", not: &[] },
    Page { path: "/dashboard/access/approvals", exact: true, kind: "approval", read: "access_wait_approval", args: |_, q| Some(json!({ "approval_id": q.get("id")?, "wait": 0 })), shape: "/dashboard/access/approvals?id=<id>", not: &[] },
    Page { path: "/dashboard/queues/queues/", exact: false, kind: "queue", read: "queues_get_queue", args: |s, _| (s.len() >= 2).then(|| json!({ "vhost": s[0], "queue": s[1] })), shape: "/dashboard/queues/queues/<vhost>/<queue>", not: &[] },
    // Postgres and streams keep their selection in the query (`?project=`, `?stream=`).
    Page { path: "/dashboard/databases", exact: true, kind: "postgres", read: "postgres_get_project", args: |_, q| Some(json!({ "project": q.get("project")? })), shape: "/dashboard/databases?project=<id>", not: &[] },
    Page { path: "/dashboard/streams", exact: true, kind: "stream", read: "streams_get", args: |_, q| Some(json!({ "stream": q.get("stream")? })), shape: "/dashboard/streams?stream=<id>", not: &[] },
    Page { path: "/dashboard/queues/queues", exact: true, kind: "vhost", read: "queues_list_queues", args: |_, q| Some(json!({ "vhost": q.get("vhost")? })), shape: "/dashboard/queues/queues?vhost=<vhost>", not: &[] },
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
        "description": "Find any roll resource (project, service, repo, database, cluster, bucket, stream, queue, function, preview, token) by its name or part of it (\"checkout-db\"), part of its id, or a roll dashboard link a person pasted. Each match says its kind, id and name, and the tool and arguments that read it (read). A link is read at once: the answer is that read's.",
        "inputSchema": { "type": "object", "properties": { "query": { "type": "string", "description": "A name, part of one or of an id, or a roll dashboard URL" } }, "required": ["query"] },
        "annotations": { "readOnlyHint": true, "openWorldHint": false },
    })
}

/// What a dashboard link shows: its page, and the arguments of the read that shows it. Only the
/// path and the query are read, so any origin works (roll cloud, a local dashboard).
pub fn resolve(link: &str) -> Option<(&'static Page, Value)> {
    let url = url::Url::parse(link).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
    let path = url.path().trim_end_matches('/');
    let query: Query = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).filter(|(_, v)| !v.is_empty()).collect();
    PAGES.iter().find_map(|p| {
        let rest = if p.exact { (path == p.path).then_some("")? } else { path.strip_prefix(p.path)? };
        let segments: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(|s| percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()).collect();
        if segments.iter().any(String::is_empty) || segments.first().is_some_and(|f| p.not.contains(&f.as_str())) {
            return None;
        }
        (p.args)(&segments, &query).map(|a| (p, a))
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

/// One match: what it is, its id and name, the read that goes on from it, and its dashboard page
/// for a person (`web`: the dashboard's origin; none when roll doesn't know it).
fn found(k: &Kind, item: &Value, web: &str) -> Option<Value> {
    let id = k.id.iter().find_map(|f| item[*f].as_str())?;
    let name = ["name", "full_name", "title"].iter().find_map(|f| item[*f].as_str()).unwrap_or(id);
    let mut m = json!({ "kind": k.kind, "id": id, "name": name, "read": { "tool": k.read, "arguments": { k.param: id } } });
    if let Some(url) = page_url(k, id, web) {
        m["url"] = json!(url);
    }
    Some(m)
}

/// `k`'s dashboard page for `id` on `web`: each part of the id encoded (a repo's `owner/name` keeps
/// its `/`).
pub fn page_url(k: &Kind, id: &str, web: &str) -> Option<String> {
    let page = k.page.filter(|_| !web.is_empty())?;
    let id: Vec<String> = id.split('/').map(|p| percent_encoding::utf8_percent_encode(p, crate::tools::SEGMENT).to_string()).collect();
    Some(format!("{}{}", web.trim_end_matches('/'), page.replace("{}", &id.join("/"))))
}

/// Run `roll_find`: `call(tool, args, quiet)` calls a tool as the caller, `has(tool)` says whether
/// the connection has it.
pub async fn run<C, F>(args: &Value, web: &str, has: impl Fn(&str) -> bool, call: C) -> Value
where
    C: Fn(&'static str, Value, bool) -> F,
    F: std::future::Future<Output = Option<Value>>,
{
    let query = args["query"].as_str().map(str::trim).unwrap_or("");
    if query.is_empty() {
        return error("query is required: a name, part of an id, or a roll dashboard link");
    }
    if query.starts_with("http://") || query.starts_with("https://") {
        let Some((page, arguments)) = resolve(query) else {
            let pages: Vec<&str> = PAGES.iter().map(|p| p.shape).collect();
            return error(&format!("not a roll page this knows: {query} (it reads {})", pages.join(", ")));
        };
        if !has(page.read) {
            return error(&format!("{query} is a {}, and this connection has no {} to read it", page.kind, page.read));
        }
        // The read the agent would make itself: its answer as it is, approvals included.
        return call(page.read, arguments, false).await.unwrap_or_else(|| error(&format!("{} isn't available", page.read)));
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
                    if let (Some(rank), Some(m)) = (rank(item, &q), found(k, item, web)) {
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
        matches.iter().map(|m| {
            let url = m["url"].as_str().map(|u| format!(" {u}")).unwrap_or_default();
            // A name and an id as JSON strings: what someone else named a thing can't pass for a line.
            format!("{} {} ({}): {} {}{url}", m["kind"].as_str().unwrap_or(""), m["name"], m["id"], m["read"]["tool"].as_str().unwrap_or(""), m["read"]["arguments"])
        }).collect::<Vec<_>>().join("\n")
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
        let read = |link: &str| resolve(link).map(|(p, a)| (p.read, a));
        assert_eq!(read("https://roll.sh/dashboard/apps/svc_1?env=production"), Some(("apps_get_service", json!({ "service": "svc_1" }))));
        assert_eq!(read("http://127.0.0.1:3000/dashboard/code/acme/api/changes/12"), Some(("code_get_change", json!({ "change": "acme/api#12" }))));
        assert_eq!(read("https://roll.sh/dashboard/code/acme/api#settings"), Some(("code_list_changes", json!({ "repo": "acme/api" }))));
        assert_eq!(read("https://roll.sh/dashboard/storage/my%20bucket"), Some(("storage_get_bucket", json!({ "bucket": "my bucket" }))));
        assert_eq!(read("https://roll.sh/dashboard/access/approvals?id=apr_1"), Some(("access_wait_approval", json!({ "approval_id": "apr_1", "wait": 0 }))));
        assert_eq!(read("https://roll.sh/dashboard/access/audit?grant=tok_1&product=x"), Some(("access_get_token", json!({ "id": "tok_1" }))));
        assert_eq!(read("https://roll.sh/dashboard/queues/queues/%2F/orders"), Some(("queues_get_queue", json!({ "vhost": "/", "queue": "orders" }))));
        assert_eq!(read("https://roll.sh/dashboard/queues/queues?vhost=prod"), Some(("queues_list_queues", json!({ "vhost": "prod" }))));
        for nothing in ["https://roll.sh/dashboard/apps/visit?service=svc_1", "https://roll.sh/dashboard/storage/-/iam", "https://roll.sh/dashboard/code/acme", "https://roll.sh/dashboard/access", "https://roll.sh/dashboard/access/approvals", "https://roll.sh/dashboard/access/approvalsx?id=a", "file:///dashboard/apps/x"] {
            assert!(resolve(nothing).is_none(), "{nothing}");
        }
    }

    #[test]
    fn no_read_it_makes_reveals() {
        // roll_find's answer is the read's, and the inspector judges it by roll_find's name: so no
        // read it makes may be one that reveals a credential.
        let all = crate::tools::all();
        for read in KINDS.iter().map(|k| k.read).chain(PAGES.iter().map(|p| p.read)).filter(|r| !r.starts_with("code_")) {
            let t = all.iter().find(|t| t.name == read).unwrap();
            assert!(!t.actions().contains(&roll_auth::scope::Action::Reveal), "{read} reveals");
        }
    }

    #[test]
    fn a_matchs_page_reads_back_as_that_match() {
        // A match's link, pasted back, is the same read with the same arguments.
        for k in KINDS.iter().filter(|k| k.page.is_some()) {
            let id = if k.kind == "repo" { "acme/web app" } else { "x_1 é" };
            let url = page_url(k, id, "https://roll.example/").unwrap();
            let (page, args) = resolve(&url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!((page.read, args), (k.read, json!({ k.param: id })), "{url}");
        }
        assert!(page_url(&KINDS[0], "prj_1", "").is_none(), "no dashboard known, no link");
    }

    #[test]
    fn every_page_reads_with_a_tool_that_takes_its_arguments() {
        let all = crate::tools::all();
        for p in PAGES.iter().filter(|p| !p.read.starts_with("code_")) {
            let read = all.iter().find(|t| t.name == p.read).unwrap_or_else(|| panic!("{}", p.read));
            assert_eq!(read.method, "GET", "{}", p.read);
            // The shape's own example, filled, gives arguments the read takes, its required ones included.
            let link = format!("https://roll.sh{}", p.shape.replace("<owner>/<repo>", "o/r").replace("<n>", "1").replace(['<', '>'], ""));
            let (found, args) = resolve(&link).unwrap_or_else(|| panic!("{link}"));
            assert_eq!(found.read, p.read, "{link}");
            for k in args.as_object().unwrap().keys() {
                assert!(read.params.iter().any(|x| x.name == k), "{} takes no {k}", p.read);
            }
            for x in read.params.iter().filter(|x| x.required) {
                assert!(args.get(x.name).is_some(), "{} needs {}", p.read, x.name);
            }
        }
    }
}
