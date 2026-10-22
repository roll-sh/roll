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

async fn pulls(State(m): S) -> Json<Value> {
    Json(Value::Array(m.pulls.lock().values().filter(|p| p["state"] == "open").cloned().collect()))
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

async fn count_pushes(State(m): S, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
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
        .route("/app/installations/{id}/access_tokens", post(token))
        .route("/installation/repositories", get(installation_repos))
        .route("/repos/{o}/{r}", get(repo))
        .route("/repos/{o}/{r}/branches/{b}", get(branch))
        .route("/repos/{o}/{r}/issues", get(issues))
        .route("/repos/{o}/{r}/issues/{n}", get(issue).patch(patch_issue))
        .route("/repos/{o}/{r}/issues/{n}/comments", post(comment))
        .route("/repos/{o}/{r}/pulls", get(pulls))
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

fn test_key() -> String {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    key.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string()
}

fn enable(a: &App, g: &GitHub, mirror: MirrorConfig) {
    let mut config = AppConfig::new(1, test_key(), SECRET, "roll-test");
    config.api_base = format!("{}/api", g.base);
    config.web_base = format!("{}/git", g.base);
    let client = ClientOptions { max_retries: 1, timeout: Duration::from_secs(10), ..Default::default() };
    github::enable(&a.code, config, Options { mirror, client }).unwrap();
}

fn fast_mirror() -> MirrorConfig {
    MirrorConfig {
        debounce: Duration::from_millis(100),
        max_delay: Duration::from_millis(400),
        backoff_initial: Duration::from_millis(50),
        backoff_max: Duration::from_secs(1),
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
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
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

    // Checks on a PR's change become check runs.
    let c5_id = c5["id"].as_str().unwrap();
    a.ok("POST", &format!("/v1/code/changes/{c5_id}/checks"), Some(json!({ "name": "test", "status": "running" }))).await;
    until("check run", || async { g.mock.calls.lock().iter().any(|(m, p, _)| m == "POST" && p.ends_with("/check-runs")) }).await;
    a.ok("POST", &format!("/v1/code/changes/{c5_id}/checks"), Some(json!({ "name": "test", "status": "success", "summary": "42 passed" }))).await;
    until("check run update", || async {
        g.mock.calls.lock().iter().any(|(m, p, b)| m == "PATCH" && p.ends_with("/check-runs/1") && b["conclusion"] == "success")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_pr_and_installation_lifecycles() {
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
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } })))
        .await;
    until("import", || async { a.ok("GET", "/v1/code/repos/acme/app", None).await["import"]["state"] == "done" }).await;
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
    let repo = repo_json(&g.mock);
    let inst = json!({ "id": INSTALLATION });
    let item = |a: &App| {
        let a = a.router.clone();
        async move {
            use tower::ServiceExt;
            let r = a.oneshot(axum::http::Request::get("/v1/code/repos/acme/app/work").body(axum::body::Body::empty()).unwrap()).await.unwrap();
            let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
            v.as_array().unwrap().iter().find(|w| w["source"] == "github#9").cloned()
        }
    };

    // An issue's life on GitHub: opened, edited, relabeled, closed, reopened.
    let ev = |action: &str, title: &str, state: &str, labels: &[&str]| json!({ "action": action, "issue": issue_json(9, title, state, labels), "repository": repo, "installation": inst });
    assert_eq!(hook(&a, "issues", "i-1", &ev("opened", "Slow page", "open", &["P3"]), SECRET).await, StatusCode::OK);
    let w9 = item(&a).await.expect("work item for #9");
    assert_eq!((w9["title"].as_str(), w9["priority"].as_i64(), w9["state"].as_str()), (Some("Slow page"), Some(3), Some("open")));
    assert_eq!(w9["github_url"], "https://github.com/acme/app/issues/9");
    assert_eq!(hook(&a, "issues", "i-2", &ev("edited", "Slow dashboard page", "open", &["P3"]), SECRET).await, StatusCode::OK);
    assert_eq!(item(&a).await.unwrap()["title"], "Slow dashboard page");
    assert_eq!(hook(&a, "issues", "i-3", &ev("labeled", "Slow dashboard page", "open", &["P1", "perf"]), SECRET).await, StatusCode::OK);
    let w9 = item(&a).await.unwrap();
    assert_eq!(w9["priority"], 1);
    assert!(w9["labels"].as_array().unwrap().iter().any(|l| l == "perf"), "{w9}");
    let mut closed = issue_json(9, "Slow dashboard page", "closed", &["perf"]);
    closed["state_reason"] = json!("not_planned");
    assert_eq!(hook(&a, "issues", "i-4", &json!({ "action": "closed", "issue": closed, "repository": repo, "installation": inst }), SECRET).await, StatusCode::OK);
    assert_eq!(item(&a).await.unwrap()["state"], "closed", "not planned closes, it isn't done");
    assert_eq!(hook(&a, "issues", "i-5", &ev("reopened", "Slow dashboard page", "open", &["perf"]), SECRET).await, StatusCode::OK);
    assert_eq!(item(&a).await.unwrap()["state"], "open", "reopened goes back in the queue");
    // Comments don't break anything.
    let comment = json!({ "action": "created", "issue": issue_json(9, "Slow dashboard page", "open", &[]), "comment": { "id": 1, "body": "+1", "user": { "login": "octo", "id": 9 }, "html_url": "https://github.com/c/1" }, "repository": repo, "installation": inst });
    assert!(hook(&a, "issue_comment", "i-6", &comment, SECRET).await.is_success());
    // Unknown events are acknowledged and ignored.
    assert!(hook(&a, "star", "i-7", &json!({ "action": "created", "repository": repo }), SECRET).await.is_success());

    // A PR updated on GitHub revises its change; closed without merging closes it.
    let changes = a.ok("GET", "/v1/code/repos/acme/app/changes", None).await;
    let c3 = changes.as_array().unwrap().iter().find(|c| c["github_pr"] == 3).unwrap()["id"].as_str().unwrap().to_string();
    git(home, &w, &["checkout", "-q", "feature"]);
    write(&w, "feature.txt", "feature v2\n");
    git(home, &w, &["commit", "-qam", "feature v2"]);
    git(home, &w, &["push", "-q", "origin", "feature"]);
    let v2 = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    git(home, &w, &["checkout", "-q", "main"]);
    let pr3 = pr_json(&g.mock, 3, "feature", &v2, &main_sha);
    g.mock.pulls.lock().insert(3, pr3.clone());
    let sync = json!({ "action": "synchronize", "number": 3, "pull_request": pr3, "repository": repo, "installation": inst });
    assert_eq!(hook(&a, "pull_request", "p-1", &sync, SECRET).await, StatusCode::OK);
    until("revision 2", || async { a.ok("GET", &format!("/v1/code/changes/{c3}"), None).await["revision"] == 2 }).await;
    let mut closed_pr = pr_json(&g.mock, 3, "feature", &v2, &main_sha);
    closed_pr["state"] = json!("closed");
    let close = json!({ "action": "closed", "number": 3, "pull_request": closed_pr, "repository": repo, "installation": inst });
    assert_eq!(hook(&a, "pull_request", "p-2", &close, SECRET).await, StatusCode::OK);
    until("change closed", || async { a.ok("GET", &format!("/v1/code/changes/{c3}"), None).await["state"] == "closed" }).await;
    assert!(!g.mock.calls.lock().iter().any(|(m, p, _)| m == "PATCH" && p == "/repos/acme/app/pulls/3"), "a close from GitHub isn't echoed back");

    // Uninstalling the App disconnects the repo and pauses its mirror; reinstalling reconnects.
    let uninstall = json!({ "action": "deleted", "installation": { "id": INSTALLATION, "account": { "login": "acme", "id": 1, "type": "Organization" } } });
    assert_eq!(hook(&a, "installation", "x-1", &uninstall, SECRET).await, StatusCode::OK);
    let gh = a.ok("GET", "/v1/code/repos/acme/app", None).await["github"].clone();
    assert_eq!(gh["connected"], false, "{gh}");
    assert_eq!(gh["mirror"]["state"], "paused", "{gh}");
    assert!(gh["mirror"]["error"].as_str().unwrap().contains("uninstalled"), "{gh}");
    let reinstall = json!({ "action": "created", "installation": { "id": INSTALLATION, "account": { "login": "acme", "id": 1, "type": "Organization" } }, "repositories": [] });
    assert_eq!(hook(&a, "installation", "x-2", &reinstall, SECRET).await, StatusCode::OK);
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/app", None).await["github"]["connected"], true);

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

/// The App's installations span every tenant, so only the admin lists or connects them; and a
/// pull request from a fork becomes an untrusted change (no automatic checks or previews).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connecting_needs_the_admin_and_fork_prs_are_untrusted() {
    let g = github_mock().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = seed(home, &format!("{}/git/acme/app.git", g.base));
    let main_sha = git(home, &w, &["rev-parse", "main"]).trim().to_string();
    let feature_sha = git(home, &w, &["rev-parse", "feature"]).trim().to_string();
    let a = app(Some("admin-secret"));
    enable(&a, &g, fast_mirror());
    let admin = Some("admin-secret");
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), admin).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent" })), admin).await;
    let owner = t["token"].as_str().map(str::to_string);

    let (s, _) = a.req("GET", "/v1/code/github/installations", None, owner.as_deref()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "an owner token can't list every tenant's installations");
    let (s, _) = a.req("GET", "/v1/code/github/installations", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let connect = json!({ "owner": "acme", "name": "app", "source": { "github": { "installation_id": INSTALLATION, "owner": "acme", "repo": "app" } } });
    let (s, body) = a.req("POST", "/v1/code/repos", Some(connect.clone()), owner.as_deref()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    let (s, body) = a.req("POST", "/v1/code/repos", Some(connect), admin).await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    until("import", || async { a.req("GET", "/v1/code/repos/acme/app", None, admin).await.1["import"]["state"] == "done" }).await;

    // PR 8 comes from eve's fork; its head commit is one the base repo has.
    let mut pr = pr_json(&g.mock, 8, "feature", &feature_sha, &main_sha);
    pr["head"]["repo"] = json!({
        "id": 99, "name": "app", "full_name": "eve/app", "private": false, "default_branch": "main", "fork": true,
        "owner": { "login": "eve", "id": 99, "type": "User" },
        "clone_url": format!("{}/git/acme/app.git", g.base), "html_url": "https://github.com/eve/app",
    });
    g.mock.pulls.lock().insert(8, pr.clone());
    let ev = json!({ "action": "opened", "number": 8, "pull_request": pr, "repository": repo_json(&g.mock), "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "pull_request", "fork-1", &ev, SECRET).await, StatusCode::OK);
    let changes = a.req("GET", "/v1/code/repos/acme/app/changes", None, admin).await.1;
    let fork = changes.as_array().unwrap().iter().find(|c| c["github_pr"] == 8).cloned().expect("a change for the fork's PR");
    assert_eq!(fork["trusted"], false, "{fork}");
    // Big payloads (GitHub allows 25 MB) aren't cut off at axum's 2 MB default.
    let big = json!({ "zen": "x".repeat(3 << 20), "hook_id": 1, "installation": { "id": INSTALLATION } });
    assert_eq!(hook(&a, "ping", "big-1", &big, SECRET).await, StatusCode::OK);
    assert!(fork["branch"].is_null(), "a fork's branch isn't one of ours: {fork}");
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
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
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
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
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
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
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
    until("PR import", || async { !a.ok("GET", "/v1/code/repos/acme/app/changes", None).await.as_array().unwrap().is_empty() }).await;
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
