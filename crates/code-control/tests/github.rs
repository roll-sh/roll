//! GitHub-connected repos against a mock GitHub: its REST API is an axum router, its git side a
//! second roll code (fed real history by the git CLI), so imports, fetches and mirror pushes
//! go over the real smart HTTP protocol.

mod common;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use code_control::github::{self, Options};
use code_github::{AppConfig, ClientOptions, MirrorConfig};
use common::{App, app, git, write};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const INSTALLATION: u64 = 7;
const SECRET: &str = "webhook-secret";

#[derive(Default)]
struct Mock {
    base: Mutex<String>,
    /// (method, path, body) of every write.
    calls: Mutex<Vec<(String, String, Value)>>,
    issues: Mutex<Vec<Value>>,
    pulls: Mutex<HashMap<u64, Value>>,
    receive_packs: AtomicUsize,
    check_runs: AtomicU64,
    /// GitHub's git endpoint fails pushes while set.
    fail_pushes: std::sync::atomic::AtomicBool,
    /// Review ids handed out, and each review's inline comments (as GitHub made them).
    next_id: AtomicU64,
    review_comments: Mutex<HashMap<u64, Vec<Value>>>,
    /// Refuse APPROVE / REQUEST_CHANGES reviews (as GitHub does for some Apps) while set.
    reject_verdicts: std::sync::atomic::AtomicBool,
    /// Collaborator permissions by login (anyone else: 404, as GitHub answers for strangers).
    perms: Mutex<HashMap<String, String>>,
    /// Review comments that start a thread (REST ids), for GraphQL's `reviewThreads`.
    thread_roots: Mutex<Vec<u64>>,
    /// Pickers: labels made through roll, the list pages read (`path?page=n`), GitHub's rate
    /// limit while set, and the org's teams refused (no Members permission) while set.
    made_labels: Mutex<Vec<Value>>,
    reads: Mutex<Vec<String>>,
    rate_limited: std::sync::atomic::AtomicBool,
    teams_forbidden: std::sync::atomic::AtomicBool,
    /// GitHub's REST API answers 503 to everything while set (an outage).
    down: std::sync::atomic::AtomicBool,
    /// GitHub's git endpoint fails fetches (upload-pack) while set.
    fail_fetches: std::sync::atomic::AtomicBool,
    /// The App's webhook delivery log (newest first), and the attempts roll asked for.
    hook_log: Mutex<Vec<Value>>,
    redeliveries: Mutex<Vec<u64>>,
}

type S = State<Arc<Mock>>;

fn record(m: &Mock, method: &str, path: String, body: Value) {
    m.calls.lock().push((method.into(), path, body));
}

fn repo_json(m: &Mock) -> Value {
    let base = m.base.lock().clone();
    json!({
        "id": 1, "name": "app", "full_name": "acme/app", "private": false, "default_branch": "main",
        "owner": { "login": "acme", "id": 1, "type": "Organization" },
        "clone_url": format!("{base}/git/acme/app.git"), "html_url": format!("{base}/acme/app"),
    })
}

async fn token(Path(_id): Path<u64>) -> Json<Value> {
    let exp = chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp() + 3600, 0).unwrap();
    Json(json!({ "token": "ghs_test", "expires_at": exp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) }))
}

async fn installations() -> Json<Value> {
    Json(json!([{ "id": INSTALLATION, "account": { "login": "acme", "id": 1, "type": "Organization" } }]))
}

async fn installation_repos(State(m): S) -> Json<Value> {
    Json(json!({ "total_count": 1, "repositories": [repo_json(&m)] }))
}

async fn repo(State(m): S) -> Json<Value> {
    Json(repo_json(&m))
}

async fn branch(Path((_o, _r, b)): Path<(String, String, String)>) -> Json<Value> {
    Json(json!({ "name": b, "commit": { "sha": "0".repeat(40) }, "protected": false }))
}

async fn issues(State(m): S) -> Json<Value> {
    Json(Value::Array(m.issues.lock().clone()))
}

async fn issue(State(m): S, Path((_o, _r, n)): Path<(String, String, u64)>) -> Response {
    match m.issues.lock().iter().find(|i| i["number"] == n) {
        Some(i) => Json(i.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" }))).into_response(),
    }
}

async fn patch_issue(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "PATCH", format!("/repos/{o}/{r}/issues/{n}"), b.clone());
    let mut issues = m.issues.lock();
    let i = issues.iter_mut().find(|i| i["number"] == n).unwrap();
    if let Some(s) = b.get("state") {
        i["state"] = s.clone();
    }
    Json(i.clone())
}

async fn comment(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/issues/{n}/comments"), b.clone());
    Json(json!({ "id": 1, "body": b["body"] }))
}

async fn list_comments(State(m): S, Path((o, r, n)): Path<(String, String, u64)>) -> Json<Value> {
    let path = format!("/repos/{o}/{r}/issues/{n}/comments");
    let made: Vec<Value> = m.calls.lock().iter().filter(|(meth, p, _)| meth == "POST" && *p == path).map(|(_, _, b)| json!({ "id": 1, "body": b["body"], "user": { "login": "roll[bot]", "type": "Bot" } })).collect();
    Json(json!(made))
}

async fn edit_comment(State(m): S, Path((o, r, id)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "PATCH", format!("/repos/{o}/{r}/issues/comments/{id}"), b.clone());
    Json(json!({ "id": id, "body": b["body"] }))
}

async fn create_deployment(State(m): S, Path((o, r)): Path<(String, String)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/deployments"), b.clone());
    Json(json!({ "id": 900 + m.next_id.fetch_add(1, Ordering::SeqCst) }))
}

async fn deployment_status(State(m): S, Path((o, r, id)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/deployments/{id}/statuses"), b.clone());
    Json(json!({ "id": 1, "state": b["state"] }))
}

async fn pulls(State(m): S, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> Json<Value> {
    // `head=owner:branch` narrows it to that branch's pull request, as GitHub does.
    let head = q.get("head").and_then(|h| h.split_once(':')).map(|(_, b)| b.to_string());
    Json(Value::Array(m.pulls.lock().values().filter(|p| p["state"] == "open" && head.as_ref().is_none_or(|h| p["head"]["ref"] == h.as_str())).cloned().collect()))
}

async fn create_pull(State(m): S, Path((o, r)): Path<(String, String)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/pulls"), b.clone());
    let n = 50 + m.pulls.lock().len() as u64;
    let pr = json!({
        "number": n, "title": b["title"], "body": b["body"], "state": "open", "draft": b["draft"], "html_url": format!("https://github.com/{o}/{r}/pull/{n}"),
        "user": { "login": "roll-test[bot]", "id": 1 }, "head": { "ref": b["head"], "sha": "0".repeat(40), "repo": repo_json(&m) }, "base": { "ref": b["base"], "sha": "0".repeat(40), "repo": repo_json(&m) },
    });
    m.pulls.lock().insert(n, pr.clone());
    Json(pr)
}

async fn add_labels(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/issues/{n}/labels"), b.clone());
    Json(Value::Array(b["labels"].as_array().cloned().unwrap_or_default().into_iter().map(|l| json!({ "name": l })).collect()))
}

async fn remove_label(State(m): S, Path((o, r, n, l)): Path<(String, String, u64, String)>) -> Response {
    record(&m, "DELETE", format!("/repos/{o}/{r}/issues/{n}/labels/{l}"), Value::Null);
    // GitHub's answer for a label the issue doesn't have.
    if l == "absent" { (StatusCode::NOT_FOUND, Json(json!({ "message": "Label does not exist" }))).into_response() } else { Json(json!([])).into_response() }
}

async fn create_status(State(m): S, Path((o, r, sha)): Path<(String, String, String)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/statuses/{sha}"), b.clone());
    Json(json!({ "id": 1, "state": b["state"], "context": b["context"] }))
}

async fn pull(State(m): S, Path((_o, _r, n)): Path<(String, String, u64)>) -> Response {
    match m.pulls.lock().get(&n) {
        Some(p) => Json(p.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" }))).into_response(),
    }
}

async fn patch_pull(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "PATCH", format!("/repos/{o}/{r}/pulls/{n}"), b.clone());
    let mut pulls = m.pulls.lock();
    let p = pulls.get_mut(&n).unwrap();
    if let Some(s) = b.get("state") {
        p["state"] = s.clone();
    }
    Json(p.clone())
}

async fn create_check(State(m): S, Path((o, r)): Path<(String, String)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/check-runs"), b.clone());
    let id = m.check_runs.fetch_add(1, Ordering::SeqCst) + 1;
    Json(json!({ "id": id, "name": b["name"], "head_sha": b["head_sha"], "status": b["status"] }))
}

async fn update_check(State(m): S, Path((o, r, id)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "PATCH", format!("/repos/{o}/{r}/check-runs/{id}"), b.clone());
    Json(json!({ "id": id, "name": "x", "head_sha": "0".repeat(40), "status": b["status"] }))
}

async fn create_review(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Response {
    record(&m, "POST", format!("/repos/{o}/{r}/pulls/{n}/reviews"), b.clone());
    if m.reject_verdicts.load(Ordering::SeqCst) && b["event"] != "COMMENT" {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "message": "Can not approve your own pull request" }))).into_response();
    }
    let id = 9_000 + m.next_id.fetch_add(1, Ordering::SeqCst);
    let comments: Vec<Value> = b["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let cid = 70_000 + m.next_id.fetch_add(1, Ordering::SeqCst);
            json!({ "id": cid, "pull_request_review_id": id, "path": c["path"], "line": c["line"], "side": c["side"], "body": c["body"], "user": { "login": "roll-test[bot]", "type": "Bot" } })
        })
        .collect();
    m.review_comments.lock().insert(id, comments);
    Json(json!({ "id": id, "state": b["event"], "body": b["body"], "html_url": format!("https://github.com/{o}/{r}/pull/{n}#pullrequestreview-{id}"), "user": { "login": "roll-test[bot]", "type": "Bot" } })).into_response()
}

async fn review_comments(State(m): S, Path((_o, _r, _n, id)): Path<(String, String, u64, u64)>) -> Json<Value> {
    Json(Value::Array(m.review_comments.lock().get(&id).cloned().unwrap_or_default()))
}

async fn create_pr_comment(State(m): S, Path((o, r, n)): Path<(String, String, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/pulls/{n}/comments"), b.clone());
    let id = 80_000 + m.next_id.fetch_add(1, Ordering::SeqCst);
    m.thread_roots.lock().push(id);
    Json(json!({ "id": id, "path": b["path"], "line": b["line"], "body": b["body"], "user": { "login": "roll-test[bot]", "type": "Bot" } }))
}

/// GitHub's GraphQL API: review threads (node `PRRT_<root comment id>`) and resolving them.
async fn graphql(State(m): S, Json(b): Json<Value>) -> Json<Value> {
    let q = b["query"].as_str().unwrap_or("").to_string();
    if q.contains("reviewThreads") {
        let mut roots = m.thread_roots.lock().clone();
        roots.extend(m.review_comments.lock().values().flatten().filter_map(|c| c["id"].as_u64()));
        record(&m, "GRAPHQL", "reviewThreads".into(), b["variables"].clone());
        let nodes: Vec<Value> = roots.iter().map(|r| json!({ "id": format!("PRRT_{r}"), "comments": { "nodes": [{ "databaseId": r }] } })).collect();
        return Json(json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } } } } }));
    }
    let resolve = !q.contains("unresolveReviewThread");
    let id = b["variables"]["id"].clone();
    record(&m, "GRAPHQL", if resolve { "resolve".into() } else { "unresolve".into() }, id.clone());
    let field = if resolve { "resolveReviewThread" } else { "unresolveReviewThread" };
    Json(json!({ "data": { field: { "thread": { "id": id, "isResolved": resolve } } } }))
}

async fn reply_pr_comment(State(m): S, Path((o, r, n, cid)): Path<(String, String, u64, u64)>, Json(b): Json<Value>) -> Json<Value> {
    record(&m, "POST", format!("/repos/{o}/{r}/pulls/{n}/comments/{cid}/replies"), b.clone());
    let id = 90_000 + m.next_id.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "id": id, "in_reply_to_id": cid, "body": b["body"], "user": { "login": "roll-test[bot]", "type": "Bot" } }))
}

async fn permission(State(m): S, Path((_o, _r, user)): Path<(String, String, String)>) -> Response {
    match m.perms.lock().get(&user) {
        Some(p) => Json(json!({ "permission": p, "user": { "login": user } })).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" }))).into_response(),
    }
}

async fn hook_log(State(m): S) -> Json<Value> {
    Json(Value::Array(m.hook_log.lock().clone()))
}

async fn redeliver(State(m): S, Path(id): Path<u64>) -> StatusCode {
    m.redeliveries.lock().push(id);
    StatusCode::ACCEPTED
}

// ── what pickers list ──

type ListQuery = axum::extract::Query<HashMap<String, String>>;

/// A list as GitHub pages it (`per_page`, `page`, a `Link` to the next), or its rate limit.
fn listing(m: &Mock, path: &str, q: &HashMap<String, String>, all: Vec<Value>, key: Option<&str>) -> Response {
    let per: usize = q.get("per_page").and_then(|p| p.parse().ok()).unwrap_or(30);
    let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    m.reads.lock().push(format!("{path}?page={page}"));
    if m.rate_limited.load(Ordering::SeqCst) {
        let mut res = (StatusCode::FORBIDDEN, Json(json!({ "message": "API rate limit exceeded for installation ID 7." }))).into_response();
        res.headers_mut().insert("x-ratelimit-remaining", "0".parse().unwrap());
        res.headers_mut().insert("x-ratelimit-reset", (chrono::Utc::now().timestamp() + 60).to_string().parse().unwrap());
        return res;
    }
    let chunk: Vec<Value> = all.iter().skip((page - 1) * per).take(per).cloned().collect();
    let body = match key {
        None => Value::Array(chunk),
        Some(k) => json!({ "total_count": all.len(), k: chunk }),
    };
    let mut res = Json(body).into_response();
    if page * per < all.len() {
        let next = format!("<{}/api{path}?per_page={per}&page={}>; rel=\"next\"", m.base.lock(), page + 1);
        res.headers_mut().insert("link", next.parse().unwrap());
    }
    res
}

async fn list_branches(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let b = |name: &str, protected: bool| json!({ "name": name, "commit": { "sha": "0".repeat(40) }, "protected": protected });
    listing(&m, &format!("/repos/{o}/{r}/branches"), &q, vec![b("main", true), b("feature", false), b("release/1.0", true)], None)
}

fn seed_labels(m: &Mock) -> Vec<Value> {
    let mut all = vec![
        json!({ "id": 1, "name": "bug", "color": "d73a4a", "description": "Something isn't working" }),
        json!({ "id": 2, "name": "docs", "color": "0075ca", "description": null }),
        json!({ "id": 3, "name": "P1", "color": "b60205" }),
    ];
    all.extend(m.made_labels.lock().iter().cloned());
    all
}

async fn list_labels(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    listing(&m, &format!("/repos/{o}/{r}/labels"), &q, seed_labels(&m), None)
}

async fn create_label(State(m): S, Path((o, r)): Path<(String, String)>, Json(b): Json<Value>) -> Response {
    record(&m, "POST", format!("/repos/{o}/{r}/labels"), b.clone());
    if seed_labels(&m).iter().any(|l| l["name"] == b["name"]) {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "message": "Validation Failed", "errors": [{ "code": "already_exists" }] }))).into_response();
    }
    let l = json!({ "id": 100, "name": b["name"], "color": b["color"].as_str().unwrap_or("ededed"), "description": null });
    m.made_labels.lock().push(l.clone());
    (StatusCode::CREATED, Json(l)).into_response()
}

async fn list_assignees(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let people = vec![
        json!({ "login": "ada", "id": 1, "type": "User", "avatar_url": "https://avatars.githubusercontent.com/u/1" }),
        json!({ "login": "octo-bot", "id": 2, "type": "Bot", "avatar_url": "http://insecure.example/x.png" }),
    ];
    listing(&m, &format!("/repos/{o}/{r}/assignees"), &q, people, None)
}

async fn list_collaborators(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let people = vec![json!({ "login": "ada", "id": 1, "type": "User" }), json!({ "login": "grace", "id": 3, "type": "User" })];
    listing(&m, &format!("/repos/{o}/{r}/collaborators"), &q, people, None)
}

async fn list_teams(State(m): S, Path(org): Path<String>, axum::extract::Query(q): ListQuery) -> Response {
    if m.teams_forbidden.load(Ordering::SeqCst) {
        return (StatusCode::FORBIDDEN, Json(json!({ "message": "Resource not accessible by integration" }))).into_response();
    }
    let teams = vec![json!({ "id": 1, "slug": "core", "name": "Core" }), json!({ "id": 2, "slug": "web", "name": "Web platform" })];
    listing(&m, &format!("/orgs/{org}/teams"), &q, teams, None)
}

async fn list_milestones(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let all = vec![
        json!({ "number": 1, "title": "v1.0", "state": "open", "due_on": "2026-12-01T08:00:00Z", "open_issues": 3 }),
        json!({ "number": 2, "title": "Later", "state": "open", "due_on": null, "open_issues": 4 }),
    ];
    listing(&m, &format!("/repos/{o}/{r}/milestones"), &q, all, None)
}

async fn list_workflows(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let all = vec![
        json!({ "id": 11, "name": "CI", "path": ".github/workflows/ci.yml", "state": "active" }),
        json!({ "id": 12, "name": "Nightly", "path": ".github/workflows/nightly.yml", "state": "disabled_manually" }),
    ];
    listing(&m, &format!("/repos/{o}/{r}/actions/workflows"), &q, all, Some("workflows"))
}

