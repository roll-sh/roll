//! code-github against a mock GitHub API (axum) and recorded-shape webhook payloads.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use code_github::auth::{Claims, unix_now};
use code_github::mapping::{issue_draft, pull_request_draft, push_draft};
use code_github::model::{CheckConclusion, CheckOutput, CheckRunRequest, CheckStatus};
use code_github::webhook::{
    CommentAction, InstallationAction, IssueAction, PullRequestAction, RepositoriesAction, sign,
};
use code_github::{
    AppConfig, ClientOptions, DraftState, Error, Event, GitHub, PrState, verify_signature,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// A throwaway RSA key generated per test run, never written anywhere.
fn test_key() -> &'static (String, String) {
    static KEY: OnceLock<(String, String)> = OnceLock::new();
    KEY.get_or_init(|| {
        use rsa::pkcs1::EncodeRsaPrivateKey;
        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        let private = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let public = private.to_public_key();
        (
            private.to_pkcs1_pem(LineEnding::LF).unwrap().to_string(),
            public.to_public_key_pem(LineEnding::LF).unwrap(),
        )
    })
}

const APP_ID: u64 = 901234;
const INSTALLATION: u64 = 61234567;

/// A canned response: status, headers, JSON body.
type Scripted = (u16, Vec<(&'static str, String)>, Value);

#[derive(Default)]
struct Mock {
    base: Mutex<String>,
    mints: AtomicU32,
    /// Path → remaining scripted failures, served before the real answer.
    failures: Mutex<HashMap<String, Vec<Scripted>>>,
    /// Bearer tokens seen per path.
    tokens: Mutex<Vec<(String, String)>>,
    revoked: Mutex<Vec<String>>,
}

type S = State<Arc<Mock>>;

fn bearer(h: &HeaderMap) -> String {
    h.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned()
}

fn check_jwt(token: &str) -> Result<Claims, StatusCode> {
    let key = DecodingKey::from_rsa_pem(test_key().1.as_bytes()).unwrap();
    let mut v = Validation::new(Algorithm::RS256);
    v.set_required_spec_claims(&["exp", "iat", "iss"]);
    let claims = jsonwebtoken::decode::<Claims>(token, &key, &v)
        .map_err(|_| StatusCode::UNAUTHORIZED)?
        .claims;
    if claims.iss != APP_ID.to_string() || claims.exp - claims.iat > 600 || claims.iat > unix_now()
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(claims)
}

/// Serve a scripted failure for `path` if one is queued, and check the installation token.
fn gate(m: &Mock, path: &str, h: &HeaderMap) -> Option<Response> {
    let token = bearer(h);
    m.tokens.lock().push((path.to_owned(), token.clone()));
    if let Some(queue) = m.failures.lock().get_mut(path)
        && !queue.is_empty()
    {
        let (status, headers, body) = queue.remove(0);
        let mut res = (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
        for (k, v) in headers {
            res.headers_mut().insert(k, v.parse().unwrap());
        }
        return Some(res);
    }
    if !token.starts_with("ghs_") || m.revoked.lock().contains(&token) {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"message": "Bad credentials"})),
            )
                .into_response(),
        );
    }
    None
}

fn rate_headers(mut res: Response, remaining: u64) -> Response {
    let h = res.headers_mut();
    h.insert("x-ratelimit-limit", "5000".parse().unwrap());
    h.insert(
        "x-ratelimit-remaining",
        remaining.to_string().parse().unwrap(),
    );
    h.insert(
        "x-ratelimit-reset",
        (unix_now() + 3600).to_string().parse().unwrap(),
    );
    res
}