async fn list_environments(State(m): S, Path((o, r)): Path<(String, String)>, axum::extract::Query(q): ListQuery) -> Response {
    let all = vec![json!({ "id": 1, "name": "production" }), json!({ "id": 2, "name": "staging" })];
    listing(&m, &format!("/repos/{o}/{r}/environments"), &q, all, Some("environments"))
}

async fn count_pushes(State(m): S, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if m.down.load(Ordering::SeqCst) && req.uri().path().starts_with("/api/") {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "message": "unicorn" }))).into_response();
    }
    if m.fail_fetches.load(Ordering::SeqCst) && req.uri().path().ends_with("/git-upload-pack") {
        return (StatusCode::SERVICE_UNAVAILABLE, "unicorn\n").into_response();
    }
    if req.uri().path().ends_with("/git-receive-pack") {
        if m.fail_pushes.load(Ordering::SeqCst) {
            return (StatusCode::SERVICE_UNAVAILABLE, "unicorn\n").into_response();
        }
        m.receive_packs.fetch_add(1, Ordering::SeqCst);
    }
    next.run(req).await
}

/// The mock GitHub: REST under `/api`, git (a second roll code) under `/git`.
struct GitHub {
    base: String,
    mock: Arc<Mock>,
    gh: App,
}

async fn github_mock() -> GitHub {
    let mock = Arc::new(Mock::default());
    let gh = app(None);
    gh.repo("acme", "app").await;
    let rest = Router::new()
        .route("/app/installations", get(installations))
        .route("/app/hook/deliveries", get(hook_log))
        .route("/app/hook/deliveries/{id}/attempts", post(redeliver))
        .route("/app/installations/{id}/access_tokens", post(token))
        .route("/installation/repositories", get(installation_repos))
        .route("/repos/{o}/{r}", get(repo))
        .route("/repos/{o}/{r}/branches/{b}", get(branch))
        .route("/repos/{o}/{r}/branches", get(list_branches))
        .route("/repos/{o}/{r}/labels", get(list_labels).post(create_label))
        .route("/repos/{o}/{r}/assignees", get(list_assignees))
        .route("/repos/{o}/{r}/collaborators", get(list_collaborators))
        .route("/repos/{o}/{r}/milestones", get(list_milestones))
        .route("/repos/{o}/{r}/actions/workflows", get(list_workflows))
        .route("/repos/{o}/{r}/environments", get(list_environments))
        .route("/orgs/{org}/teams", get(list_teams))
        .route("/repos/{o}/{r}/issues", get(issues))
        .route("/repos/{o}/{r}/issues/{n}", get(issue).patch(patch_issue))
        .route("/repos/{o}/{r}/issues/{n}/comments", get(list_comments).post(comment))
        .route("/repos/{o}/{r}/issues/comments/{id}", axum::routing::patch(edit_comment))
        .route("/repos/{o}/{r}/deployments", post(create_deployment))
        .route("/repos/{o}/{r}/deployments/{id}/statuses", post(deployment_status))
        .route("/repos/{o}/{r}/issues/{n}/labels", post(add_labels))
        .route("/repos/{o}/{r}/issues/{n}/labels/{l}", axum::routing::delete(remove_label))
        .route("/repos/{o}/{r}/statuses/{sha}", post(create_status))
        .route("/repos/{o}/{r}/pulls", get(pulls).post(create_pull))
        .route("/repos/{o}/{r}/pulls/{n}", get(pull).patch(patch_pull))
        .route("/repos/{o}/{r}/pulls/{n}/reviews", post(create_review))
        .route("/repos/{o}/{r}/pulls/{n}/reviews/{id}/comments", get(review_comments))
        .route("/repos/{o}/{r}/pulls/{n}/comments", post(create_pr_comment))
        .route("/repos/{o}/{r}/pulls/{n}/comments/{cid}/replies", post(reply_pr_comment))
        .route("/repos/{o}/{r}/collaborators/{u}/permission", get(permission))
        .route("/repos/{o}/{r}/check-runs", post(create_check))
        .route("/repos/{o}/{r}/check-runs/{id}", axum::routing::patch(update_check))
        .route("/graphql", post(graphql))
        .with_state(mock.clone());
    let router = Router::new()
        .nest("/api", rest)
        .merge(gh.router.clone())
        .layer(axum::middleware::from_fn_with_state(mock.clone(), count_pushes));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *mock.base.lock() = base.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    GitHub { base, mock, gh }
}

/// One App key for every test in the file (generating an RSA-2048 key per test was slow).
fn test_key() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    static KEY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    KEY.get_or_init(|| rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap().to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string()).clone()
}

fn enable(a: &App, g: &GitHub, mirror: MirrorConfig) {
    let mut config = AppConfig::new(1, test_key(), SECRET, "roll-test");
    config.api_base = format!("{}/api", g.base);
    config.web_base = format!("{}/git", g.base);
    let client = ClientOptions { max_retries: 1, timeout: Duration::from_secs(10), ..Default::default() };
    github::enable(&a.code, config, Options { mirror, client, ..Default::default() }).unwrap();
}

fn fast_mirror() -> MirrorConfig {
    MirrorConfig {
        debounce: Duration::from_millis(100),
        max_delay: Duration::from_millis(400),
        backoff_initial: Duration::from_millis(50),
        backoff_max: Duration::from_secs(1),
    }
}

/// A preview backend that records visual reviews accepted through it.
#[derive(Default)]
struct Accepted(Mutex<Vec<(String, String, String)>>);

#[async_trait::async_trait]
impl code_control::PreviewBackend for Accepted {
    async fn create(&self, _: &code_control::model::RepoRow, _: &code_control::model::Change, _: &std::path::Path) -> anyhow::Result<code_control::model::Preview> {
        anyhow::bail!("not in this test")
    }
    async fn delete(&self, _: &code_control::model::RepoRow, _: &code_control::model::Change) -> anyhow::Result<()> {
        Ok(())
    }
    async fn accept_visual(&self, change: &code_control::model::Change, by: &str, head: &str) -> anyhow::Result<()> {
        self.0.lock().push((change.id.clone(), by.to_string(), head.to_string()));
        Ok(())
    }
}

async fn hook(a: &App, event: &str, delivery: &str, body: &Value, secret: &str) -> StatusCode {
    let body = body.to_string();
    let sig = code_github::webhook::sign(secret.as_bytes(), body.as_bytes());
    let req = axum::http::Request::post("/v1/code/github/webhook")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", sig)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    use tower::ServiceExt;
    a.router.clone().oneshot(req).await.unwrap().status()
}