async fn access_tokens(State(m): S, Path(id): Path<u64>, h: HeaderMap) -> Response {
    if let Err(s) = check_jwt(&bearer(&h)) {
        return (
            s,
            Json(json!({"message": "A JSON web token could not be decoded"})),
        )
            .into_response();
    }
    if id != INSTALLATION {
        return (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"}))).into_response();
    }
    let n = m.mints.fetch_add(1, Ordering::SeqCst) + 1;
    let expires = chrono::DateTime::from_timestamp((unix_now() + 3600) as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    (
        StatusCode::CREATED,
        Json(json!({"token": format!("ghs_test{n}"), "expires_at": expires})),
    )
        .into_response()
}

async fn installations(h: HeaderMap) -> Response {
    if check_jwt(&bearer(&h)).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!([{"id": INSTALLATION, "account": {"login": "acme", "id": 2002, "type": "Organization"}, "app_slug": "roll-code"}])).into_response()
}

async fn installation_repos(
    State(m): S,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = gate(&m, "/installation/repositories", &h) {
        return r;
    }
    let base = m.base.lock().clone();
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let repo = |id: u64, name: &str| json!({"id": id, "name": name, "full_name": format!("acme/{name}"), "private": true, "default_branch": "main", "clone_url": format!("https://github.com/acme/{name}.git")});
    let mut res = if page == 1 {
        Json(json!({"total_count": 3, "repositories": [repo(1, "api"), repo(2, "web")]}))
            .into_response()
    } else {
        Json(json!({"total_count": 3, "repositories": [repo(3, "infra")]})).into_response()
    };
    if page == 1 {
        let link = format!(
            r#"<{base}/installation/repositories?per_page=100&page=2>; rel="next", <{base}/installation/repositories?per_page=100&page=2>; rel="last""#
        );
        res.headers_mut().insert("link", link.parse().unwrap());
    }
    rate_headers(res, 4990)
}

async fn repo(State(m): S, Path((o, r)): Path<(String, String)>, h: HeaderMap) -> Response {
    let path = format!("/repos/{o}/{r}");
    if let Some(res) = gate(&m, &path, &h) {
        return res;
    }
    rate_headers(
        Json(json!({"id": 70001, "name": r, "full_name": format!("{o}/{r}"), "private": true, "default_branch": "trunk", "clone_url": format!("https://github.com/{o}/{r}.git")})).into_response(),
        4321,
    )
}

async fn issues(
    State(m): S,
    Path((o, r)): Path<(String, String)>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(res) = gate(&m, "/issues", &h) {
        return res;
    }
    assert_eq!(q.get("state").map(String::as_str), Some("all"));
    let base = m.base.lock().clone();
    let page: u32 = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let issue = |n: u64, pr: bool| {
        let mut i = json!({"number": n, "title": format!("issue {n}"), "state": "open", "labels": [{"name": "agent"}], "body": "b"});
        if pr {
            i["pull_request"] = json!({"url": "x"});
        }
        i
    };
    match page {
        1 => {
            let mut res = Json(json!([issue(1, false), issue(2, true)])).into_response();
            let link = format!(r#"<{base}/repos/{o}/{r}/issues?state=all&page=2>; rel="next""#);
            res.headers_mut().insert("link", link.parse().unwrap());
            res
        }
        _ => Json(json!([issue(3, false)])).into_response(),
    }
}

async fn comment(
    State(m): S,
    Path((_o, _r, n)): Path<(String, String, u64)>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(res) = gate(&m, "/comments", &h) {
        return res;
    }
    (
        StatusCode::CREATED,
        Json(json!({"id": 1000 + n, "body": body["body"], "user": {"login": "roll-code[bot]"}})),
    )
        .into_response()
}

async fn close_issue(
    State(m): S,
    Path((_o, _r, n)): Path<(String, String, u64)>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(res) = gate(&m, "/close", &h) {
        return res;
    }
    Json(json!({"number": n, "title": "t", "state": body["state"], "state_reason": body["state_reason"]})).into_response()
}

async fn labels(State(m): S, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(res) = gate(&m, "/labels", &h) {
        return res;
    }
    let names: Vec<Value> = body["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| json!({"name": l}))
        .collect();
    Json(Value::Array(names)).into_response()
}

async fn check_run(State(m): S, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(res) = gate(&m, "/check-runs", &h) {
        return res;
    }
    assert!(body.get("conclusion").is_none() || body["status"] == "completed");
    (StatusCode::CREATED, Json(json!({"id": 77, "name": body["name"], "head_sha": body["head_sha"], "status": body["status"]}))).into_response()
}

async fn update_check_run(
    State(m): S,
    Path((_o, _r, id)): Path<(String, String, u64)>,
    h: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if let Some(res) = gate(&m, "/check-runs", &h) {
        return res;
    }
    assert!(
        body.get("name").is_none(),
        "unset fields must be left out of a PATCH"
    );
    Json(json!({"id": id, "name": "test", "head_sha": "abc", "status": body["status"], "conclusion": body["conclusion"]})).into_response()
}

async fn branch(
    State(m): S,
    Path((_o, r, b)): Path<(String, String, String)>,
    h: HeaderMap,
) -> Response {
    if let Some(res) = gate(&m, "/branch", &h) {
        return res;
    }
    Json(json!({"name": b, "commit": {"sha": "0d1a26e67d8f5eaf1f6ba5c57fc3c7d91ac0fd1c"}, "protected": r != "open"})).into_response()
}

async fn protection(
    State(m): S,
    Path((_o, r, _b)): Path<(String, String, String)>,
    h: HeaderMap,
) -> Response {
    if let Some(res) = gate(&m, "/protection", &h) {
        return res;
    }
    match r.as_str() {
        "rollonly" => Json(json!({"restrictions": {"users": [], "teams": [], "apps": [{"slug": "roll-code"}]}, "allow_force_pushes": {"enabled": false}})).into_response(),
        "shared" => Json(json!({"restrictions": {"users": [{"login": "octocat"}], "teams": [], "apps": [{"slug": "roll-code"}]}})).into_response(),
        "noperm" => (StatusCode::FORBIDDEN, Json(json!({"message": "Resource not accessible by integration"}))).into_response(),
        _ => Json(json!({"required_pull_request_reviews": {}, "allow_force_pushes": {"enabled": true}})).into_response(),
    }
}

/// GitHub's GraphQL endpoint: two pages of review threads; resolve/unresolve mutations; an
/// `errors` answer for an unknown thread.
async fn graphql(State(m): S, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Some(r) = gate(&m, "/graphql", &h) {
        return r;
    }
    let q = body["query"].as_str().unwrap_or("");
    let vars = &body["variables"];
    if q.contains("reviewThreads") {
        assert_eq!((vars["owner"].as_str(), vars["repo"].as_str(), vars["number"].as_u64()), (Some("acme"), Some("app"), Some(3)));
        let page = |ids: &[(&str, u64)], next: Option<&str>| {
            json!({ "data": { "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next },
                "nodes": ids.iter().map(|(id, c)| json!({ "id": id, "comments": { "nodes": [{ "databaseId": c }] } })).collect::<Vec<_>>(),
            } } } } })
        };
        return Json(match vars["after"].as_str() {
            None => page(&[("PRRT_a", 10), ("PRRT_b", 11)], Some("c1")),
            Some("c1") => page(&[("PRRT_c", 12)], None),
            Some(_) => page(&[], None),
        })
        .into_response();
    }
    let id = vars["id"].as_str().unwrap_or("");
    if id == "PRRT_missing" {
        return Json(json!({ "data": null, "errors": [{ "message": "Could not resolve to a node with the global id of 'PRRT_missing'" }] })).into_response();
    }
    m.tokens.lock().push(("graphql-mutation".into(), q.to_string()));
    if q.contains("unresolveReviewThread") {
        return Json(json!({ "data": { "unresolveReviewThread": { "thread": { "id": id, "isResolved": false } } } })).into_response();
    }
    Json(json!({ "data": { "resolveReviewThread": { "thread": { "id": id, "isResolved": true } } } })).into_response()
}