async fn until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The import brought its PRs in: there are changes, and each has its PR number (a PR's change
/// is created first and linked to its PR right after, so the list can show it in between).
async fn imported(a: &App) -> bool {
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    let changes = changes.as_array().unwrap();
    !changes.is_empty() && changes.iter().all(|c| !c["github_pr"].is_null())
}

fn issue_json(n: u64, title: &str, state: &str, labels: &[&str]) -> Value {
    json!({
        "number": n, "title": title, "body": format!("body of {n}"), "state": state,
        "labels": labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
        "html_url": format!("https://github.com/acme/app/issues/{n}"), "user": { "login": "octo", "id": 9 },
    })
}

fn pr_json(m: &Mock, n: u64, branch: &str, sha: &str, base_sha: &str) -> Value {
    json!({
        "number": n, "title": format!("PR {n}: {branch}"), "body": "please land", "state": "open", "draft": false,
        "html_url": format!("https://github.com/acme/app/pull/{n}"), "user": { "login": "octo", "id": 9 },
        "head": { "ref": branch, "sha": sha, "repo": repo_json(m) },
        "base": { "ref": "main", "sha": base_sha, "repo": repo_json(m) },
    })
}

/// Real history on the mock GitHub: merges, an exec bit, a tag, and a `feature` branch.
fn seed(home: &std::path::Path, url: &str) -> std::path::PathBuf {
    let w = home.join("seed");
    std::fs::create_dir_all(&w).unwrap();
    git(home, &w, &["init", "-q"]);
    write(&w, "README.md", "# app\n");
    write(&w, "src/main.rs", "fn main() {}\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "initial"]);
    git(home, &w, &["checkout", "-qb", "side"]);
    write(&w, "side.txt", "side\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "side work"]);
    git(home, &w, &["checkout", "-q", "main"]);
    write(&w, "run.sh", "#!/bin/sh\necho hi\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["update-index", "--chmod=+x", "run.sh"]);
    git(home, &w, &["commit", "-qm", "script"]);
    git(home, &w, &["merge", "-q", "--no-ff", "side", "-m", "merge side"]);
    git(home, &w, &["tag", "-a", "v1", "-m", "release 1"]);
    git(home, &w, &["checkout", "-qb", "feature"]);
    write(&w, "feature.txt", "feature\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "add feature"]);
    git(home, &w, &["checkout", "-q", "main"]);
    git(home, &w, &["remote", "add", "origin", url]);
    git(home, &w, &["push", "-q", "origin", "main", "feature", "v1"]);
    w
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_import_webhooks_and_mirror() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    g.mock.issues.lock().extend([issue_json(1, "Fix the flux capacitor", "open", &["bug", "P1"]), issue_json(2, "Old news", "closed", &[])]);
    let pr3 = pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha);
    g.mock.pulls.lock().insert(3, pr3);

    let a = app(None);
    enable(&a, &g, fast_mirror());
    let info = a.ok("GET", "/v1/code/github", None).await;
    assert_eq!(info["configured"], true);
    let inst = a.ok("GET", "/v1/code/github/installations", None).await;
    assert_eq!(inst[0]["repos"][0]["full_name"], "acme/app");
    assert!(inst[0]["repos"][0]["connected_repo"].is_null());

    // Connect: the history comes over git, byte-exact.
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    let repo = a
        .ok(
            "POST",
            "/v1/code/repos",
            Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } })),
        )
        .await;
    assert_eq!(repo["source"], "github");
    assert_eq!(repo["github"]["owner"], "acme");
    let repo_id = repo["id"].as_str().unwrap().to_string();
    until("import", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["import"]["state"] == "done" }).await;
    let repo = a.ok("GET", "/v1/code/repos/acme/app", None).await;
    assert_eq!(repo["head_git"], main_sha);
    for sha in git(home, &w, &["rev-list", "--all"]).lines() {
        let oid = code_store::Oid(code_git::ObjectId::from_hex(sha.as_bytes()).unwrap().0);
        let cs = a.code.store.changeset_of_git(&repo_id, &oid).unwrap().unwrap_or_else(|| panic!("{sha} imported"));
        assert_eq!(a.code.store.derive_git(&repo_id, &cs).unwrap().0.hex(), sha);
    }
    let refs = a.ok("GET", "/v1/code/repos/acme/app/refs", None).await;
    let names: Vec<&str> = refs.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"feature") && names.iter().any(|n| n.contains("v1")), "{names:?}");
    let inst = a.ok("GET", "/v1/code/github/installations", None).await;
    assert_eq!(inst[0]["repos"][0]["connected_repo"], "acme/app");

    // Open issues became work items, open PRs changes.
    until("issue import", || async { !a.ok("GET", "/v1/code/repos/acme/app/work", None).await.as_array().unwrap().is_empty() }).await;
    until("PR import", || async { imported(&a).await }).await;
    let work = a.ok("GET", "/v1/code/repos/acme/app/work", None).await;
    assert_eq!(work.as_array().unwrap().len(), 1, "closed issues aren't imported: {work}");
    assert_eq!(work[0]["source"], "github#1");
    assert_eq!(work[0]["priority"], 1);
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    assert_eq!(changes[0]["github_pr"], 3);
    assert_eq!(changes[0]["branch"], "feature");
    let pr_change = changes[0]["id"].as_str().unwrap().to_string();

    // Webhooks: signature checked, deliveries idempotent.
    let opened = json!({ "action": "opened", "issue": issue_json(4, "Add dark mode", "open", &["P0"]), "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "issues", "d-1", &opened, "wrong").await, StatusCode::UNAUTHORIZED);
    assert_eq!(hook(&a, "issues", "d-1", &opened, SECRET).await, StatusCode::OK);
    assert_eq!(hook(&a, "issues", "d-1", &opened, SECRET).await, StatusCode::OK);
    assert_eq!(hook(&a, "issues", "d-2", &opened, SECRET).await, StatusCode::OK);
    let work = a.ok("GET", "/v1/code/repos/acme/app/work", None).await;
    let fours: Vec<&Value> = work.as_array().unwrap().iter().filter(|w| w["source"] == "github#4").collect();
    assert_eq!(fours.len(), 1, "{work}");
    assert_eq!(fours[0]["priority"], 0);
    // Closed on GitHub: done in roll, and nothing is said back.
    let mut closed_issue = issue_json(4, "Add dark mode", "closed", &[]);
    closed_issue["state_reason"] = json!("completed");
    let closed = json!({ "action": "closed", "issue": closed_issue, "repository": repo_json(&g.mock) });
    assert_eq!(hook(&a, "issues", "d-3", &closed, SECRET).await, StatusCode::OK);
    let w4 = a.ok("GET", "/v1/code/repos/acme/app/work", None).await;
    assert_eq!(w4.as_array().unwrap().iter().find(|w| w["source"] == "github#4").unwrap()["state"], "done");

    // A new PR from a branch pushed to GitHub becomes a change.
    write(&w, "topic.txt", "topic\n");
    git(home, &w, &["checkout", "-qb", "topic"]);
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "topic work"]);
    git(home, &w, &["push", "-q", "origin", "topic"]);
    let topic_sha = git(home, &w, &["rev-parse", "topic"]).trim().to_string();
    git(home, &w, &["checkout", "-q", "main"]);
    let pr5 = pr_json(&g.mock, 5, "topic", &topic_sha, &main_sha);
    g.mock.pulls.lock().insert(5, pr5.clone());
    let ev = json!({ "action": "opened", "number": 5, "pull_request": pr5, "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "pull_request", "d-4", &ev, SECRET).await, StatusCode::OK);
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    let c5 = changes.as_array().unwrap().iter().find(|c| c["github_pr"] == 5).unwrap().clone();
    let c5_git = a.code.store.derive_git(&repo_id, &a.code.store.resolve(&repo_id, c5["head"].as_str().unwrap()).unwrap().unwrap()).unwrap().0;
    assert_eq!(c5_git.hex(), topic_sha);
    // A label on the pull request is the change's (roll previews reads `no-preview`, `preview`).
    let mut labelled = pr5.clone();
    labelled["labels"] = json!([{ "name": "no-preview" }]);
    let ev = json!({ "action": "labeled", "number": 5, "pull_request": labelled, "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "pull_request", "d-4l", &ev, SECRET).await, StatusCode::OK);
    let c5_now = a.ok("GET", &format!("/v1/code/changes/{}", c5["id"].as_str().unwrap()), None).await;
    assert_eq!(c5_now["labels"], json!(["no-preview"]));

    // Checks on a PR's change become check runs.
    let c5_id = c5["id"].as_str().unwrap();
    a.ok("POST", &format!("/v1/code/changes/{c5_id}/checks"), Some(json!({ "name": "test", "status": "running" }))).await;
    until("check run", || async { g.mock.calls.lock().iter().any(|(m, p, _)| m == "POST" && p.ends_with("/check-runs")) }).await;
    a.ok("POST", &format!("/v1/code/changes/{c5_id}/checks"), Some(json!({ "name": "test", "status": "success", "summary": "42 passed" }))).await;
    until("check run update", || async {
        g.mock.calls.lock().iter().any(|(m, p, b)| m == "PATCH" && p.ends_with("/check-runs/1") && b["conclusion"] == "success")
    })
    .await;

    // A preview environment on a PR's change (roll previews): one comment, edited in place as
    // the preview changes; a `roll/preview` status on the head; a deployment that goes inactive
    // when the preview is removed.
    let env = |state: &str| code_control::model::Preview {
        state: code_control::model::PreviewState::Running,
        url: Some("http://web-pr5-shop.preview.test".into()),
        postgres: None,
        redis: None,
        error: None,
        created_at: 1,
        environment: Some(code_control::model::PreviewEnvironment {
            id: "pv_test".into(),
            slug: "pr5-shop".into(),
            state: state.into(),
            services: vec![code_control::model::PreviewServiceLink { name: "web".into(), url: "http://web-pr5-shop.preview.test".into(), state: "sleeping".into() }],
            resources: Vec::new(),
            sleeping_usd_month: 0.01,
            active_usd_hour: 0.002,
            dashboard_url: None,
            github: Default::default(),
            head: None,
            schema_changes: None,
            screenshot_url: None,
            expires_at: None,
            earlier: Vec::new(),
            visual_change: None,
            page_speed: None,
            page_health: None,
            page_a11y: None,
            base_change: None,
            diff_url: None,
            pages: Vec::new(),
            speed_budget: None,
            visual_review: None,
        }),
    };
    let preview_calls = |path: &str| g.mock.calls.lock().iter().filter(|(_, p, _)| p.contains(path)).cloned().collect::<Vec<_>>();
    a.code.report_environment(c5_id, Some(&env("creating"))).unwrap();
    until("preview comment, status and deployment", || async {
        !preview_calls("/issues/5/comments").is_empty() && !preview_calls("/deployments/").is_empty()
    })
    .await;
    a.code.report_environment(c5_id, Some(&env("ready"))).unwrap();
    until("the comment edited, the status green", || async {
        !preview_calls("/issues/comments/").is_empty()
            && preview_calls("/statuses/").iter().any(|(_, _, b)| b["context"] == "roll/preview" && b["state"] == "success")
    })
    .await;
    assert_eq!(preview_calls("/issues/5/comments").len(), 1, "one comment, edited in place");
    assert_eq!(preview_calls("/deployments").iter().filter(|(_, p, _)| p.ends_with("/deployments")).count(), 1, "one deployment per head");

    // A visual change waiting for a review: `roll/visual`, a check run with Accept. A click is
    // accepted for a writer who isn't the change's author, through the preview backend.
    let accepted = Arc::new(Accepted::default());
    a.code.set_preview_backend(accepted.clone());
    let mut waiting = env("ready");
    waiting.environment.as_mut().unwrap().visual_review =
        Some(code_control::model::PreviewVisualReview { state: "changed".into(), branch: "main".into(), percent: 8.0, by: None, required: false, head: Some("cs_seen".into()), pages: Vec::new() });
    a.code.report_environment(c5_id, Some(&waiting)).unwrap();
    let visual = || g.mock.calls.lock().iter().filter(|(m, p, b)| m == "POST" && p.ends_with("/check-runs") && b["name"] == "roll/visual").map(|(_, _, b)| b.clone()).collect::<Vec<_>>();
    until("the roll/visual check run", || async { !visual().is_empty() }).await;
    let run = visual().remove(0);
    assert_eq!((run["conclusion"].clone(), run["actions"][0]["identifier"].clone(), run["external_id"].clone()), (json!("action_required"), json!("accept"), json!(c5_id)));
    let sha = run["head_sha"].as_str().unwrap().to_string();
    let click = |login: &str, sha: &str| json!({ "action": "requested_action", "check_run": { "id": 1, "name": "roll/visual", "head_sha": sha, "external_id": c5_id },
        "requested_action": { "identifier": "accept" }, "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION }, "sender": { "login": login } });
    let author = c5["author"].as_str().unwrap().to_string();
    g.mock.perms.lock().insert(author.clone(), "write".into());
    g.mock.perms.lock().insert("reader".into(), "read".into());
    g.mock.perms.lock().insert("rev".into(), "write".into());
    // Its author, a reader, a writer on an older push's check run, then a writer on this one's.
    let stale = "0".repeat(40);
    for (i, (who, sha)) in [(author.as_str(), sha.as_str()), ("reader", sha.as_str()), ("rev", stale.as_str()), ("rev", sha.as_str())].into_iter().enumerate() {
        assert_eq!(hook(&a, "check_run", &format!("d-visual-{i}"), &click(who, sha), SECRET).await, StatusCode::OK);
    }
    until("the writer's Accept", || async { !accepted.0.lock().is_empty() }).await;
    assert_eq!(*accepted.0.lock(), vec![(c5_id.to_string(), "rev".to_string(), "cs_seen".to_string())], "not its author's, a reader's or an older push's");
    a.code.report_environment(c5_id, None).unwrap();
    until("the deployment inactive", || async { preview_calls("/deployments/").iter().any(|(_, _, b)| b["state"] == "inactive") }).await;
    // Removed before it was ready, its pending status is settled: nothing waits on that commit.
    let pending = || preview_calls("/statuses/").iter().filter(|(_, _, b)| b["state"] == "pending").count();
    let before = pending();
    a.code.report_environment(c5_id, Some(&env("creating"))).unwrap();
    until("a pending status", || async { pending() > before }).await;
    a.code.report_environment(c5_id, None).unwrap();
    until("the pending status settled", || async {
        preview_calls("/statuses/").iter().any(|(_, _, b)| b["state"] == "success" && b["description"] == "Preview removed before it was ready")
    })
    .await;

    // Landing the PR's change: mirrored to GitHub, the PR commented on and closed.
    let landed = a.ok("POST", &format!("/v1/code/changes/{pr_change}/land"), None).await;
    assert_eq!(landed["status"], "landed");
    until("PR closed", || async {
        let calls = g.mock.calls.lock();
        calls.iter().any(|(m, p, _)| m == "POST" && p == "/repos/acme/app/issues/3/comments")
            && calls.iter().any(|(m, p, b)| m == "PATCH" && p == "/repos/acme/app/pulls/3" && b["state"] == "closed")
    })
    .await;
    let gh_head = || async { g.gh.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].clone() };
    let roll_head = || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].clone() };
    until("first mirror push", || async { gh_head().await == roll_head().await }).await;

    // 200 landings → a handful of fast-forward pushes, and GitHub ends at roll's head.
    let pushes_before = g.mock.receive_packs.load(Ordering::SeqCst);
    let base = roll_head().await.as_str().unwrap().to_string();
    let lands = (0..200).map(|i| {
        let router = a.router.clone();
        let base = base.clone();
        tokio::spawn(async move {
            use tower::ServiceExt;
            let body = json!({ "base": base, "message": format!("agent {i}"), "files": { format!("agents/{i}.txt"): { "content": format!("{i}\n") } } });
            let req = axum::http::Request::post("/v1/code/repos/acme/app/land")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        })
    });
    for t in lands.collect::<Vec<_>>() {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    until("mirror catches up", || async { gh_head().await == roll_head().await }).await;
    let pushes = g.mock.receive_packs.load(Ordering::SeqCst) - pushes_before;
    assert!((1..=10).contains(&pushes), "200 landings took {pushes} pushes");
    let repo = a.ok("GET", "/v1/code/repos/acme/app", None).await;
    until("mirror status ok", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["mirror"]["state"] == "ok" }).await;
    assert!(repo["github"]["mirror"]["last_push_at"].is_number(), "{}", repo["github"]);
    let file = g.gh.raw("GET", "/v1/code/repos/acme/app/blob/main/agents/199.txt", None, None).await;
    assert_eq!(file.1, b"199\n");

    // Finishing the issue's work item in roll closes the issue on GitHub.
    let w1 = a.ok("GET", "/v1/code/repos/acme/app/work", None).await;
    let w1 = w1.as_array().unwrap().iter().find(|w| w["source"] == "github#1").unwrap()["id"].as_str().unwrap().to_string();
    a.ok("POST", &format!("/v1/code/work/{w1}/complete"), None).await;
    until("issue closed", || async {
        g.mock.calls.lock().iter().any(|(m, p, b)| m == "PATCH" && p == "/repos/acme/app/issues/1" && b["state_reason"] == "completed")
    })
    .await;
    // Issue #4 was closed on GitHub, so roll never wrote to it.
    assert!(!g.mock.calls.lock().iter().any(|(_, p, _)| p.contains("/issues/4")));

    // GitHub refusing pushes: the mirror reports the error, backs off, and recovers once it can push.
    g.mock.fail_pushes.store(true, Ordering::SeqCst);
    a.ok("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "while github is down", "files": { "down.txt": { "content": "d\n" } } }))).await;
    until("mirror error", || async {
        let m = a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["mirror"].clone();
        m["state"] == "error" && m["error"].is_string() && m["behind"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    g.mock.fail_pushes.store(false, Ordering::SeqCst);
    until("mirror recovered", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["mirror"]["state"] == "ok" }).await;
    assert_eq!(g.gh.raw("GET", "/v1/code/repos/acme/app/blob/main/down.txt", None, None).await.1, b"d\n");

    // Deleting the roll repo stops its mirror: later landings elsewhere push nothing for it.
    let pushes = g.mock.receive_packs.load(Ordering::SeqCst);
    a.ok("DELETE", "/v1/code/repos/acme/app", None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(g.mock.receive_packs.load(Ordering::SeqCst), pushes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_to_github_main_is_ingested_and_merged() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let a = app(None);
    // The mirror stays quiet, so roll and GitHub can move apart.
    let slow = MirrorConfig { debounce: Duration::from_secs(600), max_delay: Duration::from_secs(600), ..fast_mirror() };
    enable(&a, &g, slow);
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    let repo = a
        .ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } })))
        .await;
    let repo_id = repo["id"].as_str().unwrap().to_string();
    until("import", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["import"]["state"] == "done" }).await;

    let push_event = |before: &str, after: &str| {
        json!({
            "ref": "refs/heads/main", "before": before, "after": after,
            "repository": repo_json(&g.mock), "sender": { "login": "human", "id": 5 }, "installation": { "id": INSTALLATION },
        })
    };

    // A human pushes to GitHub's main; roll has nothing newer: fast-forward.
    let before = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    write(&w, "docs/one.md", "one\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "docs on github"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let after = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(hook(&a, "push", "p-1", &push_event(&before, &after), SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"], after);

    // roll lands something; GitHub moves too. roll merges GitHub's commit in.
    a.ok("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "roll side", "files": { "roll.txt": { "content": "roll\n" } } }))).await;
    let roll_head = a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].as_str().unwrap().to_string();
    write(&w, "docs/two.md", "two\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "more docs on github"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let after2 = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(hook(&a, "push", "p-2", &push_event(&after, &after2), SECRET).await, StatusCode::OK);
    let head = a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].as_str().unwrap().to_string();
    let cs = a.code.store.resolve(&repo_id, &head).unwrap().unwrap();
    let raw = a.code.store.git_object(&repo_id, &code_store::Oid(code_git::ObjectId::from_hex(head.as_bytes()).unwrap().0)).await.unwrap().unwrap().1;
    let parents: Vec<String> = code_git::parse_commit(&raw).unwrap().parents.iter().map(|p| p.to_hex()).collect();
    assert_eq!(parents, [roll_head.clone(), after2.clone()], "first parent stays roll's mainline");
    let files: Vec<String> = a.code.store.list_tree(&repo_id, &cs, "docs").unwrap().unwrap().iter().map(|e| e.name.clone()).collect();
    assert_eq!(files, ["one.md", "two.md"]);
    assert!(a.code.store.read_file(&repo_id, &cs, "roll.txt").await.unwrap().is_some());

    // Both change the same file: the mirror pauses with the paths.
    a.ok("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "roll edits readme", "files": { "README.md": { "content": "# app (roll)\n" } } }))).await;
    // GitHub's main now has roll's merge? No: the mirror is quiet, so GitHub is still at after2.
    write(&w, "README.md", "# app (github)\n");
    git(home, &w, &["commit", "-qam", "github edits readme"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let after3 = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    assert_eq!(hook(&a, "push", "p-3", &push_event(&after2, &after3), SECRET).await, StatusCode::OK);
    until("mirror paused", || async {
        let m = a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["mirror"].clone();
        m["state"] == "paused" && m["error"].as_str().is_some_and(|e| e.contains("README.md"))
    })
    .await;
}

/// Reviews both ways: GitHub reviews and inline comments (CodeRabbit, Greptile, people) become the
/// change's reviews and threads; reviews and comments made in roll go to the pull request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviews_both_ways() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = app(None);
    enable(&a, &g, fast_mirror());
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } }))).await;
    until("PR import", || async { imported(&a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == 3).unwrap().clone();
    let id = c3["id"].as_str().unwrap().to_string();
    let repo = repo_json(&g.mock);
    let inst = json!({ "id": INSTALLATION });
    let pr = pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha);
    let bot = |login: &str| json!({ "login": login, "id": 42, "type": "Bot" });
    let reviews = |a: &App| {
        let a = a.router.clone();
        let id = id.clone();
        async move {
            use tower::ServiceExt;
            let r = a.oneshot(axum::http::Request::get(format!("/v1/code/changes/{id}/reviews")).body(axum::body::Body::empty()).unwrap()).await.unwrap();
            serde_json::from_slice::<Value>(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap()
        }
    };

    // CodeRabbit requests changes on GitHub: a review in roll from the app, blocking the change.
    let review = |action: &str, rid: u64, user: Value, state: &str| {
        json!({ "action": action, "review": { "id": rid, "user": user, "body": "Walkthrough: adds feature.txt", "state": state, "commit_id": feature_sha, "html_url": format!("https://github.com/acme/app/pull/3#pullrequestreview-{rid}") }, "pull_request": pr, "repository": repo, "installation": inst, "sender": user })
    };
    assert_eq!(hook(&a, "pull_request_review", "r-1", &review("submitted", 501, bot("coderabbitai[bot]"), "changes_requested"), SECRET).await, StatusCode::OK);
    let rs = reviews(&a).await;
    assert_eq!(rs.as_array().unwrap().len(), 1, "{rs}");
    assert_eq!(rs[0]["reviewer"], json!({ "kind": "app", "name": "coderabbitai[bot]", "app": null }));
    assert_eq!((rs[0]["verdict"].as_str(), rs[0]["source"].as_str(), rs[0]["stale"].as_bool()), (Some("request_changes"), Some("github"), Some(false)));
    assert_eq!(rs[0]["url"], "https://github.com/acme/app/pull/3#pullrequestreview-501");
    let c = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    assert_eq!(c["blockers"][0]["kind"], "changes_requested");
    // Redelivered under a new delivery id: still one review. Edited: the body changes.
    assert_eq!(hook(&a, "pull_request_review", "r-2", &review("submitted", 501, bot("coderabbitai[bot]"), "changes_requested"), SECRET).await, StatusCode::OK);
    assert_eq!(reviews(&a).await.as_array().unwrap().len(), 1);
    let mut edited = review("edited", 501, bot("coderabbitai[bot]"), "changes_requested");
    edited["review"]["body"] = json!("Walkthrough v2");
    hook(&a, "pull_request_review", "r-3", &edited, SECRET).await;
    assert_eq!(reviews(&a).await[0]["body"], "Walkthrough v2");
    // A pending review isn't one yet; roll's own echoes are ignored.
    hook(&a, "pull_request_review", "r-4", &review("submitted", 502, json!({ "login": "octo", "id": 9, "type": "User" }), "pending"), SECRET).await;
    hook(&a, "pull_request_review", "r-5", &review("submitted", 503, bot("roll-test[bot]"), "approved"), SECRET).await;
    assert_eq!(reviews(&a).await.as_array().unwrap().len(), 1);

    // Greptile comments inline (with a suggestion); someone replies, edits, deletes.
    let comment = |action: &str, cid: u64, reply_to: Option<u64>, user: Value, body: &str| {
        json!({ "action": action, "comment": { "id": cid, "pull_request_review_id": 600, "in_reply_to_id": reply_to, "path": "feature.txt", "line": 1, "side": "RIGHT", "commit_id": feature_sha, "body": body, "user": user }, "pull_request": pr, "repository": repo, "installation": inst, "sender": user })
    };
    hook(&a, "pull_request_review_comment", "c-1", &comment("created", 700, None, bot("greptile-apps[bot]"), "Name it better\n```suggestion\nfeature!\n```"), SECRET).await;
    let threads = a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await;
    assert_eq!(threads.as_array().unwrap().len(), 1, "{threads}");
    let th = threads[0].clone();
    assert_eq!((th["path"].as_str(), th["line"].as_u64(), th["source"].as_str()), (Some("feature.txt"), Some(1), Some("github")));
    assert_eq!(th["review"], "github:600", "its review hasn't arrived yet");
    assert_eq!(th["comments"][0]["author"]["name"], "greptile-apps[bot]");
    assert_eq!(th["comments"][0]["suggestion"], "feature!");
    // The review it belongs to arrives: the thread is linked to it.
    hook(&a, "pull_request_review", "r-6", &review("submitted", 600, bot("greptile-apps[bot]"), "commented"), SECRET).await;
    let th = a.ok("GET", &format!("/v1/code/threads/{}", th["id"].as_str().unwrap()), None).await;
    assert!(th["review"].as_str().unwrap().starts_with("rv_"), "{th}");
    let human = json!({ "login": "octo", "id": 9, "type": "User" });
    hook(&a, "pull_request_review_comment", "c-2", &comment("created", 701, Some(700), human.clone(), "agreed"), SECRET).await;
    hook(&a, "pull_request_review_comment", "c-2b", &comment("created", 701, Some(700), human.clone(), "agreed"), SECRET).await;
    let th = a.ok("GET", &format!("/v1/code/threads/{}", th["id"].as_str().unwrap()), None).await;
    assert_eq!(th["comments"].as_array().unwrap().len(), 2, "a reply goes in its thread, once");
    assert_eq!(th["comments"][1]["author"], json!({ "kind": "user", "name": "octo", "app": null }));
    hook(&a, "pull_request_review_comment", "c-3", &comment("edited", 701, Some(700), human.clone(), "agreed!"), SECRET).await;
    hook(&a, "pull_request_review_comment", "c-4", &comment("deleted", 700, None, bot("greptile-apps[bot]"), ""), SECRET).await;
    let th = a.ok("GET", &format!("/v1/code/threads/{}", th["id"].as_str().unwrap()), None).await;
    assert_eq!(th["comments"].as_array().unwrap().len(), 1);
    assert_eq!(th["comments"][0]["body"], "agreed!");
    // Resolved on GitHub: resolved here.
    let resolved = json!({ "action": "resolved", "thread": { "node_id": "PRRT_1", "comments": [{ "id": 700, "path": "feature.txt", "body": "x" }] }, "pull_request": pr, "repository": repo, "installation": inst, "sender": human });
    hook(&a, "pull_request_review_thread", "t-1", &resolved, SECRET).await;
    let th = a.ok("GET", &format!("/v1/code/threads/{}", th["id"].as_str().unwrap()), None).await;
    assert_eq!((th["resolved"].as_bool(), th["resolved_by"].as_str()), (Some(true), Some("@octo")));
    // Dismissed on GitHub: the change isn't blocked anymore.
    hook(&a, "pull_request_review", "r-7", &review("dismissed", 501, bot("coderabbitai[bot]"), "dismissed"), SECRET).await;
    let c = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    assert!(c["blockers"].as_array().unwrap().iter().all(|b| b["kind"] != "changes_requested"), "{c}");

    // A review in roll, with an inline comment, goes to the pull request as one review.
    let mine = a.ok("POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": "approve", "body": "Ship it", "comments": [{ "path": "feature.txt", "line": 1, "body": "nice" }] }))).await;
    until("review posted", || async { g.mock.calls.lock().iter().any(|(_, p, _)| p.ends_with("/pulls/3/reviews")) }).await;
    let posted = g.mock.calls.lock().iter().find(|(_, p, _)| p.ends_with("/pulls/3/reviews")).unwrap().2.clone();
    assert_eq!(posted["event"], "APPROVE");
    assert_eq!(posted["commit_id"], feature_sha.as_str());
    assert!(posted["body"].as_str().unwrap().starts_with("Ship it"), "{posted}");
    assert_eq!(posted["comments"][0]["path"], "feature.txt");
    assert_eq!(posted["comments"][0]["side"], "RIGHT");
    until("review keyed", || async { reviews(&a).await.as_array().unwrap().iter().any(|r| r["id"] == mine["id"] && r["url"].is_string()) }).await;
    // A reply on GitHub to the comment roll posted lands in roll's thread.
    let my_thread = mine["threads"][0]["id"].as_str().unwrap().to_string();
    let gh_comment = loop {
        let c = g.mock.review_comments.lock().values().flatten().next().cloned();
        if let Some(c) = c {
            break c["id"].as_u64().unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    until("thread keyed", || async {
        hook(&a, "pull_request_review_comment", &format!("c-k-{}", rand::random::<u32>()), &comment("created", 7000 + rand::random::<u16>() as u64, Some(gh_comment), human.clone(), "thanks"), SECRET).await;
        a.ok("GET", &format!("/v1/code/threads/{my_thread}"), None).await["comments"].as_array().unwrap().iter().any(|c| c["body"] == "thanks")
    })
    .await;
    // Nothing echoed: the review webhook for roll's own review is ignored.
    let n = reviews(&a).await.as_array().unwrap().len();
    hook(&a, "pull_request_review", "r-8", &review("submitted", 9_000, bot("roll-test[bot]"), "approved"), SECRET).await;
    assert_eq!(reviews(&a).await.as_array().unwrap().len(), n);

    // A thread opened in roll on its own becomes a PR comment; a reply in roll, a reply there.
    let t = a.ok("POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "feature.txt", "line": 1, "body": "one more thing" }))).await;
    until("thread posted", || async { g.mock.calls.lock().iter().any(|(_, p, b)| p.ends_with("/pulls/3/comments") && b["body"].as_str().unwrap_or("").starts_with("one more thing")) }).await;
    a.ok("POST", &format!("/v1/code/threads/{}/comments", t["id"].as_str().unwrap()), Some(json!({ "body": "and another" }))).await;
    until("reply posted", || async { g.mock.calls.lock().iter().any(|(_, p, b)| p.contains("/replies") && b["body"].as_str().unwrap_or("").starts_with("and another")) }).await;
    let posted_once = g.mock.calls.lock().iter().filter(|(_, p, b)| p.ends_with("/pulls/3/comments") && b["body"].as_str().unwrap_or("").starts_with("one more thing")).count();
    assert_eq!(posted_once, 1, "each comment is posted once");

    // Resolving a thread in roll resolves it on GitHub (GraphQL; the node id looked up by the
    // thread's first comment), once. Unresolving unresolves it.
    let gql = |what: &str| g.mock.calls.lock().iter().filter(|(m, p, _)| m == "GRAPHQL" && p == what).map(|(_, _, b)| b.clone()).collect::<Vec<Value>>();
    let tid = t["id"].as_str().unwrap().to_string();
    a.ok("POST", &format!("/v1/code/threads/{tid}/resolve"), Some(json!({}))).await;
    until("thread resolved on GitHub", || async { !gql("resolve").is_empty() }).await;
    let root = g.mock.calls.lock().iter().find(|(_, p, b)| p.ends_with("/pulls/3/comments") && b["body"].as_str().unwrap_or("").starts_with("one more thing")).map(|_| ()).is_some();
    assert!(root);
    let resolved_node = gql("resolve")[0].as_str().unwrap().to_string();
    assert!(resolved_node.starts_with("PRRT_8"), "the thread roll started: {resolved_node}");
    assert_eq!(gql("reviewThreads")[0]["number"], 3);
    // Another thread event for it (a reply in roll): nothing sent again.
    a.ok("POST", &format!("/v1/code/threads/{tid}/comments"), Some(json!({ "body": "done" }))).await;
    until("reply posted", || async { g.mock.calls.lock().iter().any(|(_, p, b)| p.contains("/replies") && b["body"].as_str().unwrap_or("").starts_with("done")) }).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(gql("resolve").len(), 1, "resolved once");
    a.ok("POST", &format!("/v1/code/threads/{tid}/unresolve"), Some(json!({}))).await;
    until("thread unresolved on GitHub", || async { gql("unresolve").len() == 1 }).await;
    assert_eq!(gql("unresolve")[0], json!(resolved_node));
    assert_eq!(gql("reviewThreads").len(), 1, "the node id is remembered");
    // The thread GitHub resolved (its node id came with the webhook) isn't echoed back; when
    // roll unresolves it, the webhook's node id is used without a lookup.
    let gh_thread = a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await.as_array().unwrap().iter().find(|t| t["source"] == "github").unwrap().clone();
    assert_eq!(gh_thread["resolved"], true);
    assert!(gql("resolve").iter().all(|n| n != "PRRT_1"), "GitHub's own resolution isn't sent back");
    a.ok("POST", &format!("/v1/code/threads/{}/unresolve", gh_thread["id"].as_str().unwrap()), Some(json!({}))).await;
    until("GitHub's thread unresolved", || async { gql("unresolve").iter().any(|n| n == "PRRT_1") }).await;
    assert_eq!(gql("reviewThreads").len(), 1, "no lookup for a thread whose node id came by webhook");

    // GitHub refusing an App's approval: posted as a comment review instead.
    g.mock.reject_verdicts.store(true, Ordering::SeqCst);
    a.ok("POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": "request_changes", "body": "wait" }))).await;
    until("fallback review", || async { g.mock.calls.lock().iter().any(|(_, p, b)| p.ends_with("/pulls/3/reviews") && b["event"] == "COMMENT" && b["body"].as_str().unwrap_or("").contains("Changes requested")) }).await;

    // Anyone can approve a public PR on GitHub; only people who can push decide in roll. An
    // outsider's approval (or a missing association) is kept as a comment; a collaborator's counts.
    let with_assoc = |rid: u64, login: &str, assoc: Option<&str>, state: &str| {
        let mut v = review("submitted", rid, json!({ "login": login, "id": rid, "type": "User" }), state);
        if let Some(x) = assoc {
            v["review"]["author_association"] = json!(x);
        }
        v
    };
    for (i, (rid, login, assoc, state)) in [
        (9_101, "drive-by", Some("NONE"), "approved"),
        (9_102, "contrib", Some("CONTRIBUTOR"), "changes_requested"),
        (9_103, "unknown", None, "approved"),
        (9_104, "maintainer", Some("COLLABORATOR"), "approved"),
        (9_105, "org-member", Some("MEMBER"), "changes_requested"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(hook(&a, "pull_request_review", &format!("assoc-{i}"), &with_assoc(rid, login, assoc, state), SECRET).await, StatusCode::OK);
    }
    let rs = reviews(&a).await;
    let verdict_of = |login: &str| rs.as_array().unwrap().iter().find(|r| r["reviewer"]["name"] == login).map(|r| r["verdict"].as_str().unwrap().to_string());
    assert_eq!(verdict_of("drive-by").as_deref(), Some("comment"));
    assert_eq!(verdict_of("contrib").as_deref(), Some("comment"));
    assert_eq!(verdict_of("unknown").as_deref(), Some("comment"));
    assert_eq!(verdict_of("maintainer").as_deref(), Some("approve"));
    assert_eq!(verdict_of("org-member").as_deref(), Some("request_changes"));
}