async fn serve() -> (Arc<Mock>, String) {
    let m = Arc::new(Mock::default());
    let app = Router::new()
        .route("/app/installations", get(installations))
        .route("/app/installations/{id}/access_tokens", post(access_tokens))
        .route("/installation/repositories", get(installation_repos))
        .route("/repos/{o}/{r}", get(repo))
        .route("/repos/{o}/{r}/issues", get(issues))
        .route("/repos/{o}/{r}/issues/{n}", patch(close_issue))
        .route("/repos/{o}/{r}/issues/{n}/comments", post(comment))
        .route(
            "/repos/{o}/{r}/issues/{n}/labels",
            axum::routing::put(labels),
        )
        .route("/repos/{o}/{r}/check-runs", post(check_run))
        .route("/repos/{o}/{r}/check-runs/{id}", patch(update_check_run))
        .route("/repos/{o}/{r}/branches/{b}", get(branch))
        .route("/repos/{o}/{r}/branches/{b}/protection", get(protection))
        .route("/graphql", post(graphql))
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    *m.base.lock() = base.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (m, base)
}

fn client(base: &str) -> GitHub {
    let mut config = AppConfig::new(APP_ID, test_key().0.clone(), "whsec", "roll-code");
    config.api_base = base.into();
    let opts = ClientOptions {
        max_backoff: Duration::from_secs(3),
        server_error_backoff: Duration::from_millis(10),
        secondary_backoff: Duration::from_millis(20),
        ..Default::default()
    };
    GitHub::with_options(config, opts).unwrap()
}

#[test]
fn app_jwt_claims() {
    let mut config = AppConfig::new(APP_ID, test_key().0.clone(), "s", "roll-code");
    config.api_base = "http://unused".into();
    let gh = GitHub::new(config).unwrap();
    let now = unix_now();
    let claims = check_jwt(&gh.auth().jwt_at(now).unwrap()).unwrap();
    assert_eq!(claims.iat, now - 60);
    assert!(claims.exp <= now + 600);
    assert_eq!(claims.iss, "901234");
    assert!(matches!(
        GitHub::new(AppConfig::new(1, "not a key", "s", "x")),
        Err(Error::Config(_))
    ));
}

#[tokio::test]
async fn installation_tokens_are_cached_and_reminted_when_revoked() {
    let (m, base) = serve().await;
    let gh = client(&base);
    let installs = gh.list_installations().await.unwrap();
    assert_eq!(installs[0].id, INSTALLATION);

    gh.repository(INSTALLATION, "acme", "api").await.unwrap();
    let repo = gh.repository(INSTALLATION, "acme", "api").await.unwrap();
    assert_eq!(repo.default_branch.as_deref(), Some("trunk"));
    assert_eq!(
        m.mints.load(Ordering::SeqCst),
        1,
        "second call reuses the cached token"
    );

    // GitHub revokes the token: the client mints a new one and retries.
    m.revoked.lock().push("ghs_test1".into());
    gh.repository(INSTALLATION, "acme", "api").await.unwrap();
    assert_eq!(m.mints.load(Ordering::SeqCst), 2);

    let creds = gh
        .git_credentials(INSTALLATION, "acme", "api")
        .await
        .unwrap();
    assert_eq!(creds.url, "https://github.com/acme/api.git");
    assert_eq!(creds.username, "x-access-token");
    assert_eq!(creds.password, "ghs_test2");
    assert!(creds.expires_at > unix_now() + 3000);

    let rate = gh.rate_limit(Some(INSTALLATION)).unwrap();
    assert_eq!((rate.limit, rate.remaining), (Some(5000), Some(4321)));
    assert!(matches!(gh.installation_token(999).await, Err(e) if e.is_not_found()));
}

#[tokio::test]
async fn pagination() {
    let (_m, base) = serve().await;
    let gh = client(&base);
    let repos = gh.installation_repositories(INSTALLATION).await.unwrap();
    assert_eq!(
        repos
            .iter()
            .map(|r| r.full_name.as_str())
            .collect::<Vec<_>>(),
        ["acme/api", "acme/web", "acme/infra"]
    );
    let issues = gh
        .list_issues(INSTALLATION, "acme", "api", Some("2026-10-01T00:00:00Z"))
        .await
        .unwrap();
    assert_eq!(
        issues.iter().map(|i| i.number).collect::<Vec<_>>(),
        [1, 3],
        "pull requests are filtered out"
    );
}