/// A mock Anthropic API: every review finds one bug on feature.txt's first line.
async fn anthropic_mock() -> String {
    let app = Router::new().fallback(|uri: axum::http::Uri, body: axum::body::Bytes| async move {
        if uri.path().ends_with("/count_tokens") {
            return Json(json!({ "input_tokens": 100 })).into_response();
        }
        let _ = body;
        let review = json!({
            "walkthrough": "Adds feature.txt.", "files": [{ "path": "feature.txt", "summary": "new file" }], "diagram": null, "notes": [],
            "findings": [{ "path": "feature.txt", "side": "new", "start_line": 1, "line": 1, "severity": "bug", "title": "Typo",
                           "body": "Should this say features?", "suggestion": "features", "confidence": 0.9 }]
        });
        Json(json!({ "model": "claude-opus-5-5", "stop_reason": "end_turn", "content": [{ "type": "text", "text": review.to_string() }], "usage": { "input_tokens": 10, "output_tokens": 5 } }))
            .into_response()
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_reviewer_posts_to_the_pull_request() {
    let g = github_mock().await;
    let ai = anthropic_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = common::app_with(|c| {
        c.review.hosted = Some(code_control::ai::Hosted { provider: "anthropic".into(), api_key: "sk-test".into(), base_url: Some(ai.clone()), model: None, secondary_model: None });
    });
    a.code.start_reviewer();
    enable(&a, &g, fast_mirror());
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } }))).await;
    until("PR import", || async { imported(&a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == 3).unwrap().clone();
    let id = c3["id"].as_str().unwrap().to_string();
    a.ok("PATCH", "/v1/code/repos/acme/app/reviewer", Some(json!({ "enabled": true, "mode": "standard" }))).await;
    a.ok("POST", &format!("/v1/code/changes/{id}/ai-review"), Some(json!({}))).await;
    let posted = |g: &GitHub| g.mock.calls.lock().iter().filter(|(_, p, b)| p.ends_with("/pulls/3/reviews") && b["body"].as_str().unwrap_or("").contains("roll review")).map(|x| x.2.clone()).collect::<Vec<_>>();
    until("roll's review posted to the PR", || async { !posted(&g).is_empty() }).await;
    let p = posted(&g)[0].clone();
    assert_eq!(p["event"], "REQUEST_CHANGES");
    assert_eq!(p["commit_id"], feature_sha.as_str());
    assert_eq!(p["comments"][0]["path"], "feature.txt");
    assert!(p["comments"][0]["body"].as_str().unwrap().contains("```suggestion\nfeatures\n```"));
    // The second `review` event (its `ai` record) doesn't post it again.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(posted(&g).len(), 1);
}

/// `@roll` on GitHub: in a pull request's conversation and in inline review comments, for people
/// GitHub says can write; answers go back to the pull request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn roll_commands_from_github_comments() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    g.mock.perms.lock().insert("octo".into(), "write".into());
    g.mock.perms.lock().insert("viewer".into(), "read".into());
    let a = app(None);
    enable(&a, &g, fast_mirror());
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } }))).await;
    until("PR import", || async { imported(&a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == 3).unwrap().clone();
    let id = c3["id"].as_str().unwrap().to_string();
    let repo = repo_json(&g.mock);
    let inst = json!({ "id": INSTALLATION });
    let pr = pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha);
    let user = |login: &str| json!({ "login": login, "id": 9, "type": if login.ends_with("[bot]") { "Bot" } else { "User" } });
    let mut issue = issue_json(3, "PR 3", "open", &[]);
    issue["pull_request"] = json!({ "url": "https://api.github.com/repos/acme/app/pulls/3" });
    let convo = |action: &str, cid: u64, login: &str, body: &str, issue: &Value| {
        json!({ "action": action, "issue": issue, "comment": { "id": cid, "body": body, "user": user(login), "html_url": "x" }, "repository": repo, "installation": inst, "sender": user(login) })
    };
    let pr_comments = |m: &Mock| m.calls.lock().iter().filter(|(_, p, _)| p == "/repos/acme/app/issues/3/comments").map(|(_, _, b)| b["body"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();

    // A writer pauses roll's reviews from the conversation: done, and answered there.
    assert_eq!(hook(&a, "issue_comment", "ic-1", &convo("created", 900, "octo", "@roll pause", &issue), SECRET).await, StatusCode::OK);
    until("the pause answer", || async { pr_comments(&g.mock).iter().any(|b| b.starts_with("Paused")) }).await;
    // Readers, strangers, edits, roll's own comments and plain issues: nothing happens.
    let before = pr_comments(&g.mock).len();
    hook(&a, "issue_comment", "ic-2", &convo("created", 901, "viewer", "@roll resume", &issue), SECRET).await;
    hook(&a, "issue_comment", "ic-3", &convo("created", 902, "stranger", "@roll resume", &issue), SECRET).await;
    hook(&a, "issue_comment", "ic-4", &convo("edited", 903, "octo", "@roll resume", &issue), SECRET).await;
    hook(&a, "issue_comment", "ic-5", &convo("created", 904, "roll-test[bot]", "@roll resume", &issue), SECRET).await;
    hook(&a, "issue_comment", "ic-6", &convo("created", 905, "octo", "@roll resume", &issue_json(5, "an issue", "open", &[])), SECRET).await;
    hook(&a, "issue_comment", "ic-7", &convo("created", 906, "octo", "no command here", &issue), SECRET).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(pr_comments(&g.mock).len(), before, "{:?}", pr_comments(&g.mock));
    // Line-level commands need a line: the answer says so.
    hook(&a, "issue_comment", "ic-8", &convo("created", 907, "octo", "@roll explain", &issue), SECRET).await;
    until("the explain answer", || async { pr_comments(&g.mock).iter().any(|b| b.contains("inline comment")) }).await;
    // Learnings from the conversation.
    hook(&a, "issue_comment", "ic-9", &convo("created", 908, "octo", "@roll don't flag TODO comments", &issue), SECRET).await;
    until("the learning", || async { a.ok("GET", "/v1/code/repos/acme/app/learnings", None).await.as_array().unwrap().iter().any(|l| l["text"].as_str().unwrap_or("").contains("TODO")) }).await;

    // In an inline review comment: answered in the thread (and so on GitHub, as a reply).
    let inline = |cid: u64, login: &str, body: &str| {
        json!({ "action": "created", "comment": { "id": cid, "pull_request_review_id": 610, "in_reply_to_id": null, "path": "feature.txt", "line": 1, "side": "RIGHT", "commit_id": feature_sha, "body": body, "user": user(login) }, "pull_request": pr, "repository": repo, "installation": inst, "sender": user(login) })
    };
    hook(&a, "pull_request_review_comment", "rc-1", &inline(910, "octo", "@roll resolve"), SECRET).await;
    let roll_reply = || async {
        let ts = a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await;
        ts.as_array().unwrap().iter().find(|t| t["comments"][0]["body"] == "@roll resolve").map(|t| t["comments"].as_array().unwrap().iter().any(|c| c["author"]["kind"] == "roll"))
    };
    until("roll's reply in the thread", || async { roll_reply().await == Some(true) }).await;
    until("the reply on GitHub", || async { g.mock.calls.lock().iter().any(|(_, p, _)| p == "/repos/acme/app/pulls/3/comments/910/replies") }).await;
    // A reader's inline command: the comment is kept, roll stays quiet.
    hook(&a, "pull_request_review_comment", "rc-2", &inline(911, "viewer", "@roll review"), SECRET).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ts = a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await;
    let th = ts.as_array().unwrap().iter().find(|t| t["comments"][0]["body"] == "@roll review").unwrap();
    assert_eq!(th["comments"].as_array().unwrap().len(), 1);
}

/// Every review event and action GitHub sends, against the rules: what counts, what's stale,
/// what's ignored (docs/code/reviews.md, "External reviewers").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn review_events_every_action_against_the_rules() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = app(None);
    enable(&a, &g, fast_mirror());
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } }))).await;
    until("PR import", || async { imported(&a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == 3).unwrap().clone();
    let id = c3["id"].as_str().unwrap().to_string();
    let (repo, inst, pr) = (repo_json(&g.mock), json!({ "id": INSTALLATION }), pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let user = |login: &str| json!({ "login": login, "id": 11, "type": "User" });
    let bot = |login: &str| json!({ "login": login, "id": 42, "type": "Bot" });
    let review = |action: &str, rid: u64, who: Value, state: &str, assoc: Option<&str>, commit: &str| {
        let mut v = json!({ "action": action, "review": { "id": rid, "user": who, "body": format!("review {rid}"), "state": state, "commit_id": commit }, "pull_request": pr, "repository": repo, "installation": inst, "sender": who });
        if let Some(x) = assoc {
            v["review"]["author_association"] = json!(x);
        }
        v
    };
    let comment = |action: &str, cid: u64, extra: Value| {
        let mut c = json!({ "id": cid, "path": "feature.txt", "line": 1, "side": "RIGHT", "commit_id": feature_sha, "body": format!("comment {cid}"), "user": user("octo") });
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        json!({ "action": action, "comment": c, "pull_request": pr, "repository": repo, "installation": inst, "sender": user("octo") })
    };
    let thread = |action: &str, root: u64, sender: Value| {
        json!({ "action": action, "thread": { "node_id": format!("PRRT_{root}"), "comments": [{ "id": root, "path": "feature.txt", "body": "x" }] }, "pull_request": pr, "repository": repo, "installation": inst, "sender": sender })
    };
    let kinds = || {
        let (a, id) = (&a, id.clone());
        async move {
            let c = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
            c["blockers"].as_array().unwrap().iter().map(|b| b["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    let reviews = || {
        let (a, id) = (&a, id.clone());
        async move { a.ok("GET", &format!("/v1/code/changes/{id}/reviews"), None).await.as_array().unwrap().clone() }
    };
    let by_ext = |rs: &[Value], rid: u64| rs.iter().find(|r| r["url"].is_null() && r["body"] == format!("review {rid}")).cloned();
    let mut n = 0;
    let mut send = |event: &'static str, body: Value| {
        n += 1;
        let (a, delivery) = (&a, format!("e-{n}"));
        async move { assert_eq!(hook(a, event, &delivery, &body, SECRET).await, StatusCode::OK, "{event}: {body}") }
    };
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "required_approvals": 1 }))).await;
    a.ok("POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "@lead", "required": true }))).await;
    assert_eq!(kinds().await, vec!["needs_approval", "review_pending"]);

    // The pull request's author (octo) approving it doesn't count, owner or not.
    send("pull_request_review", review("submitted", 800, user("octo"), "approved", Some("OWNER"), &feature_sha)).await;
    assert_eq!(by_ext(&reviews().await, 800).unwrap()["verdict"], "approve");
    assert_eq!(kinds().await, vec!["needs_approval", "review_pending"]);
    // pull_request_review.submitted from the repo's owner (any case) on the head: counts, and
    // settles the review requested from them.
    send("pull_request_review", review("submitted", 801, user("lead"), "approved", Some("owner"), &feature_sha)).await;
    let rs = reviews().await;
    let v = by_ext(&rs, 801).unwrap();
    assert_eq!((v["verdict"].as_str(), v["reviewer"]["kind"].as_str(), v["stale"].as_bool()), (Some("approve"), Some("user"), Some(false)));
    assert_eq!(kinds().await, Vec::<String>::new());
    // .dismissed: no longer counts, dismissed by the sender; a redelivery changes nothing.
    send("pull_request_review", review("dismissed", 801, user("lead"), "dismissed", Some("OWNER"), &feature_sha)).await;
    send("pull_request_review", review("dismissed", 801, user("lead"), "dismissed", Some("OWNER"), &feature_sha)).await;
    let v = by_ext(&reviews().await, 801).unwrap();
    assert_eq!((v["dismissed"].as_bool(), v["dismissed_by"].as_str(), v["dismiss_reason"].as_str()), (Some(true), Some("@lead"), Some("dismissed on GitHub")));
    assert_eq!(kinds().await, vec!["needs_approval"]);
    // .dismissed for a review roll never saw: nothing.
    send("pull_request_review", review("dismissed", 802, user("octo"), "dismissed", Some("OWNER"), &feature_sha)).await;
    assert!(by_ext(&reviews().await, 802).is_none());
    // .edited for a review roll never saw (submitted while it wasn't listening): recorded.
    send("pull_request_review", review("edited", 803, user("late"), "approved", Some("MEMBER"), &feature_sha)).await;
    assert_eq!(by_ext(&reviews().await, 803).unwrap()["verdict"], "approve");
    assert!(kinds().await.is_empty());
    send("pull_request_review", review("dismissed", 803, user("late"), "dismissed", Some("MEMBER"), &feature_sha)).await;

    // A bot's approval is kept as one whatever its association, and counts only with
    // count_ai_approval; a bot's change request blocks.
    send("pull_request_review", review("submitted", 804, bot("greptile-apps[bot]"), "approved", Some("NONE"), &feature_sha)).await;
    assert_eq!(by_ext(&reviews().await, 804).unwrap()["verdict"], "approve");
    assert_eq!(kinds().await, vec!["needs_approval"]);
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "count_ai_approval": true }))).await;
    assert!(kinds().await.is_empty());
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "count_ai_approval": false }))).await;
    send("pull_request_review", review("submitted", 805, bot("coderabbitai[bot]"), "changes_requested", None, &feature_sha)).await;
    assert_eq!(kinds().await, vec!["needs_approval", "changes_requested"]);
    send("pull_request_review", review("dismissed", 805, bot("coderabbitai[bot]"), "dismissed", None, &feature_sha)).await;

    // Reviews of a commit that isn't the change's head are stale; dismiss_stale decides.
    let unknown = "ab".repeat(20);
    send("pull_request_review", review("submitted", 806, user("maint"), "approved", Some("COLLABORATOR"), &main_sha)).await;
    send("pull_request_review", review("submitted", 807, user("maint2"), "approved", Some("COLLABORATOR"), &unknown)).await;
    let rs = reviews().await;
    for rid in [806, 807] {
        let v = by_ext(&rs, rid).unwrap();
        assert_eq!((v["revision"].as_u64(), v["stale"].as_bool()), (Some(0), Some(true)), "{v}");
    }
    assert_eq!(kinds().await, vec!["needs_approval"]);
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "dismiss_stale": false }))).await;
    assert!(kinds().await.is_empty());
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "dismiss_stale": true, "required_approvals": 0 }))).await;

    // A review on a pull request roll has no change for: ignored.
    let before = reviews().await.len();
    let mut other = review("submitted", 808, user("lead"), "approved", Some("OWNER"), &feature_sha);
    other["pull_request"] = pr_json(&g.mock, 99, "nope", &feature_sha, &main_sha);
    send("pull_request_review", other).await;
    assert_eq!(reviews().await.len(), before);

    // pull_request_review_comment.created: on the base side (LEFT) → old; without a line (already
    // outdated on GitHub) → outdated; .edited and .deleted of comments roll never saw do nothing.
    send("pull_request_review_comment", comment("created", 900, json!({ "side": "LEFT", "path": "README.md" }))).await;
    send("pull_request_review_comment", comment("created", 901, json!({ "line": null, "original_line": 1 }))).await;
    send("pull_request_review_comment", comment("edited", 999, json!({}))).await;
    send("pull_request_review_comment", comment("deleted", 998, json!({}))).await;
    let ts = a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await;
    let ts = ts.as_array().unwrap();
    assert_eq!(ts.len(), 2, "{ts:?}");
    let t900 = ts.iter().find(|t| t["comments"][0]["body"] == "comment 900").unwrap();
    let t901 = ts.iter().find(|t| t["comments"][0]["body"] == "comment 901").unwrap();
    assert_eq!((t900["side"].as_str(), t900["path"].as_str(), t900["outdated"].as_bool()), (Some("old"), Some("README.md"), Some(false)));
    assert_eq!((t901["outdated"].as_bool(), t901["line"].as_u64(), t901["review"].clone()), (Some(true), Some(1), Value::Null));
    let thread_of = |tid: Value| {
        let a = &a;
        async move { a.ok("GET", &format!("/v1/code/threads/{}", tid.as_str().unwrap()), None).await }
    };

    // pull_request_review_thread: resolved and unresolved on GitHub follow in roll, and decide
    // require_resolved_threads; roll's own events and unknown threads are ignored.
    a.ok("PATCH", "/v1/code/repos/acme/app/review-settings", Some(json!({ "require_resolved_threads": true }))).await;
    assert_eq!(kinds().await, vec!["unresolved_threads"]);
    send("pull_request_review_thread", thread("resolved", 900, user("octo"))).await;
    send("pull_request_review_thread", thread("resolved", 901, bot("roll-test[bot]"))).await;
    send("pull_request_review_thread", thread("resolved", 12_345, user("octo"))).await;
    assert_eq!(thread_of(t900["id"].clone()).await["resolved"], true);
    assert_eq!(thread_of(t901["id"].clone()).await["resolved"], false, "roll's own resolution events are ignored");
    assert_eq!(kinds().await, vec!["unresolved_threads"]);
    send("pull_request_review_thread", thread("resolved", 901, user("octo"))).await;
    assert!(kinds().await.is_empty());
    send("pull_request_review_thread", thread("unresolved", 900, user("octo"))).await;
    let t = thread_of(t900["id"].clone()).await;
    assert_eq!((t["resolved"].as_bool(), t["resolved_by"].clone()), (Some(false), Value::Null));
    assert_eq!(kinds().await, vec!["unresolved_threads"]);
    send("pull_request_review_thread", thread("unresolved", 12_345, user("octo"))).await;
    // Resolution GitHub reported isn't sent back to it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!g.mock.calls.lock().iter().any(|(m, p, _)| m == "GRAPHQL" && p != "reviewThreads"), "nothing echoed to GitHub");
}