#[tokio::test]
async fn writes() {
    let (_m, base) = serve().await;
    let gh = client(&base);
    let c = gh
        .create_comment(INSTALLATION, "acme", "api", 12, "Landed as cs_1a2b3c")
        .await
        .unwrap();
    assert_eq!(c.body.as_deref(), Some("Landed as cs_1a2b3c"));
    let i = gh
        .close_issue(INSTALLATION, "acme", "api", 12, Some("completed"))
        .await
        .unwrap();
    assert_eq!(
        (i.state.as_str(), i.state_reason.as_deref()),
        ("closed", Some("completed"))
    );
    let l = gh
        .set_labels(
            INSTALLATION,
            "acme",
            "api",
            12,
            &["agent".into(), "p1".into()],
        )
        .await
        .unwrap();
    assert_eq!(l.len(), 2);

    let run = gh
        .create_check_run(
            INSTALLATION,
            "acme",
            "api",
            &CheckRunRequest {
                name: Some("test".into()),
                head_sha: Some("abc".into()),
                status: Some(CheckStatus::InProgress),
                details_url: Some("https://code.roll.sh/acme/api/changes/chg_1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!((run.id, run.status), (77, CheckStatus::InProgress));
    let done = gh
        .update_check_run(
            INSTALLATION,
            "acme",
            "api",
            run.id,
            &CheckRunRequest {
                status: Some(CheckStatus::Completed),
                conclusion: Some(CheckConclusion::Success),
                output: Some(CheckOutput {
                    title: "ok".into(),
                    summary: "412 passed".into(),
                    text: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(done.conclusion, Some(CheckConclusion::Success));
}

#[tokio::test]
async fn branch_protection() {
    let (_m, base) = serve().await;
    let gh = client(&base);
    let p = gh
        .branch_protection(INSTALLATION, "acme", "open", "main")
        .await
        .unwrap();
    assert!(!p.protected && p.roll_can_push);
    let p = gh
        .branch_protection(INSTALLATION, "acme", "rollonly", "main")
        .await
        .unwrap();
    assert!(
        p.protected
            && p.details
            && p.restricts_pushes
            && p.only_roll_pushes
            && p.roll_can_push
            && !p.allows_force_pushes
    );
    let p = gh
        .branch_protection(INSTALLATION, "acme", "shared", "main")
        .await
        .unwrap();
    assert!(p.roll_can_push && !p.only_roll_pushes);
    let p = gh
        .branch_protection(INSTALLATION, "acme", "reviews", "release-1.0")
        .await
        .unwrap();
    assert!(
        p.protected && p.details && !p.restricts_pushes && p.roll_can_push && p.allows_force_pushes
    );
    let p = gh
        .branch_protection(INSTALLATION, "acme", "noperm", "main")
        .await
        .unwrap();
    assert!(p.protected && !p.details && !p.roll_can_push);
}

#[tokio::test]
async fn rate_limits_and_retries() {
    let (m, base) = serve().await;
    let gh = client(&base);
    let fail = |path: &str, status: u16, headers: Vec<(&'static str, String)>, msg: &str| {
        m.failures.lock().entry(path.into()).or_default().push((
            status,
            headers,
            json!({"message": msg}),
        ));
    };

    // Secondary limit with retry-after, then a primary limit that resets in a moment, then a 502.
    fail(
        "/repos/acme/api",
        429,
        vec![("retry-after", "0".into())],
        "slow down",
    );
    fail(
        "/repos/acme/api",
        403,
        vec![],
        "You have exceeded a secondary rate limit. Please wait a few minutes.",
    );
    fail("/repos/acme/api", 502, vec![], "Bad Gateway");
    let t = Instant::now();
    gh.repository(INSTALLATION, "acme", "api").await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2));

    // A primary limit that resets in an hour fails fast instead of sleeping.
    let reset = unix_now() + 3600;
    fail(
        "/repos/acme/api",
        403,
        vec![
            ("x-ratelimit-remaining", "0".into()),
            ("x-ratelimit-reset", reset.to_string()),
        ],
        "API rate limit exceeded",
    );
    let t = Instant::now();
    let e = gh
        .repository(INSTALLATION, "acme", "api")
        .await
        .unwrap_err();
    assert!(
        matches!(e, Error::RateLimited { reset: r } if r >= reset - 1),
        "{e}"
    );
    assert_eq!(
        gh.rate_limit(Some(INSTALLATION)).unwrap().remaining,
        Some(0)
    );
    // The budget is known to be gone: the next call doesn't even reach GitHub.
    let before = m.tokens.lock().len();
    assert!(matches!(
        gh.repository(INSTALLATION, "acme", "api").await,
        Err(Error::RateLimited { .. })
    ));
    assert_eq!(m.tokens.lock().len(), before);
    assert!(t.elapsed() < Duration::from_secs(2));

    // A plain 403 is an error, not a rate limit.
    let gh = client(&base);
    fail(
        "/repos/acme/api",
        403,
        vec![],
        "Resource not accessible by integration",
    );
    let e = gh
        .repository(INSTALLATION, "acme", "api")
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(403));

    // Retries run out.
    for _ in 0..4 {
        fail("/repos/acme/api", 503, vec![], "unavailable");
    }
    assert_eq!(
        gh.repository(INSTALLATION, "acme", "api")
            .await
            .unwrap_err()
            .status(),
        Some(503)
    );
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn webhook_signature() {
    let body = fixture("push");
    let sig = sign(b"whsec", &body);
    assert!(verify_signature(b"whsec", &body, Some(&sig)));
    let mut tampered = body.clone();
    tampered[10] ^= 1;
    assert!(!verify_signature(b"whsec", &tampered, Some(&sig)));
    assert!(!verify_signature(b"other", &body, Some(&sig)));
}

#[test]
fn webhook_events() {
    let parse = |event: &str, name: &str| Event::parse(event, &fixture(name)).unwrap();

    assert!(matches!(
        parse("ping", "ping"),
        Event::Ping {
            hook_id: Some(512345678),
            ..
        }
    ));

    let Event::Installation {
        action,
        installation,
        repositories,
    } = parse("installation", "installation_created")
    else {
        panic!()
    };
    assert_eq!(action, InstallationAction::Created);
    assert_eq!(installation.account.unwrap().login, "acme");
    assert_eq!(repositories.len(), 2);

    let e = parse(
        "installation_repositories",
        "installation_repositories_added",
    );
    assert_eq!(e.installation_id(), Some(INSTALLATION));
    let Event::InstallationRepositories {
        action,
        added,
        removed,
        ..
    } = e
    else {
        panic!()
    };
    assert_eq!(
        (action, added[0].full_name.as_str(), removed.len()),
        (RepositoriesAction::Added, "acme/infra", 0)
    );

    let e = parse("push", "push");
    assert_eq!(e.installation_id(), Some(INSTALLATION));
    assert_eq!(e.repository().unwrap().full_name, "acme/api");
    let Event::Push(push) = e else { panic!() };
    let d = push_draft(&push);
    assert_eq!(d.branch(), Some("main"));
    assert!(d.default_branch && !d.forced);
    assert_eq!(
        d.before.as_deref(),
        Some("6113728f27ae82c7b1a177c8d03f9e96e0adf246")
    );
    assert_eq!(d.commits[0].message, "Fix the retry loop\n\nCloses #12");
    assert_eq!(
        d.commits[0].author_email.as_deref(),
        Some("octocat@example.com")
    );
    assert_eq!(d.sender.as_deref(), Some("octocat"));

    let e = parse("push", "push_branch_created");
    assert_eq!(
        e.sender().unwrap().login,
        "roll-code[bot]",
        "roll's own pushes are recognisable"
    );
    let Event::Push(push) = e else { panic!() };
    let d = push_draft(&push);
    assert_eq!(
        (d.before.as_deref(), d.branch(), d.default_branch),
        (None, Some("agent/fix-12"), false)
    );

    let Event::PullRequest(pr) = parse("pull_request", "pull_request_opened") else {
        panic!()
    };
    assert_eq!((pr.action, pr.number), (PullRequestAction::Opened, 42));
    let d = pull_request_draft(&pr.pull_request);
    assert_eq!(d.state, PrState::Open);
    assert_eq!(d.fork.as_deref(), Some("contributor/api"));
    assert_eq!(
        d.head_clone_url.as_deref(),
        Some("https://github.com/contributor/api.git")
    );
    assert_eq!(
        (d.head_ref.as_str(), d.base_ref.as_str()),
        ("rate-limit", "main")
    );
    assert_eq!(d.head_sha, "b7e23ec29af22b0b4e41da31e868d57226121c84");

    let Event::PullRequest(pr) = parse("pull_request", "pull_request_closed_merged") else {
        panic!()
    };
    let d = pull_request_draft(&pr.pull_request);
    assert_eq!(
        (pr.action, d.state, d.fork, d.body.as_str()),
        (PullRequestAction::Closed, PrState::Merged, None, "")
    );

    assert_eq!(
        parse("pull_request", "pull_request_labeled"),
        Event::Ignored {
            event: "pull_request".into(),
            action: Some("labeled".into())
        }
    );

    let Event::Issues(i) = parse("issues", "issues_labeled") else {
        panic!()
    };
    assert_eq!(i.action, IssueAction::Labeled);
    assert_eq!(i.label.unwrap().name, "agent");
    let d = issue_draft(&i.issue);
    assert_eq!(d.external_id, "github#12");
    assert_eq!(d.labels, ["bug", "agent"]);
    assert_eq!(d.state, DraftState::Open);
    assert_eq!(
        d.url.as_deref(),
        Some("https://github.com/acme/api/issues/12")
    );

    let Event::Issues(i) = parse("issues", "issues_closed") else {
        panic!()
    };
    let d = issue_draft(&i.issue);
    assert_eq!((d.state, d.body.as_str()), (DraftState::NotPlanned, ""));

    let Event::IssueComment(c) = parse("issue_comment", "issue_comment_created") else {
        panic!()
    };
    assert_eq!(c.action, CommentAction::Created);
    assert!(c.issue.is_pull_request());
    assert_eq!(c.comment.body.as_deref(), Some("/roll land"));

    assert_eq!(
        parse("check_suite", "check_suite_requested"),
        Event::Ignored {
            event: "check_suite".into(),
            action: Some("requested".into())
        }
    );
    assert!(Event::parse("push", b"not json").is_err());
    assert!(Event::parse("push", br#"{"ref": 1}"#).is_err());
}

#[tokio::test]
async fn review_threads_over_graphql() {
    let (m, base) = serve().await;
    let gh = client(&base);
    // Found on the second page by its first comment's REST id; absent ones are None.
    assert_eq!(gh.review_thread_node(INSTALLATION, "acme", "app", 3, 12).await.unwrap().as_deref(), Some("PRRT_c"));
    assert_eq!(gh.review_thread_node(INSTALLATION, "acme", "app", 3, 10).await.unwrap().as_deref(), Some("PRRT_a"));
    assert_eq!(gh.review_thread_node(INSTALLATION, "acme", "app", 3, 99).await.unwrap(), None);
    assert!(gh.set_review_thread_resolved(INSTALLATION, "PRRT_a", true).await.unwrap());
    assert!(!gh.set_review_thread_resolved(INSTALLATION, "PRRT_a", false).await.unwrap());
    let mutations: Vec<String> = m.tokens.lock().iter().filter(|(p, _)| p == "graphql-mutation").map(|(_, q)| q.clone()).collect();
    assert!(mutations[0].contains("resolveReviewThread") && !mutations[0].contains("unresolve"));
    assert!(mutations[1].contains("unresolveReviewThread"));
    // GraphQL errors (HTTP 200 with `errors`) are errors.
    let e = gh.set_review_thread_resolved(INSTALLATION, "PRRT_missing", true).await.unwrap_err();
    assert!(e.to_string().contains("Could not resolve"), "{e}");
    // With the installation token, like REST.
    assert!(m.tokens.lock().iter().filter(|(p, _)| p == "/graphql").all(|(_, t)| t.starts_with("ghs_")));
}