const ZERO: &str = "0000000000000000000000000000000000000000";

/// Connect acme/app (installation 7) and wait for the import.
async fn connect(a: &App) -> String {
    a.ok("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" }))).await;
    let repo = a
        .ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } })))
        .await;
    until("import", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["import"]["state"] == "done" }).await;
    repo["id"].as_str().unwrap().to_string()
}

fn quiet_mirror() -> MirrorConfig {
    MirrorConfig { debounce: Duration::from_secs(600), max_delay: Duration::from_secs(600), ..fast_mirror() }
}

fn rev(home: &std::path::Path, w: &std::path::Path, r: &str) -> String {
    git(home, w, &["rev-parse", r]).trim().to_string()
}

/// A change's head as a git commit id.
fn head_git(a: &App, repo_id: &str, c: &Value) -> String {
    let cs = a.code.store.resolve(repo_id, c["head"].as_str().unwrap()).unwrap().unwrap();
    a.code.store.derive_git(repo_id, &cs).unwrap().0.hex()
}

fn mark(a: &App, key: &str) -> Option<String> {
    a.code.db.sql().query_row("SELECT value FROM github_marks WHERE key = ?", [key], |r| r.get(0)).ok()
}

/// Push webhooks beyond the default branch's fast-forward and merge: a pushed branch opens its
/// change and later pushes revise it (a repeat doesn't), deleting the branch closes it (unless a
/// PR owns it), tags (annotated and lightweight) come and go, roll's own bot echo is ignored, a
/// rewritten default branch pauses the mirror, and pushes to repos roll doesn't know do nothing.
/// The mirror reports `pending` while roll is ahead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_webhooks_for_branches_tags_and_echoes() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &rev(home, &w, "feature"), &rev(home, &w, "main")));
    let a = app(None);
    enable(&a, &g, quiet_mirror());
    let repo_id = connect(&a).await;
    until("PR import", || async { imported(&a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await[0]["id"].as_str().unwrap().to_string();
    let push = |r: &str, before: &str, after: &str, sender: &str| {
        json!({
            "ref": r, "before": before, "after": after, "created": before == ZERO, "deleted": after == ZERO,
            "repository": repo_json(&g.mock), "sender": { "login": sender, "id": 5 }, "installation": { "id": INSTALLATION },
        })
    };
    let mirror = || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["mirror"].clone() };

    // roll lands; the quiet mirror hasn't pushed it: pending, one behind.
    until("mirror ok after the import", || async { mirror().await["state"] == "ok" }).await;
    a.ok("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "roll ahead", "files": { "ahead.txt": { "content": "a\n" } } }))).await;
    until("mirror pending", || async {
        let m = mirror().await;
        m["state"] == "pending" && m["behind"].as_u64().unwrap_or(0) >= 1 && m["error"].is_null()
    })
    .await;

    // A branch pushed to GitHub opens a change from its commit.
    git(home, &w, &["checkout", "-qb", "agent/x"]);
    write(&w, "agent.txt", "one\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "agent work", "-m", "the body"]);
    git(home, &w, &["push", "-q", "origin", "agent/x"]);
    let x1 = rev(home, &w, "agent/x");
    assert_eq!(hook(&a, "push", "b-1", &push("refs/heads/agent/x", ZERO, &x1, "human"), SECRET).await, StatusCode::OK);
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    let cx = changes.as_array().unwrap().iter().find(|c| c["branch"] == "agent/x").cloned().expect("a change for the pushed branch");
    assert_eq!((cx["title"].as_str(), cx["body"].as_str(), cx["author"].as_str()), (Some("agent work"), Some("the body"), Some("human")));
    assert_eq!(head_git(&a, &repo_id, &cx), x1);
    let cx_id = cx["id"].as_str().unwrap().to_string();
    // Another push revises it; the same head again doesn't.
    write(&w, "agent.txt", "two\n");
    git(home, &w, &["commit", "-qam", "agent work v2"]);
    git(home, &w, &["push", "-q", "origin", "agent/x"]);
    let x2 = rev(home, &w, "agent/x");
    assert_eq!(hook(&a, "push", "b-2", &push("refs/heads/agent/x", &x1, &x2, "human"), SECRET).await, StatusCode::OK);
    let cx = a.ok("GET", &format!("/v1/code/changes/{cx_id}"), None).await;
    assert_eq!((cx["revision"].as_u64(), head_git(&a, &repo_id, &cx)), (Some(2), x2.clone()));
    assert_eq!(hook(&a, "push", "b-3", &push("refs/heads/agent/x", &x1, &x2, "human"), SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{cx_id}"), None).await["revision"], 2);
    // Deleting the branch closes its change and drops the ref.
    git(home, &w, &["checkout", "-q", "main"]);
    git(home, &w, &["push", "-q", "origin", ":agent/x"]);
    assert_eq!(hook(&a, "push", "b-4", &push("refs/heads/agent/x", &x2, ZERO, "human"), SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{cx_id}"), None).await["state"], "closed");
    let ref_names = || async {
        a.ok("GET", "/v1/code/repos/acme/app/refs", None).await.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    assert!(!ref_names().await.iter().any(|n| n.ends_with("agent/x")));
    // A branch that a PR owns: the ref goes, the change stays (the PR decides).
    assert_eq!(hook(&a, "push", "b-5", &push("refs/heads/feature", &rev(home, &w, "feature"), ZERO, "human"), SECRET).await, StatusCode::OK);
    assert!(!ref_names().await.iter().any(|n| n == "feature"));
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{c3}"), None).await["state"], "open");

    // Tags: annotated and lightweight, created and deleted.
    git(home, &w, &["tag", "-a", "v2", "-m", "release 2"]);
    git(home, &w, &["tag", "v3"]);
    git(home, &w, &["push", "-q", "origin", "v2", "v3"]);
    let (v2, v3) = (rev(home, &w, "v2"), rev(home, &w, "v3"));
    assert_ne!(v2, v3, "v2 is a tag object");
    assert_eq!(hook(&a, "push", "t-1", &push("refs/tags/v2", ZERO, &v2, "human"), SECRET).await, StatusCode::OK);
    assert_eq!(hook(&a, "push", "t-2", &push("refs/tags/v3", ZERO, &v3, "human"), SECRET).await, StatusCode::OK);
    let names = ref_names().await;
    assert!(names.iter().any(|n| n.ends_with("v2")) && names.iter().any(|n| n.ends_with("v3")), "{names:?}");
    assert_eq!(hook(&a, "push", "t-3", &push("refs/tags/v2", &v2, ZERO, "human"), SECRET).await, StatusCode::OK);
    let names = ref_names().await;
    assert!(!names.iter().any(|n| n.ends_with("v2")) && names.iter().any(|n| n.ends_with("v3")), "{names:?}");

    // roll's own push coming back isn't ingested (here GitHub's main even has a commit roll lacks).
    let roll_head = a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].clone();
    let main_before = rev(home, &w, "main");
    write(&w, "bot.txt", "bot\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "as the bot"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let bot = rev(home, &w, "main");
    assert_eq!(hook(&a, "push", "e-1", &push("refs/heads/main", &main_before, &bot, "roll-test[bot]"), SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"], roll_head);
    assert_eq!(a.raw("GET", "/v1/code/repos/acme/app/blob/main/bot.txt", None, None).await.0, StatusCode::NOT_FOUND);

    // A push to a repository roll doesn't have connected: nothing.
    let mut other = push("refs/heads/main", &main_before, &bot, "human");
    other["repository"]["full_name"] = json!("acme/other");
    other["repository"]["name"] = json!("other");
    assert_eq!(hook(&a, "push", "o-1", &other, SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"], roll_head);

    // GitHub's main rewritten (no common history): the mirror pauses and says why.
    git(home, &w, &["checkout", "-q", "--orphan", "fresh"]);
    git(home, &w, &["rm", "-rqf", "."]);
    write(&w, "fresh.txt", "fresh\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "start over"]);
    // (The mock refuses unrelated history on an existing ref; GitHub only needs to serve it.)
    git(home, &w, &["push", "-q", "origin", "fresh"]);
    let fresh = rev(home, &w, "fresh");
    let mut forced = push("refs/heads/main", &bot, &fresh, "human");
    forced["forced"] = json!(true);
    assert_eq!(hook(&a, "push", "r-1", &forced, SECRET).await, StatusCode::OK);
    until("mirror paused on rewritten history", || async {
        let m = mirror().await;
        m["state"] == "paused" && m["error"].as_str().is_some_and(|e| e.contains("rewritten"))
    })
    .await;
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"], roll_head, "nothing was landed");
}

/// Pull request and issue actions end to end, the queue included: edits, draft and ready, a
/// close or a draft dequeues a queued change, reopen, merged on GitHub (landed, dequeued, not
/// echoed), PRs into other branches ignored, draft PRs; issues unlabeled, issue events about
/// PRs ignored, closed without a reason; installations suspended, unsuspended, repos removed and
/// added, events for other installations and repos; deliveries that fail aren't recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pull_request_issue_and_installation_actions() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let (main_sha, feature_sha) = (rev(home, &w, "main"), rev(home, &w, "feature"));
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = app(None);
    enable(&a, &g, quiet_mirror());
    let a = &a;
    connect(a).await;
    until("PR import", || async { imported(a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await[0]["id"].as_str().unwrap().to_string();
    let repo = repo_json(&g.mock);
    let inst = json!({ "id": INSTALLATION });
    let pr_ev = |action: &str, pr: &Value| json!({ "action": action, "number": pr["number"], "pull_request": pr, "repository": repo, "installation": inst });
    let change = |id: &str| {
        let path = format!("/v1/code/changes/{id}");
        async move { a.ok("GET", &path, None).await }
    };
    let depth = || async { a.ok("GET", "/v1/code/repos/acme/app/queue", None).await["stats"]["depth"].as_u64().unwrap() };
    let enqueue = |id: &str| {
        let id = id.to_string();
        async move { a.req("POST", &format!("/v1/code/changes/{id}/enqueue"), None, None).await }
    };
    // The queue holds what's enqueued (nothing lands while paused).
    a.ok("POST", "/v1/code/repos/acme/app/queue/pause", None).await;

    // edited: title and body follow; no new revision.
    let mut pr3 = pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha);
    pr3["title"] = json!("Renamed on GitHub");
    pr3["body"] = json!("new body");
    assert_eq!(hook(a, "pull_request", "p-1", &pr_ev("edited", &pr3), SECRET).await, StatusCode::OK);
    let c = change(&c3).await;
    assert_eq!((c["title"].as_str(), c["body"].as_str(), c["revision"].as_u64()), (Some("Renamed on GitHub"), Some("new body"), Some(1)));
    // converted_to_draft / ready_for_review.
    pr3["draft"] = json!(true);
    assert_eq!(hook(a, "pull_request", "p-2", &pr_ev("converted_to_draft", &pr3), SECRET).await, StatusCode::OK);
    assert_eq!(change(&c3).await["draft"], true);
    pr3["draft"] = json!(false);
    assert_eq!(hook(a, "pull_request", "p-3", &pr_ev("ready_for_review", &pr3), SECRET).await, StatusCode::OK);
    assert_eq!(change(&c3).await["draft"], false);
    // Back to draft while queued: it leaves the queue.
    assert_eq!(enqueue(&c3).await.0, StatusCode::ACCEPTED);
    assert_eq!(depth().await, 1);
    pr3["draft"] = json!(true);
    assert_eq!(hook(a, "pull_request", "p-4", &pr_ev("converted_to_draft", &pr3), SECRET).await, StatusCode::OK);
    let c = change(&c3).await;
    assert_eq!((c["state"].as_str(), c["draft"].as_bool(), depth().await), (Some("open"), Some(true), 0));
    // Closed on GitHub while queued: closed, and out of the queue.
    pr3["draft"] = json!(false);
    hook(a, "pull_request", "p-5", &pr_ev("ready_for_review", &pr3), SECRET).await;
    assert_eq!(enqueue(&c3).await.0, StatusCode::ACCEPTED);
    pr3["state"] = json!("closed");
    assert_eq!(hook(a, "pull_request", "p-6", &pr_ev("closed", &pr3), SECRET).await, StatusCode::OK);
    let c = change(&c3).await;
    assert_eq!((c["state"].as_str(), depth().await), (Some("closed"), 0), "{c}");
    assert!(c["queue"].is_null(), "{c}");
    // Reopened: open again.
    pr3["state"] = json!("open");
    assert_eq!(hook(a, "pull_request", "p-7", &pr_ev("reopened", &pr3), SECRET).await, StatusCode::OK);
    assert_eq!(change(&c3).await["state"], "open");
    // synchronize: the PR's new head revises its change.
    git(home, &w, &["checkout", "-q", "feature"]);
    write(&w, "feature.txt", "feature v2\n");
    git(home, &w, &["commit", "-qam", "feature v2"]);
    git(home, &w, &["push", "-q", "origin", "feature"]);
    git(home, &w, &["checkout", "-q", "main"]);
    pr3["head"]["sha"] = json!(rev(home, &w, "feature"));
    assert_eq!(hook(a, "pull_request", "p-12", &pr_ev("synchronize", &pr3), SECRET).await, StatusCode::OK);
    until("revision 2", || async { change(&c3).await["revision"] == 2 }).await;

    // Merged on GitHub while queued: landed, dequeued, and nothing said back to GitHub.
    git(home, &w, &["checkout", "-qb", "topic"]);
    write(&w, "topic.txt", "topic\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "topic work"]);
    git(home, &w, &["push", "-q", "origin", "topic", "side"]);
    git(home, &w, &["checkout", "-q", "main"]);
    let mut pr5 = pr_json(&g.mock, 5, "topic", &rev(home, &w, "topic"), &main_sha);
    g.mock.pulls.lock().insert(5, pr5.clone());
    assert_eq!(hook(a, "pull_request", "p-8", &pr_ev("opened", &pr5), SECRET).await, StatusCode::OK);
    let find = |n: u64| async move { a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == n).cloned() };
    let c5 = find(5).await.expect("PR 5's change")["id"].as_str().unwrap().to_string();
    assert_eq!(enqueue(&c5).await.0, StatusCode::ACCEPTED);
    pr5["state"] = json!("closed");
    pr5["merged"] = json!(true);
    pr5["merged_at"] = json!("2026-10-22T10:00:00Z");
    assert_eq!(hook(a, "pull_request", "p-9", &pr_ev("closed", &pr5), SECRET).await, StatusCode::OK);
    assert_eq!((change(&c5).await["state"].as_str(), depth().await), (Some("landed"), 0));

    // Into another branch: not a change. A draft PR: a draft change.
    let mut pr6 = pr_json(&g.mock, 6, "feature", &feature_sha, &main_sha);
    pr6["base"]["ref"] = json!("release");
    assert_eq!(hook(a, "pull_request", "p-10", &pr_ev("opened", &pr6), SECRET).await, StatusCode::OK);
    assert!(find(6).await.is_none());
    let mut pr7 = pr_json(&g.mock, 7, "side", &rev(home, &w, "side"), &main_sha);
    pr7["draft"] = json!(true);
    assert_eq!(hook(a, "pull_request", "p-11", &pr_ev("opened", &pr7), SECRET).await, StatusCode::OK);
    let c7 = find(7).await.expect("PR 7's change");
    assert_eq!(c7["draft"], true);
    assert!(c7["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "draft"), "{c7}");
    // Nothing went back to GitHub for any of it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(g.mock.calls.lock().iter().all(|(_, p, _)| !p.contains("/pulls/") && !p.contains("/issues/")), "{:?}", g.mock.calls.lock());

    // Issues: labels set the priority, edited, unlabeled, an issue event about a PR, closed as
    // not planned, reopened, closed with no reason, deleted; comments and unknown events are fine.
    let issue_ev = |action: &str, issue: Value| json!({ "action": action, "issue": issue, "repository": repo, "installation": inst });
    hook(a, "issues", "i-1", &issue_ev("opened", issue_json(9, "Perf", "open", &["P1", "perf"])), SECRET).await;
    let item = || async { a.ok("GET", "/v1/code/repos/acme/app/work", None).await.as_array().unwrap().iter().find(|w| w["source"] == "github#9").cloned().unwrap() };
    let w9 = item().await;
    assert_eq!((w9["priority"].as_i64(), w9["github_url"].as_str()), (Some(1), Some("https://github.com/acme/app/issues/9")));
    assert_eq!(hook(a, "issues", "i-6", &issue_ev("edited", issue_json(9, "Perf on Safari", "open", &["P1", "perf"])), SECRET).await, StatusCode::OK);
    assert_eq!(item().await["title"], "Perf on Safari");
    assert_eq!(hook(a, "issues", "i-2", &issue_ev("unlabeled", issue_json(9, "Perf", "open", &["perf"])), SECRET).await, StatusCode::OK);
    let w9 = item().await;
    assert_eq!((w9["priority"].as_i64(), w9["labels"].clone()), (Some(2), json!(["perf"])));
    let mut not_planned = issue_json(9, "Perf", "closed", &["perf"]);
    not_planned["state_reason"] = json!("not_planned");
    assert_eq!(hook(a, "issues", "i-7", &issue_ev("closed", not_planned), SECRET).await, StatusCode::OK);
    assert_eq!(item().await["state"], "closed", "not planned closes, it isn't done");
    assert_eq!(hook(a, "issues", "i-8", &issue_ev("reopened", issue_json(9, "Perf", "open", &["perf"])), SECRET).await, StatusCode::OK);
    assert_eq!(item().await["state"], "open", "reopened goes back in the queue");
    let comment = json!({ "action": "created", "issue": issue_json(9, "Perf", "open", &[]), "comment": { "id": 1, "body": "+1", "user": { "login": "octo", "id": 9 }, "html_url": "https://github.com/c/1" }, "repository": repo, "installation": inst });
    assert!(hook(a, "issue_comment", "i-9", &comment, SECRET).await.is_success());
    assert!(hook(a, "star", "i-10", &json!({ "action": "created", "repository": repo }), SECRET).await.is_success());
    let mut about_pr = issue_json(3, "PR 3", "open", &[]);
    about_pr["pull_request"] = json!({ "url": "https://api.github.com/repos/acme/app/pulls/3" });
    assert_eq!(hook(a, "issues", "i-3", &issue_ev("opened", about_pr), SECRET).await, StatusCode::OK);
    assert!(!a.ok("GET", "/v1/code/repos/acme/app/work", None).await.as_array().unwrap().iter().any(|w| w["source"] == "github#3"));
    assert_eq!(hook(a, "issues", "i-4", &issue_ev("closed", issue_json(9, "Perf", "closed", &["perf"])), SECRET).await, StatusCode::OK);
    assert_eq!(item().await["state"], "done", "closed without a reason is completed");
    assert_eq!(hook(a, "issues", "i-5", &issue_ev("deleted", issue_json(9, "Perf", "closed", &["perf"])), SECRET).await, StatusCode::OK);
    assert_eq!(item().await["state"], "done", "deleted is ignored");

    // Installations: other installations and repos don't matter; suspend / unsuspend; the repo
    // removed from the installation and added back.
    let gh = || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["github"].clone() };
    let account = json!({ "login": "acme", "id": 1, "type": "Organization" });
    let installation = |action: &str, id: u64| json!({ "action": action, "installation": { "id": id, "account": account } });
    let repos = |action: &str, full_name: &str| {
        let mut r = repo_json(&g.mock);
        r["full_name"] = json!(full_name);
        let (added, removed) = if action == "added" { (vec![r], vec![]) } else { (vec![], vec![r]) };
        json!({ "action": action, "installation": { "id": INSTALLATION, "account": account }, "repositories_added": added, "repositories_removed": removed })
    };
    assert_eq!(hook(a, "installation_repositories", "x-1", &repos("removed", "acme/infra"), SECRET).await, StatusCode::OK);
    assert_eq!(hook(a, "installation", "x-2", &installation("deleted", 8), SECRET).await, StatusCode::OK);
    assert_eq!(gh().await["connected"], true);
    assert_eq!(hook(a, "installation", "x-3", &installation("suspend", INSTALLATION), SECRET).await, StatusCode::OK);
    let l = gh().await;
    assert_eq!((l["connected"].as_bool(), l["mirror"]["state"].as_str()), (Some(false), Some("paused")), "{l}");
    assert!(l["mirror"]["error"].as_str().unwrap().contains("suspended"), "{l}");
    assert_eq!(hook(a, "installation", "x-4", &installation("new_permissions_accepted", INSTALLATION), SECRET).await, StatusCode::OK);
    assert_eq!(gh().await["connected"], false, "new permissions don't reconnect");
    assert_eq!(hook(a, "installation", "x-5", &installation("unsuspend", INSTALLATION), SECRET).await, StatusCode::OK);
    until("reconnected and unpaused", || async {
        let l = gh().await;
        l["connected"] == true && l["mirror"]["state"] != "paused" && l["mirror"]["error"].is_null()
    })
    .await;
    // Uninstalling the App disconnects the repo and pauses its mirror; reinstalling reconnects.
    assert_eq!(hook(a, "installation", "x-9", &installation("deleted", INSTALLATION), SECRET).await, StatusCode::OK);
    let l = gh().await;
    assert_eq!((l["connected"].as_bool(), l["mirror"]["state"].as_str()), (Some(false), Some("paused")), "{l}");
    assert!(l["mirror"]["error"].as_str().unwrap().contains("uninstalled"), "{l}");
    let mut reinstall = installation("created", INSTALLATION);
    reinstall["repositories"] = json!([]);
    assert_eq!(hook(a, "installation", "x-10", &reinstall, SECRET).await, StatusCode::OK);
    until("reinstalled and unpaused", || async {
        let l = gh().await;
        l["connected"] == true && l["mirror"]["state"] != "paused" && l["mirror"]["error"].is_null()
    })
    .await;
    assert_eq!(hook(a, "installation_repositories", "x-6", &repos("removed", "acme/app"), SECRET).await, StatusCode::OK);
    let l = gh().await;
    assert_eq!((l["connected"].as_bool(), l["mirror"]["state"].as_str()), (Some(false), Some("paused")), "{l}");
    assert!(l["mirror"]["error"].as_str().unwrap().contains("lost access"), "{l}");
    // Disconnected: webhooks for it are ignored.
    hook(a, "issues", "x-7", &issue_ev("opened", issue_json(10, "While away", "open", &[])), SECRET).await;
    assert!(!a.ok("GET", "/v1/code/repos/acme/app/work", None).await.as_array().unwrap().iter().any(|w| w["source"] == "github#10"));
    assert_eq!(hook(a, "installation_repositories", "x-8", &repos("added", "acme/app"), SECRET).await, StatusCode::OK);
    assert_eq!(gh().await["connected"], true);

    // Deliveries: unsigned 401, unparseable 400, and a failed one isn't recorded, so GitHub's
    // redelivery is processed again.
    use tower::ServiceExt;
    let unsigned = axum::http::Request::post("/v1/code/github/webhook").header("x-github-event", "ping").body(axum::body::Body::from("{}")).unwrap();
    assert_eq!(a.router.clone().oneshot(unsigned).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let bad = "{not json";
    let req = axum::http::Request::post("/v1/code/github/webhook")
        .header("x-github-event", "push")
        .header("x-hub-signature-256", code_github::webhook::sign(SECRET.as_bytes(), bad.as_bytes()))
        .body(axum::body::Body::from(bad))
        .unwrap();
    assert_eq!(a.router.clone().oneshot(req).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let missing = json!({ "ref": "refs/heads/ghost", "before": ZERO, "after": "1234567890123456789012345678901234567890", "created": true, "repository": repo, "sender": { "login": "human", "id": 5 }, "installation": inst });
    let first = hook(a, "push", "f-1", &missing, SECRET).await;
    assert!(first.is_server_error() || first.is_client_error(), "{first}");
    assert_eq!(hook(a, "push", "f-1", &missing, SECRET).await, first, "the redelivery is processed, not deduplicated");
}

/// roll → GitHub: every check status becomes the matching check run; GitHub's own checks and
/// changes without a PR aren't sent; landing a PR whose PR GitHub already closed writes nothing;
/// a landed PR is commented on once however often its change is updated; an item closed in
/// roll closes its issue as not planned; an issue GitHub already closed isn't written to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outbound_notifications_once_each() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let (main_sha, feature_sha) = (rev(home, &w, "main"), rev(home, &w, "feature"));
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = app(None);
    enable(&a, &g, fast_mirror());
    let a = &a;
    connect(a).await;
    until("PR import", || async { imported(a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await[0]["id"].as_str().unwrap().to_string();
    let check = |change: &str, body: Value| {
        let change = change.to_string();
        async move { a.ok("POST", &format!("/v1/code/changes/{change}/checks"), Some(body)).await }
    };
    let runs = || g.mock.calls.lock().iter().filter(|(m, p, _)| m == "POST" && p.ends_with("/check-runs")).map(|(_, _, b)| b.clone()).collect::<Vec<_>>();

    // Not sent: a check GitHub reported, and a check on a change that isn't a PR.
    check(&c3, json!({ "name": "from-github", "status": "success", "source": "github" })).await;
    let native = a.ok("POST", "/v1/code/repos/acme/app/changes", Some(json!({ "title": "native", "message": "native", "files": { "n.txt": { "content": "n\n" } } }))).await;
    check(native["id"].as_str().unwrap(), json!({ "name": "native-check", "status": "success" })).await;
    // Every status, mapped.
    let want = [
        ("q", "queued", "queued", Value::Null),
        ("r", "running", "in_progress", Value::Null),
        ("s", "success", "completed", json!("success")),
        ("f", "failure", "completed", json!("failure")),
        ("c", "cancelled", "completed", json!("cancelled")),
        ("k", "skipped", "completed", json!("skipped")),
    ];
    for (name, status, _, _) in &want {
        check(&c3, json!({ "name": name, "status": status })).await;
    }
    until("six check runs", || async { runs().len() >= 6 }).await;
    let runs = runs();
    assert_eq!(runs.len(), 6, "{runs:?}");
    for (name, _, status, conclusion) in &want {
        let run = runs.iter().find(|r| r["name"] == *name).unwrap_or_else(|| panic!("check run {name}"));
        assert_eq!((&run["status"], run.get("conclusion").unwrap_or(&Value::Null)), (&json!(status), conclusion), "{name}: {run}");
        assert_eq!(run["head_sha"], feature_sha);
        assert_eq!(run["external_id"], c3.as_str());
    }

    // GitHub already closed PR 3 (a webhook roll missed): landing writes nothing to it.
    g.mock.pulls.lock().get_mut(&3).unwrap()["state"] = json!("closed");
    a.ok("POST", &format!("/v1/code/changes/{c3}/land"), None).await;
    until("PR 3's notification handled", || async { mark(a, &format!("pr-landed:{c3}")).as_deref() == Some("done") }).await;
    assert!(!g.mock.calls.lock().iter().any(|(_, p, _)| p == "/repos/acme/app/issues/3/comments" || p == "/repos/acme/app/pulls/3"));

    // An open PR's change lands: one comment and one close, however often the change is updated.
    git(home, &w, &["checkout", "-qb", "topic"]);
    write(&w, "topic.txt", "topic\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "topic work"]);
    git(home, &w, &["push", "-q", "origin", "topic"]);
    git(home, &w, &["checkout", "-q", "main"]);
    let pr5 = pr_json(&g.mock, 5, "topic", &rev(home, &w, "topic"), &main_sha);
    g.mock.pulls.lock().insert(5, pr5.clone());
    let ev = json!({ "action": "opened", "number": 5, "pull_request": pr5, "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION } });
    assert_eq!(hook(a, "pull_request", "o-1", &ev, SECRET).await, StatusCode::OK);
    let c5 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().iter().find(|c| c["github_pr"] == 5).unwrap()["id"].as_str().unwrap().to_string();
    a.ok("POST", &format!("/v1/code/changes/{c5}/land"), None).await;
    until("PR 5 closed", || async { g.mock.calls.lock().iter().any(|(m, p, _)| m == "PATCH" && p == "/repos/acme/app/pulls/5") }).await;
    a.ok("PATCH", &format!("/v1/code/changes/{c5}"), Some(json!({ "title": "renamed after landing" }))).await;
    // A later notification (a check run) shows the update's event was handled.
    let n = g.mock.check_runs.load(Ordering::SeqCst);
    let c7 = a.ok("POST", "/v1/code/repos/acme/app/changes", Some(json!({ "title": "x", "message": "x", "files": { "x.txt": { "content": "x\n" } } }))).await;
    a.code.db.sql().execute("UPDATE changes SET github_pr = 77 WHERE id = ?", [c7["id"].as_str().unwrap()]).unwrap();
    check(c7["id"].as_str().unwrap(), json!({ "name": "later", "status": "running" })).await;
    until("the later check run", || async { g.mock.check_runs.load(Ordering::SeqCst) > n }).await;
    let calls = g.mock.calls.lock().clone();
    assert_eq!(calls.iter().filter(|(_, p, _)| p == "/repos/acme/app/issues/5/comments").count(), 1, "{calls:?}");
    assert_eq!(calls.iter().filter(|(_, p, _)| p == "/repos/acme/app/pulls/5").count(), 1, "{calls:?}");

    // Issues: closed in roll → closed as not planned; already closed on GitHub → untouched.
    g.mock.issues.lock().extend([issue_json(11, "Won't do", "open", &[]), issue_json(12, "Done elsewhere", "open", &[])]);
    let repo = repo_json(&g.mock);
    for (d, n) in [("w-1", 11), ("w-2", 12)] {
        let ev = json!({ "action": "opened", "issue": issue_json(n, "t", "open", &[]), "repository": repo, "installation": { "id": INSTALLATION } });
        assert_eq!(hook(a, "issues", d, &ev, SECRET).await, StatusCode::OK);
    }
    let work = a.ok("GET", "/v1/code/repos/acme/app/work", None).await;
    let wid = |n: u64| work.as_array().unwrap().iter().find(|w| w["source"] == format!("github#{n}")).unwrap()["id"].as_str().unwrap().to_string();
    let (w11, w12) = (wid(11), wid(12));
    a.ok("PATCH", &format!("/v1/code/work/{w11}"), Some(json!({ "state": "closed" }))).await;
    until("issue 11 closed as not planned", || async {
        g.mock.calls.lock().iter().any(|(m, p, b)| m == "PATCH" && p == "/repos/acme/app/issues/11" && b["state"] == "closed" && b["state_reason"] == "not_planned")
    })
    .await;
    assert!(g.mock.calls.lock().iter().any(|(_, p, b)| p == "/repos/acme/app/issues/11/comments" && b["body"] == "Closed in roll."));
    g.mock.issues.lock().iter_mut().find(|i| i["number"] == 12).unwrap()["state"] = json!("closed");
    a.ok("POST", &format!("/v1/code/work/{w12}/complete"), None).await;
    until("issue 12's notification handled", || async { mark(a, &format!("issue-closed:{w12}")).as_deref() == Some("done") }).await;
    assert!(!g.mock.calls.lock().iter().any(|(_, p, _)| p.starts_with("/repos/acme/app/issues/12")));
}

// ── automations (chatops::github) ──

/// A connected acme/app (imported), with an automation listening to GitHub: what an owner who
/// set one up has. (App, the mock, its scratch dir, the history checkout, main's sha.)
async fn connected_with_automation() -> (App, GitHub, tempfile::TempDir, std::path::PathBuf, String) {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let w = seed(tmp.path(), &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(tmp.path(), &w, &["rev-parse", "main"]).trim().to_string();
    let a = app(Some("admin-secret"));
    enable(&a, &g, fast_mirror());
    let admin = Some("admin-secret");
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), admin).await;
    let connect = json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } });
    let (s, body) = a.req("POST", "/v1/code/repos", Some(connect), admin).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    until("import", || async { a.req("GET", "/v1/code/repos/acme/app", None, admin).await.1["import"]["state"] == "done" }).await;
    let spec = json!({
        "name": "hear GitHub",
        "trigger": { "type": "integration", "source": "github" },
        "steps": [{ "id": "say", "kind": "notify", "config": { "title": "heard {{event.kind}}" } }],
    });
    let (s, v) = a.req("POST", "/v1/code/agents/acme/automations", Some(spec), admin).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    (a, g, tmp, w, main_sha)
}

fn automation_events(a: &App) -> Vec<(String, String, Value)> {
    a.code
        .db
        .sql()
        .query("SELECT key, kind, data FROM automation_events WHERE source = 'github' ORDER BY at, id", code_sql::params![], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .unwrap()
        .into_iter()
        .map(|(k, kind, d)| (k, kind, serde_json::from_str(&d).unwrap()))
        .collect()
}

/// GitHub events become the owner's automation events: issues, `@roll` comments only from people
/// who can push, review requests, failing checks on main; a fork's pull request only once a
/// writer approved its head; a redelivery records nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automations_hear_github_with_comment_authority_and_forks() {
    let (a, g, tmp, w, main_sha) = connected_with_automation().await;
    let repo = repo_json(&g.mock);
    let inst = json!({ "id": INSTALLATION });
    let kinds = |a: &App| automation_events(a).into_iter().map(|e| e.1).collect::<Vec<_>>();

    // The App's installations span every tenant, so only the admin lists or connects them.
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent" })), Some("admin-secret")).await;
    let owner = t["token"].as_str().map(str::to_string);
    let (s, _) = a.req("GET", "/v1/code/github/installations", None, owner.as_deref()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "an owner token can't list every tenant's installations");
    let (s, _) = a.req("GET", "/v1/code/github/installations", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let connect = json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } });
    let (s, body) = a.req("POST", "/v1/code/repos", Some(connect), owner.as_deref()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    let mut issue = issue_json(11, "Login 500s", "open", &["bug"]);
    issue["author_association"] = json!("MEMBER");
    let opened = json!({ "action": "opened", "issue": issue, "repository": repo, "installation": inst });
    assert_eq!(hook(&a, "issues", "a-1", &opened, SECRET).await, StatusCode::OK);
    // GitHub redelivering it (the same delivery id) records nothing new.
    assert_eq!(hook(&a, "issues", "a-1", &opened, SECRET).await, StatusCode::OK);
    let ev = automation_events(&a);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].1.as_str(), ev[0].2["number"].as_u64(), ev[0].2["trusted_author"].as_bool(), ev[0].2["repo"].as_str()), ("issue.opened", Some(11), Some(true), Some("acme/app")));
    assert!(ev[0].0.starts_with("github:a-1:"), "{}", ev[0].0);

    // A stranger's issue is recorded as untrusted: the trigger takes it only with `anyone`.
    let mut stranger = issue_json(12, "Please add X", "open", &[]);
    stranger["author_association"] = json!("NONE");
    hook(&a, "issues", "a-2", &json!({ "action": "opened", "issue": stranger, "repository": repo, "installation": inst }), SECRET).await;
    let ev = automation_events(&a);
    let trig = a.code.automations.registry.trigger("github").expect("registered");
    let as_event = |e: &(String, String, Value)| -> agent_proto::automation::AutomationEvent {
        serde_json::from_value(json!({ "id": "aev_1", "source": "github", "kind": e.1, "key": e.0, "at": 1, "data": e.2 })).unwrap()
    };
    assert!(!trig.matches(&json!({}), &as_event(&ev[1])), "a stranger's issue starts nothing by default");
    assert!(trig.matches(&json!({ "anyone": true }), &as_event(&ev[1])));
    assert!(trig.matches(&json!({ "labels": ["bug"] }), &as_event(&ev[0])));
    assert!(!trig.matches(&json!({ "labels": ["docs"] }), &as_event(&ev[0])));
    assert!(!trig.matches(&json!({ "repos": ["other"] }), &as_event(&ev[0])));

    // @roll comments: only people who can push speak for roll.
    let comment = |n: u64, body: &str, assoc: &str, pr: bool| {
        let mut issue = issue_json(n, "t", "open", &[]);
        if pr {
            issue["pull_request"] = json!({ "url": "x" });
        }
        json!({ "action": "created", "issue": issue, "repository": repo, "installation": inst,
            "comment": { "id": n * 100, "body": body, "user": { "login": "octo", "id": 9 }, "author_association": assoc, "html_url": "https://github.com/acme/app/issues/1#c" } })
    };
    hook(&a, "issue_comment", "a-3", &comment(11, "@roll fix this please", "CONTRIBUTOR", false), SECRET).await;
    hook(&a, "issue_comment", "a-4", &comment(11, "@rollbar is down", "MEMBER", false), SECRET).await;
    hook(&a, "issue_comment", "a-5", &comment(11, "thanks!", "MEMBER", false), SECRET).await;
    assert_eq!(kinds(&a), ["issue.opened", "issue.opened"]);
    hook(&a, "issue_comment", "a-6", &comment(11, "Looks real. @roll fix this please", "MEMBER", false), SECRET).await;
    let ev = automation_events(&a);
    assert_eq!((ev[2].1.as_str(), ev[2].2["prompt"].as_str(), ev[2].2["is_pr"].as_bool()), ("comment.mention", Some("fix this please"), Some(false)));

    // A fork's pull request: nothing until a writer approves its head.
    let feature_sha = git(tmp.path(), &w, &["rev-parse", "feature"]).trim().to_string();
    let mut pr = pr_json(&g.mock, 8, "feature", &feature_sha, &main_sha);
    pr["head"]["repo"] = json!({ "id": 99, "name": "app", "full_name": "eve/app", "private": false, "default_branch": "main", "fork": true,
        "owner": { "login": "eve", "id": 99, "type": "User" }, "clone_url": format!("{}/git/acme/app.git", g.base), "html_url": "https://github.com/eve/app" });
    g.mock.pulls.lock().insert(8, pr.clone());
    hook(&a, "pull_request", "a-7", &json!({ "action": "opened", "number": 8, "pull_request": pr, "repository": repo, "installation": inst }), SECRET).await;
    hook(&a, "issue_comment", "a-8", &comment(8, "@roll review the migration", "MEMBER", true), SECRET).await;
    let review = json!({ "action": "review_requested", "number": 8, "pull_request": pr, "requested_reviewer": { "login": "ada" }, "repository": repo, "installation": inst });
    hook(&a, "pull_request", "a-9", &review, SECRET).await;
    assert_eq!(automation_events(&a).len(), 3, "a fork's pull request starts nothing on its own");
    let changes = a.req("GET", "/v1/code/repos/acme/app/changes", None, Some("admin-secret")).await.1;
    let fork_change = changes.as_array().unwrap().iter().find(|c| c["github_pr"] == 8).cloned().expect("a change for the fork's PR");
    assert_eq!(fork_change["trusted"], false, "no automatic checks or previews: {fork_change}");
    assert!(fork_change["branch"].is_null(), "a fork's branch isn't one of ours: {fork_change}");
    let fork = fork_change["id"].as_str().unwrap().to_string();
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{fork}/approve-run"), None, Some("admin-secret")).await;
    assert_eq!(s, StatusCode::OK);
    hook(&a, "issue_comment", "a-10", &comment(8, "@roll review the migration", "MEMBER", true), SECRET).await;
    hook(&a, "pull_request", "a-11", &review, SECRET).await;
    let ev = automation_events(&a);
    assert_eq!(kinds(&a)[3..], ["comment.mention", "review.requested"]);
    assert_eq!((ev[3].2["is_pr"].as_bool(), ev[3].2["change"]["id"].as_str()), (Some(true), Some(fork.as_str())));
    assert_eq!(ev[4].2["reviewer"], "ada");

    // Failing checks: on main (a check run, a commit status); a fork's branch with no pull request
    // listed starts nothing; a passing one neither.
    let check = |branch: &str, conclusion: &str| {
        json!({ "action": "completed", "repository": repo, "installation": inst, "check_run": {
            "id": 1, "name": "ci / test", "head_sha": main_sha, "status": "completed", "conclusion": conclusion, "details_url": "https://ci.example.com/1",
            "check_suite": { "head_branch": branch }, "pull_requests": [] } })
    };
    hook(&a, "check_run", "a-12", &check("main", "failure"), SECRET).await;
    hook(&a, "check_run", "a-13", &check("eve-branch", "failure"), SECRET).await;
    hook(&a, "check_run", "a-14", &check("main", "success"), SECRET).await;
    let status = json!({ "sha": main_sha, "state": "error", "context": "ci/build", "branches": [{ "name": "main" }], "repository": repo, "installation": inst, "target_url": "https://ci.example.com/b" });
    hook(&a, "status", "a-15", &status, SECRET).await;
    let ev = automation_events(&a);
    assert_eq!(kinds(&a)[5..], ["check.failed", "check.failed"], "{ev:?}");
    assert_eq!((ev[5].2["on_default"].as_bool(), ev[5].2["check"].as_str(), ev[5].2["via"].as_str()), (Some(true), Some("ci / test"), Some("check_run")));
    assert_eq!((ev[6].2["check"].as_str(), ev[6].2["via"].as_str()), (Some("ci/build"), Some("status")));
    assert!(trig.matches(&json!({ "branches": ["main"] }), &as_event(&ev[5])));
    assert!(!trig.matches(&json!({ "branches": ["release"] }), &as_event(&ev[5])));

    // Big payloads (GitHub allows 25 MB) aren't cut off at axum's 2 MB default.
    let big = json!({ "zen": "x".repeat(3 << 20), "hook_id": 1, "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "ping", "big-1", &big, SECRET).await, StatusCode::OK);
}

/// The GitHub steps against the mock: a comment (made once however often the step runs), labels,
/// a commit status, and a pull request opened from a roll change (its head pushed as a branch),
/// which the change is linked to; only the owner's own connected repos.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automation_steps_comment_label_status_and_open_a_pull_request() {
    use agent_proto::automation::{AutomationId, AutomationRunId, Step};
    use code_control::agents::automations::{OwnerRef, StepContext, StepOutcome};
    let (a, g, _tmp, _w, main_sha) = connected_with_automation().await;
    let owner = OwnerRef { id: a.code.db.owner("acme").unwrap().unwrap().id, name: "acme".into() };
    let event = json!({ "source": "github", "kind": "comment.mention", "data": { "repo": "acme/app", "number": 11, "sha": main_sha } });
    let run = |kind: &str, config: Value, step: &str| {
        let s: Step = serde_json::from_value(json!({ "id": step, "kind": kind, "config": config })).unwrap();
        let mut ctx = StepContext::new(a.code.clone(), owner.clone(), AutomationId("aut_gh".into()), "gh".into(), None, AutomationRunId("arn_gh".into()), s, 1, json!({ "event": event, "steps": {} }), Value::Null, 0);
        let k = a.code.automations.registry.step(kind).expect("registered");
        async move { k.run(&mut ctx).await }
    };
    let calls = |m: &str, p: &str| g.mock.calls.lock().iter().filter(|c| c.0 == m && c.1 == p).count();

    let out = run("github.comment", json!({ "body": "On it: {{event.data.number}}" }), "c").await;
    assert!(matches!(&out, StepOutcome::Done(v) if v["number"] == 11), "{out:?}");
    assert_eq!(g.mock.calls.lock().iter().rev().find(|c| c.1 == "/repos/acme/app/issues/11/comments").unwrap().2["body"], "On it: 11");
    // The same step of the same run again (a retried attempt): no second comment.
    run("github.comment", json!({ "body": "On it" }), "c").await;
    assert_eq!(calls("POST", "/repos/acme/app/issues/11/comments"), 1);

    let out = run("github.label", json!({ "add": ["roll"], "remove": ["triage", "absent"] }), "l").await;
    assert_eq!(out, StepOutcome::Done(json!({ "labels": ["roll"], "number": 11 })));
    assert_eq!(calls("DELETE", "/repos/acme/app/issues/11/labels/absent"), 1, "a label it doesn't have is fine");

    let out = run("github.status", json!({ "state": "failure", "context": "roll/agent", "description": "the agent found the bug" }), "s").await;
    assert!(matches!(out, StepOutcome::Done(_)), "{out:?}");
    let st = g.mock.calls.lock().iter().find(|c| c.1 == format!("/repos/acme/app/statuses/{main_sha}")).unwrap().2.clone();
    assert_eq!((st["state"].as_str(), st["context"].as_str()), (Some("failure"), Some("roll/agent")));
    assert!(matches!(run("github.status", json!({ "state": "great" }), "s2").await, StepOutcome::Fail(_)));

    // Another owner's repo is out of reach.
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "evil", "kind": "org" })), Some("admin-secret")).await;
    a.req("POST", "/v1/code/repos", Some(json!({ "owner": "evil", "name": "app" })), Some("admin-secret")).await;
    let out = run("github.comment", json!({ "repo": "evil/app", "number": 1, "body": "x" }), "e").await;
    assert!(matches!(&out, StepOutcome::Fail(m) if m.contains("isn't one of this owner's")), "{out:?}");

    // A roll change opened as a pull request: its head pushed to GitHub, the PR opened, the
    // change linked to it (so its webhook finds the change), and a second run finds that PR.
    let body = json!({ "title": "Fix login", "message": "Fix login", "base": "main", "author": "agent", "files": { "fix.txt": { "content": "fixed\n" } } });
    let (s, c) = a.req("POST", "/v1/code/repos/acme/app/changes", Some(body), Some("admin-secret")).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let n = c["number"].as_u64().unwrap();
    let out = run("github.pr", json!({ "change": n.to_string() }), "p").await;
    let StepOutcome::Done(pr) = out else { panic!("{out:?}") };
    let created = g.mock.calls.lock().iter().find(|c| c.1 == "/repos/acme/app/pulls").unwrap().2.clone();
    assert_eq!((created["base"].as_str(), created["title"].as_str()), (Some("main"), Some("Fix login")));
    let branch = created["head"].as_str().unwrap().to_string();
    let refs = g.gh.ok("GET", "/v1/code/repos/acme/app/refs", None).await;
    assert!(refs.to_string().contains(&branch), "the branch is on GitHub: {refs}");
    let linked = a.req("GET", &format!("/v1/code/repos/acme/app/changes/{n}"), None, Some("admin-secret")).await.1;
    assert_eq!(linked["github_pr"], pr["number"]);
    run("github.pr", json!({ "change": n }), "p2").await;
    assert_eq!(calls("POST", "/repos/acme/app/pulls"), 1, "one pull request per change");
}

// ── pickers (agents::lookup::github) ──

/// The pickers' GitHub lookups go through the owner's connected repos with the installation's
/// token (never in an answer): repos, branches (GitHub's pages), labels (made here, which drops
/// the cached ones), assignees, reviewers (people and teams), teams, milestones, workflows,
/// environments, issues and pull requests (by number too), stored values resolved. Another
/// owner's repo is a 404; a missing permission says which; a rate limit is said and GitHub left
/// alone until then; a repo GitHub lost access to asks to connect it again at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pickers_look_up_github_through_the_owners_repos() {
    let (a, g, _tmp, _w, _main) = connected_with_automation().await;
    let admin = Some("admin-secret");
    let base = "/v1/code/agents/acme/lookup/github";
    let answers = Arc::new(Mutex::new(Vec::<String>::new()));
    let get = |path: String| {
        let (a, answers) = (&a, answers.clone());
        async move {
            let (s, v) = a.req("GET", &path, None, admin).await;
            answers.lock().push(v.to_string());
            (s, v)
        }
    };
    let ids = |v: &Value| v["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let reads = |m: &Mock, path: &str| m.reads.lock().iter().filter(|r| r.starts_with(path)).count();

    // The owner's connected repos: roll's names, GitHub's in the hint (roll alone answers).
    let (s, v) = get(format!("{base}/repos")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"], json!([{ "id": "acme/app", "label": "acme/app", "key": "app", "hint": "acme/app", "icon": "repo" }]));

    // Branches: GitHub's pages (the cursor is its page number); the default one says so.
    let (_, v) = get(format!("{base}/branches?parent=app&limit=2")).await;
    assert_eq!((ids(&v), v["next"].as_str()), (vec!["main".to_string(), "feature".into()], Some("2")), "{v}");
    assert_eq!(v["items"][0]["hint"], "default");
    let (_, v) = get(format!("{base}/branches?parent=acme/app&limit=2&cursor=2")).await;
    assert_eq!((ids(&v), v.get("next")), (vec!["release/1.0".to_string()], None));
    assert!(g.mock.reads.lock().contains(&"/repos/acme/app/branches?page=2".to_string()));
    // A search reads GitHub's pages of 100 and keeps what matches.
    let (_, v) = get(format!("{base}/branches?parent=app&q=REL")).await;
    assert_eq!(ids(&v), ["release/1.0"]);
    let (s, _) = get(format!("{base}/branches?parent=app&cursor=https://evil.example/")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a cursor is a page number, never a URL");

    // Labels with their colour; the second look is the cache's; making one shows it at once.
    let (_, v) = get(format!("{base}/labels?parent=app")).await;
    assert_eq!(v["items"][0], json!({ "id": "bug", "label": "bug", "hint": "Something isn't working", "color": "#d73a4a" }));
    assert_eq!(reads(&g.mock, "/repos/acme/app/labels"), 1);
    get(format!("{base}/labels?parent=app")).await;
    assert_eq!(reads(&g.mock, "/repos/acme/app/labels"), 1, "served from the cache");
    let (s, v) = a.req("POST", &format!("{base}/labels"), Some(json!({ "name": "needs qa", "parent": "app", "color": "#4c8dff" })), admin).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "id": "needs qa", "label": "needs qa", "color": "#4c8dff" }));
    assert_eq!(g.mock.calls.lock().iter().find(|c| c.1 == "/repos/acme/app/labels").unwrap().2, json!({ "name": "needs qa", "color": "4c8dff" }));
    let (_, v) = get(format!("{base}/labels?parent=app")).await;
    assert!(ids(&v).contains(&"needs qa".to_string()) && reads(&g.mock, "/repos/acme/app/labels") == 2, "{v}");
    let (s, _) = a.req("POST", &format!("{base}/labels"), Some(json!({ "name": "bug", "parent": "app" })), admin).await;
    assert_eq!(s, StatusCode::CONFLICT, "GitHub has it already");
    let (s, _) = a.req("POST", &format!("{base}/branches"), Some(json!({ "name": "x", "parent": "app" })), admin).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "roll only makes labels");
    // A field's stored values resolve (a trigger's labels without a repo: every connected repo's).
    let (_, v) = get(format!("{base}/labels?ids=bug,P1")).await;
    assert_eq!(ids(&v), ["bug", "P1"]);
    assert!(v["items"][0].get("group").is_none(), "one connected repo: its rows as they are");

    // People: only https pictures, bots say so; reviewers are people, then the org's teams.
    let (_, v) = get(format!("{base}/users?parent=app")).await;
    assert_eq!(v["items"][0], json!({ "id": "ada", "label": "ada", "avatar": "https://avatars.githubusercontent.com/u/1", "icon": "user" }));
    assert_eq!(v["items"][1], json!({ "id": "octo-bot", "label": "octo-bot", "hint": "bot", "icon": "user" }));
    let (_, v) = get(format!("{base}/reviewers?parent=app")).await;
    assert_eq!(ids(&v), ["ada", "grace", "acme/core", "acme/web"]);
    assert_eq!((v["items"][0]["group"].as_str(), v["items"][2]["group"].as_str()), (Some("People"), Some("Teams")));
    let (_, v) = get(format!("{base}/teams?parent=app&q=platform")).await;
    assert_eq!(v["items"], json!([{ "id": "acme/web", "label": "Web platform", "key": "web", "hint": "@acme/web", "icon": "team" }]));

    // Milestones, workflows (by file), environments.
    let (_, v) = get(format!("{base}/milestones?parent=app")).await;
    assert_eq!(v["items"][0], json!({ "id": "1", "label": "v1.0", "key": "v1.0", "hint": "due 2026-12-01", "icon": "milestone" }));
    assert_eq!(v["items"][1]["hint"], "4 open");
    let (_, v) = get(format!("{base}/workflows?parent=app")).await;
    assert_eq!(v["items"][0], json!({ "id": "ci.yml", "label": "CI", "key": "11", "hint": ".github/workflows/ci.yml", "icon": "workflow" }));
    assert_eq!(v["items"][1]["hint"], "disabled · .github/workflows/nightly.yml");
    let (_, v) = get(format!("{base}/environments?parent=app")).await;
    assert_eq!(ids(&v), ["production", "staging"]);

    // Open issues and pull requests; a number is looked up as it is.
    {
        let mut issues = g.mock.issues.lock();
        issues.push(issue_json(21, "Login 500s on Safari", "open", &["bug"]));
        let mut pr = issue_json(22, "Cache sessions", "open", &[]);
        pr["pull_request"] = json!({ "url": "x" });
        issues.push(pr);
    }
    let (_, v) = get(format!("{base}/issues?parent=app&q=safari")).await;
    // Issues and pull requests each say which they are (the picker lines their titles up).
    assert_eq!(v["items"], json!([{ "id": "21", "label": "Login 500s on Safari", "key": "#21", "hint": "#21", "icon": "issue" }]));
    let (_, v) = get(format!("{base}/issues?parent=app&q=%2322")).await;
    assert_eq!((v["items"][0]["label"].as_str(), v["items"][0]["icon"].as_str()), (Some("Cache sessions"), Some("pull")));
    let (_, v) = get(format!("{base}/issues?parent=app&ids=21,%23999")).await;
    assert_eq!(ids(&v), ["21"], "a number GitHub doesn't know is left out");

    // Another owner's repo is out of reach, whoever asks and however it's named.
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "evil", "kind": "org" })), admin).await;
    a.req("POST", "/v1/code/repos", Some(json!({ "owner": "evil", "name": "app" })), admin).await;
    for path in [format!("{base}/branches?parent=evil/app"), "/v1/code/agents/evil/lookup/github/branches?parent=acme/app".into(), "/v1/code/agents/evil/lookup/github/labels?parent=app".into()] {
        let (s, v) = get(path.clone()).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}: {v}");
    }
    let (_, v) = get("/v1/code/agents/evil/lookup/github/repos".into()).await;
    assert_eq!(v["problem"]["code"], "not_connected", "{v}");
    let (s, _) = get(format!("{base}/stars?parent=app")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A permission the App lacks: said, naming it; reviewers still list the people.
    g.mock.teams_forbidden.store(true, Ordering::SeqCst);
    let (_, v) = get(format!("{base}/teams?parent=app&q=co")).await;
    assert_eq!(v["problem"]["code"], "reconnect", "{v}");
    assert!(v["problem"]["message"].as_str().unwrap().contains("Members"), "{v}");
    let (_, v) = get(format!("{base}/reviewers?parent=app&q=a")).await;
    assert_eq!(ids(&v), ["ada", "grace"], "{v}");

    // GitHub's rate limit: said quietly, and nothing goes to GitHub until it resets.
    g.mock.rate_limited.store(true, Ordering::SeqCst);
    let before = g.mock.reads.lock().len();
    let (s, v) = get(format!("{base}/labels?parent=app&q=zz")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["problem"]["code"], "rate_limited", "{v}");
    assert!((1..=60).contains(&v["problem"]["retry_after"].as_u64().unwrap()), "{v}");
    assert_eq!(g.mock.reads.lock().len(), before + 1);
    let (_, v) = get(format!("{base}/branches?parent=app&q=yy")).await;
    assert_eq!(v["problem"]["code"], "rate_limited");
    let (_, v) = get(format!("{base}/milestones?parent=app&q=v")).await;
    assert_eq!(v["problem"]["code"], "rate_limited");
    assert_eq!(g.mock.reads.lock().len(), before + 1, "GitHub is left alone while it said to wait");

    // GitHub loses access to the repo: the cached rows go, and the picker asks to connect again.
    let account = json!({ "login": "acme", "id": 1, "type": "Organization" });
    let removed = json!({ "action": "removed", "installation": { "id": INSTALLATION, "account": account }, "repositories_added": [], "repositories_removed": [repo_json(&g.mock)] });
    assert_eq!(hook(&a, "installation_repositories", "pk-1", &removed, SECRET).await, StatusCode::OK);
    let (_, v) = get(format!("{base}/labels?parent=app")).await;
    assert_eq!((v["problem"]["code"].as_str(), v["items"].as_array().map(Vec::len)), (Some("reconnect"), Some(0)), "{v}");

    assert!(answers.lock().iter().all(|v| !v.contains("ghs_test")), "the installation token never leaves the server");
}

// ── either side down ──

fn mirror_state(v: &Value) -> String {
    v["github"]["mirror"]["state"].as_str().unwrap_or("").to_string()
}

/// GitHub is down when a PR's change lands: the close and the comment wait in the delivery queue
/// (retried with backoff, nothing given up) and reach GitHub once, after it's back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_landed_prs_close_reaches_github_after_an_outage() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let w = seed(tmp.path(), &format!("{}/git/acme/app.git", g.base));
    let (main_sha, feature_sha) = (rev(tmp.path(), &w, "main"), rev(tmp.path(), &w, "feature"));
    g.mock.pulls.lock().insert(3, pr_json(&g.mock, 3, "feature", &feature_sha, &main_sha));
    let a = common::app_with(|c| c.review_app_retry_ms = 50);
    enable(&a, &g, quiet_mirror());
    let a = &a;
    let repo_id = connect(a).await;
    until("PR import", || async { imported(a).await }).await;
    let c3 = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await[0]["id"].as_str().unwrap().to_string();

    g.mock.down.store(true, Ordering::SeqCst);
    a.ok("POST", &format!("/v1/code/changes/{c3}/land"), None).await;
    let pending = || {
        a.code
            .db
            .sql()
            .query_row("SELECT state, attempts FROM review_deliveries WHERE app_id = ? AND event = 'github.pr_landed' ORDER BY rowid LIMIT 1", [format!("gh_{repo_id}")], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .unwrap()
    };
    until("a few failed attempts", || async { pending().1 >= 3 }).await;
    assert_eq!(pending().0, "pending", "an outage never gives up");
    let closes = || g.mock.calls.lock().iter().filter(|(m, p, _)| m == "PATCH" && p == "/repos/acme/app/pulls/3").count();
    assert_eq!(closes(), 0);

    g.mock.down.store(false, Ordering::SeqCst);
    until("PR 3 closed", || async { closes() == 1 }).await;
    until("delivered", || async { pending().0 == "delivered" }).await;
    let calls = g.mock.calls.lock().clone();
    assert_eq!(calls.iter().filter(|(_, p, _)| p == "/repos/acme/app/issues/3/comments").count(), 1, "{calls:?}");
    assert_eq!(closes(), 1);
}

/// roll was down while GitHub moved. A push whose delivery failed is redelivered when roll asks
/// GitHub's delivery log (not one roll recorded, nor one that got through). With nothing in the
/// log, the reconcile finds the rest: a direct push to main that had stopped the mirror (merged in,
/// and the mirror resumes) and a pull request opened meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_github_did_while_roll_was_down_is_caught_up() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let a = app(None);
    enable(&a, &g, fast_mirror());
    let a = &a;
    let repo_id = connect(a).await;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Redelivery: a push GitHub couldn't deliver.
    let before = rev(home, &w, "main");
    write(&w, "docs/missed.md", "missed\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "pushed while roll was down"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let after = rev(home, &w, "main");
    let push = json!({
        "ref": "refs/heads/main", "before": before, "after": after,
        "repository": repo_json(&g.mock), "sender": { "login": "human", "id": 5 }, "installation": { "id": INSTALLATION },
    });
    let ping = json!({ "zen": "hi", "hook_id": 1 });
    assert_eq!(hook(a, "ping", "seen-1", &ping, SECRET).await, StatusCode::OK);
    *g.mock.hook_log.lock() = vec![
        json!({ "id": 13, "guid": "lost-1", "delivered_at": now, "redelivery": false, "status_code": 502, "event": "push" }),
        json!({ "id": 12, "guid": "seen-1", "delivered_at": now, "redelivery": false, "status_code": 0, "event": "ping" }),
        json!({ "id": 11, "guid": "fine-1", "delivered_at": now, "redelivery": false, "status_code": 200, "event": "push" }),
    ];
    assert_eq!(a.code.github_catch_up_now().await.unwrap(), 1);
    assert_eq!(*g.mock.redeliveries.lock(), [13]);
    // Not asked twice while its attempt hasn't shown up.
    assert_eq!(a.code.github_catch_up_now().await.unwrap(), 0);
    // GitHub's redelivery.
    assert_eq!(hook(a, "push", "lost-1", &push, SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"], after);
    g.mock.hook_log.lock().insert(0, json!({ "id": 14, "guid": "lost-1", "delivered_at": now, "redelivery": true, "status_code": 200, "event": "push" }));
    assert_eq!(a.code.github_catch_up_now().await.unwrap(), 0);
    until("mirror ok", || async { mirror_state(&a.ok("GET", "/v1/code/repos/acme/app", None).await) == "ok" }).await;

    // Reconcile. GitHub's main moves, and its push reaches roll while roll can't fetch from
    // GitHub: the mirror stops (GitHub has commits roll never saw) and the delivery fails, and
    // GitHub's log will have nothing roll can use.
    g.mock.fail_fetches.store(true, Ordering::SeqCst);
    write(&w, "docs/direct.md", "direct\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "direct push to main"]);
    git(home, &w, &["push", "-q", "origin", "main"]);
    let github_main = rev(home, &w, "main");
    let push = json!({
        "ref": "refs/heads/main", "before": after, "after": github_main,
        "repository": repo_json(&g.mock), "sender": { "login": "human", "id": 5 }, "installation": { "id": INSTALLATION },
    });
    assert_ne!(hook(a, "push", "failed-1", &push, SECRET).await, StatusCode::OK);
    until("mirror stopped", || async { mirror_state(&a.ok("GET", "/v1/code/repos/acme/app", None).await) == "paused" }).await;
    // roll lands meanwhile, and a pull request opens on GitHub with no delivery at all.
    a.ok("POST", "/v1/code/repos/acme/app/land", Some(json!({ "message": "roll side", "files": { "roll.txt": { "content": "roll\n" } } }))).await;
    let roll_head = a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].as_str().unwrap().to_string();
    git(home, &w, &["checkout", "-qb", "outage"]);
    write(&w, "outage.txt", "outage\n");
    git(home, &w, &["add", "-A"]);
    git(home, &w, &["commit", "-qm", "work during the outage"]);
    git(home, &w, &["push", "-q", "origin", "outage"]);
    git(home, &w, &["checkout", "-q", "main"]);
    g.mock.pulls.lock().insert(9, pr_json(&g.mock, 9, "outage", &rev(home, &w, "outage"), &github_main));
    g.mock.fail_fetches.store(false, Ordering::SeqCst);

    let done = a.code.github_reconcile_now(&repo_id).await.unwrap();
    assert_eq!((done.branches, done.pulls), (2, 1), "{done:?}");
    let head = a.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"].as_str().unwrap().to_string();
    let raw = a.code.store.git_object(&repo_id, &code_store::Oid(code_git::ObjectId::from_hex(head.as_bytes()).unwrap().0)).await.unwrap().unwrap().1;
    let parents: Vec<String> = code_git::parse_commit(&raw).unwrap().parents.iter().map(|p| p.to_hex()).collect();
    assert_eq!(parents, [roll_head, github_main], "GitHub's push merged into roll's mainline");
    until("the mirror caught GitHub up", || async { g.gh.ok("GET", "/v1/code/repos/acme/app", None).await["head_git"] == head.as_str() }).await;
    until("mirror ok again", || async { mirror_state(&a.ok("GET", "/v1/code/repos/acme/app", None).await) == "ok" }).await;
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    assert!(changes.as_array().unwrap().iter().any(|c| c["github_pr"] == 9 && c["state"] == "open"), "{changes}");
    // A second look finds nothing new.
    assert_eq!(a.code.github_reconcile_now(&repo_id).await.unwrap(), Default::default());
}
