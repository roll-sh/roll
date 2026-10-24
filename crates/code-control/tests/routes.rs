//! Every route `api::router` declares (but the review and AI routes, in review.rs and
//! reviewer.rs) against every kind of caller, and the statuses (docs/code/http-api.md) that no
//! other file asserts. Errors are always JSON `{error}`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{App, app};
use serde_json::{Value, json};
use std::collections::HashMap;

const ADMIN: &str = "admin-secret";
const OK: StatusCode = StatusCode::OK;
const NOT_FOUND: StatusCode = StatusCode::NOT_FOUND;
const UNAUTHORIZED: StatusCode = StatusCode::UNAUTHORIZED;
const FORBIDDEN: StatusCode = StatusCode::FORBIDDEN;

/// Send a request and return its status. Error answers must be JSON `{error: string}`. Success
/// bodies aren't read (`/events` never ends).
async fn call(a: &App, method: &str, path: &str, body: Option<&Value>, token: Option<&str>) -> StatusCode {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = a.send(req).await;
    let s = resp.status();
    if s.is_client_error() || s.is_server_error() {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("{method} {path}: a {s} answers JSON, got {:?}", String::from_utf8_lossy(&bytes)));
        assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()), "{method} {path}: {s} without an error message: {v}");
    }
    s
}

/// A token-mode daemon: `acme` owns `pub` (public) and `priv` (private), each with a landing, a
/// change, a work item and a webhook; `other` is another owner. Tokens: acme's (`ci`), acme's
/// agent (`agent:bot`), other's, a revoked one of acme's, and two of acme's review apps (installed
/// on `priv`, and only on `pub`). Ids are in `ids`, keyed `{repo}.change` etc.
struct World {
    a: App,
    acme: String,
    agent: String,
    other: String,
    revoked: String,
    app_in: String,
    app_out: String,
    ids: HashMap<String, String>,
}

async fn world() -> World {
    let a = app(Some(ADMIN));
    let admin = Some(ADMIN);
    let token = |owner: &'static str, name: &'static str| {
        let a = &a;
        async move {
            let (s, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": name })), admin).await;
            assert_eq!(s, StatusCode::CREATED, "{t}");
            (t["token"].as_str().unwrap().to_string(), t["id"].as_str().unwrap().to_string())
        }
    };
    for owner in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": owner, "kind": "org" })), admin).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (acme, acme_id) = token("acme", "ci").await;
    let (agent, _) = token("acme", "agent:bot").await;
    let (other, _) = token("other", "ci").await;
    let (revoked, revoked_id) = token("acme", "old").await;
    let (s, _) = a.req("DELETE", &format!("/v1/code/owners/acme/tokens/{revoked_id}"), None, admin).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let mut ids = HashMap::from([("token".to_string(), acme_id)]);
    for (name, visibility) in [("pub", "public"), ("priv", "private")] {
        let p = format!("/v1/code/repos/acme/{name}");
        let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": visibility })), admin).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let files = json!({ "README.md": { "content": "# hi\n" }, "a.txt": { "content": "a\n" }, "docs/x.md": { "content": "x\n" } });
        let (s, v) = a.req("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": files })), admin).await;
        assert_eq!(s, OK, "{v}");
        ids.insert(format!("{name}.cs"), v["changeset"].as_str().unwrap().to_string());
        let (_, c) = a.req("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "c.txt": { "content": "c\n" } } })), admin).await;
        ids.insert(format!("{name}.change"), c["id"].as_str().unwrap().to_string());
        ids.insert(format!("{name}.cn"), c["number"].to_string());
        let (_, w) = a.req("POST", &format!("{p}/work"), Some(json!({ "title": "w" })), admin).await;
        ids.insert(format!("{name}.work"), w["id"].as_str().unwrap().to_string());
        ids.insert(format!("{name}.wn"), w["number"].to_string());
        let (s, h) = a.req("POST", &format!("{p}/webhooks"), Some(json!({ "url": "http://127.0.0.1:9/hook" })), admin).await;
        assert_eq!(s, StatusCode::CREATED, "{h}");
        ids.insert(format!("{name}.hook"), h["webhook"]["id"].as_str().unwrap().to_string());
        ids.insert(format!("{name}.delivery"), h["ping"].as_str().unwrap().to_string());
    }
    // Review apps of acme: one installed on the private repo, one only on the public one.
    let app_token = |installed: &'static str| {
        let a = &a;
        async move {
            let body = json!({ "name": format!("bot-{installed}"), "webhook_url": "https://example.com/r", "repos": [installed] });
            let (s, v) = a.req("POST", "/v1/code/owners/acme/review-apps", Some(body), admin).await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
            v["token"].as_str().unwrap().to_string()
        }
    };
    let app_in = app_token("priv").await;
    let app_out = app_token("pub").await;
    World { a, acme, agent, other, revoked, app_in, app_out, ids }
}

impl World {
    /// `path` with `{r}` (the repo's name) and `{change}`, `{cn}`, `{work}`, `{wn}`, `{hook}`,
    /// `{delivery}`, `{cs}` (that repo's), `{token}` filled in.
    fn path(&self, repo: &str, path: &str) -> String {
        let mut p = path.replace("{r}", repo);
        for k in ["change", "cn", "work", "wn", "hook", "delivery", "cs"] {
            p = p.replace(&format!("{{{k}}}"), &self.ids[&format!("{repo}.{k}")]);
        }
        p.replace("{token}", &self.ids["token"])
    }
}

const R: &str = "/v1/code/repos/acme/{r}";

/// Routes that read a repo (`Code::readable`).
fn reads() -> Vec<(&'static str, String)> {
    let repo = |p: &str| ("GET", format!("{R}{p}"));
    vec![
        repo(""),
        repo("/refs"),
        repo("/tree/main"),
        repo("/tree/main/"),
        repo("/tree/main/docs"),
        repo("/blob/main/a.txt"),
        repo("/readme/main"),
        repo("/blame/main/a.txt"),
        repo("/statuses/main"),
        repo("/paths/main"),
        repo("/log"),
        repo("/changesets/{cs}"),
        repo("/compare/{cs}...main"),
        repo("/changes"),
        repo("/changes/{cn}"),
        repo("/work"),
        repo("/work/{wn}"),
        repo("/events"),
        repo("/queue"),
        ("GET", "/v1/code/changes/{change}".into()),
        ("GET", "/v1/code/work/{work}".into()),
    ]
}

/// Routes that write to a repo (`Code::writable`, `writable_change`, `writable_work`).
fn writes() -> Vec<(&'static str, String)> {
    let repo = |m: &'static str, p: &str| (m, format!("{R}{p}"));
    let change = |m: &'static str, p: &str| (m, format!("/v1/code/changes/{{change}}{p}"));
    let work = |m: &'static str, p: &str| (m, format!("/v1/code/work/{{work}}{p}"));
    vec![
        repo("POST", "/changesets"),
        repo("POST", "/land"),
        repo("POST", "/changes"),
        repo("POST", "/work"),
        repo("POST", "/work/claim"),
        repo("POST", "/statuses/main"),
        repo("GET", "/audit"),
        change("PATCH", ""),
        change("POST", "/enqueue"),
        change("DELETE", "/enqueue"),
        change("PATCH", "/enqueue"),
        change("POST", "/revise"),
        change("POST", "/land"),
        change("POST", "/close"),
        change("POST", "/checks"),
        change("POST", "/preview"),
        change("DELETE", "/preview"),
        work("PATCH", ""),
        work("POST", "/heartbeat"),
        work("POST", "/release"),
        work("POST", "/complete"),
    ]
}

/// Routes that manage a repo's settings (`Code::check_manage`): writers, but not agent tokens.
fn manages() -> Vec<(&'static str, String)> {
    let repo = |m: &'static str, p: &str| (m, format!("{R}{p}"));
    vec![
        repo("PATCH", ""),
        repo("DELETE", ""),
        repo("GET", "/webhooks"),
        repo("POST", "/webhooks"),
        repo("GET", "/webhooks/{hook}"),
        repo("PATCH", "/webhooks/{hook}"),
        repo("DELETE", "/webhooks/{hook}"),
        repo("POST", "/webhooks/{hook}/ping"),
        repo("GET", "/webhooks/{hook}/deliveries"),
        repo("POST", "/webhooks/{hook}/deliveries/{delivery}/redeliver"),
        repo("POST", "/queue/pause"),
        repo("POST", "/queue/resume"),
        ("POST", "/v1/code/changes/{change}/approve-run".into()),
    ]
}

/// Writes to the owner itself (`Code::manages`).
fn owner_writes() -> Vec<(&'static str, String)> {
    vec![
        ("PATCH", "/v1/code/owners/acme".into()),
        ("POST", "/v1/code/owners/acme/tokens".into()),
        ("DELETE", "/v1/code/owners/acme/tokens/{token}".into()),
    ]
}

/// Every caller × every repo route, on a public and a private repo: what the auth matrix in
/// docs/code/test-coverage.md says, route by route. Denials change nothing, so the same world
/// serves every row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_repo_route_follows_the_auth_matrix() {
    let w = world().await;
    let a = &w.a;
    let (acme, agent, other) = (Some(w.acme.as_str()), Some(w.agent.as_str()), Some(w.other.as_str()));
    let (app_in, app_out) = (Some(w.app_in.as_str()), Some(w.app_out.as_str()));
    // (caller, token, [public, private]) for each kind of route. Owners, admin and agents
    // writing are 2xx (contract.rs::auth_matrix, tokens.rs); here only the denials.
    let read: [(&str, Option<&str>, [StatusCode; 2]); 7] = [
        ("admin", Some(ADMIN), [OK, OK]),
        ("owner", acme, [OK, OK]),
        ("agent", agent, [OK, OK]),
        ("review app installed on priv", app_in, [OK, OK]),
        ("review app not installed on priv", app_out, [OK, NOT_FOUND]),
        ("another owner", other, [OK, NOT_FOUND]),
        ("anonymous", None, [OK, NOT_FOUND]),
    ];
    let write: [(&str, Option<&str>, [StatusCode; 2]); 4] = [
        ("review app installed on priv", app_in, [FORBIDDEN, FORBIDDEN]),
        ("review app not installed on priv", app_out, [FORBIDDEN, NOT_FOUND]),
        ("another owner", other, [FORBIDDEN, NOT_FOUND]),
        ("anonymous", None, [UNAUTHORIZED, NOT_FOUND]),
    ];
    let manage: [(&str, Option<&str>, [StatusCode; 2]); 5] = [
        ("agent", agent, [FORBIDDEN, FORBIDDEN]),
        ("review app installed on priv", app_in, [FORBIDDEN, FORBIDDEN]),
        ("review app not installed on priv", app_out, [FORBIDDEN, NOT_FOUND]),
        ("another owner", other, [FORBIDDEN, NOT_FOUND]),
        ("anonymous", None, [UNAUTHORIZED, NOT_FOUND]),
    ];
    let mut n = 0;
    for (routes, rows) in [(reads(), &read[..]), (writes(), &write[..]), (manages(), &manage[..])] {
        for (method, path) in &routes {
            for (who, token, want) in rows {
                for (repo, want) in [("pub", want[0]), ("priv", want[1])] {
                    let p = w.path(repo, path);
                    assert_eq!(call(a, method, &p, None, *token).await, want, "{who}: {method} {p}");
                    n += 1;
                }
            }
        }
    }
    assert!(n > 400, "{n} requests");

    // The owner itself: managed by its own token (and admin), not by its agents or anyone else.
    for (method, path) in owner_writes() {
        let p = w.path("pub", &path);
        for (who, token, want) in [("agent", agent, FORBIDDEN), ("review app", app_in, FORBIDDEN), ("another owner", other, FORBIDDEN), ("anonymous", None, UNAUTHORIZED)] {
            assert_eq!(call(a, method, &p, Some(&json!({ "name": "x" })), token).await, want, "{who}: {method} {p}");
        }
    }
    // Owner-only reads are 404 to everyone else, so they don't learn what's there.
    for (who, token, want) in [
        ("admin", Some(ADMIN), OK),
        ("owner", acme, OK),
        ("agent", agent, NOT_FOUND),
        ("review app", app_in, NOT_FOUND),
        ("another owner", other, NOT_FOUND),
        ("anonymous", None, NOT_FOUND),
    ] {
        assert_eq!(call(a, "GET", "/v1/code/owners/acme/tokens", None, token).await, want, "{who}");
    }
    // Admin-only routes.
    for (who, token, want) in [("owner", acme, FORBIDDEN), ("agent", agent, FORBIDDEN), ("review app", app_in, FORBIDDEN), ("anonymous", None, UNAUTHORIZED)] {
        assert_eq!(call(a, "POST", "/v1/code/owners", Some(&json!({ "name": "neworg" })), token).await, want, "{who} creates an owner");
        assert_eq!(call(a, "GET", "/v1/code/github/installations", None, token).await, want, "{who} lists installations");
    }
    // Creating a repo: the owner's writers.
    let create = json!({ "owner": "acme", "name": "fresh" });
    for (who, token, want) in [("review app", app_in, FORBIDDEN), ("another owner", other, FORBIDDEN), ("anonymous", None, UNAUTHORIZED)] {
        assert_eq!(call(a, "POST", "/v1/code/repos", Some(&create), token).await, want, "{who}");
    }
    // A private repo the caller can't read hides from the listings too.
    for (who, token, sees) in [("owner", acme, true), ("review app installed on priv", app_in, true), ("review app not installed on priv", app_out, false), ("another owner", other, false), ("anonymous", None, false)] {
        let (_, list) = a.req("GET", "/v1/code/repos?owner=acme", None, token).await;
        let names: Vec<&str> = list.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names.contains(&"priv"), sees, "{who}: {names:?}");
    }
    // Listings and owner pages answer everyone (filtered).
    for token in [Some(ADMIN), acme, agent, app_in, other, None] {
        for path in ["/v1/code/owners", "/v1/code/owners/acme", "/v1/code/repos", "/v1/code/repos?owner=acme", "/v1/code/names/acme/pub"] {
            assert_eq!(call(a, "GET", path, None, token).await, OK, "{token:?} {path}");
        }
    }
}

/// An unknown (owner or review app), garbage or revoked token is a 401 on every route that looks
/// at the caller, even public reads, so a client finds out its token is bad. The routes that
/// don't (server info, name checks, the GitHub App's endpoints) answer as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_tokens_are_401_on_every_route() {
    let w = world().await;
    let a = &w.a;
    let mut routes: Vec<(&str, String)> = reads().into_iter().chain(writes()).chain(manages()).chain(owner_writes()).collect();
    routes.extend([
        ("GET", "/v1/code/owners/acme/tokens".into()),
        ("GET", "/v1/code/owners".into()),
        ("POST", "/v1/code/owners".into()),
        ("GET", "/v1/code/owners/acme".into()),
        ("GET", "/v1/code/repos".into()),
        ("POST", "/v1/code/repos".into()),
        ("GET", "/v1/code/names/acme/pub".into()),
        ("GET", "/v1/code/github/installations".into()),
    ]);
    for token in ["rlc_0000", "rla_0000", "hunter2", w.revoked.as_str()] {
        for (method, path) in &routes {
            for repo in ["pub", "priv"] {
                let p = w.path(repo, path);
                assert_eq!(call(a, method, &p, Some(&json!({ "owner": "acme", "name": "x" })), Some(token)).await, UNAUTHORIZED, "{token}: {method} {p}");
            }
        }
        // No caller needed: never a 401.
        let (s, v) = a.req("GET", "/v1/code", None, Some(token)).await;
        assert_eq!((s, v["auth"]["caller"]["kind"].as_str()), (OK, Some("invalid")));
        assert_eq!(call(a, "GET", "/v1/code/names/neworg", None, Some(token)).await, OK);
        assert_eq!(call(a, "GET", "/v1/code/github", None, Some(token)).await, OK);
        assert_eq!(call(a, "GET", "/v1/code/health", None, Some(token)).await, OK);
        assert_eq!(call(a, "GET", "/v1/code/github/setup", None, Some(token)).await, StatusCode::FOUND);
        assert_eq!(call(a, "POST", "/v1/code/github/webhook", Some(&json!({})), Some(token)).await, NOT_FOUND, "no App configured");
    }
}

/// Statuses the contract promises that no other test file asserts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn statuses_of_every_route() {
    let a = app(None);
    a.repo("acme", "x").await;
    a.repo("acme", "y").await;
    let x = "/v1/code/repos/acme/x";
    a.ok("POST", &format!("{x}/land"), Some(json!({ "message": "init", "files": { "README.md": { "content": "# x\n" }, "a.txt": { "content": "a\n" } } }))).await;
    let c = a.ok("POST", &format!("{x}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "c.txt": { "content": "c\n" } } }))).await;
    let cid = c["id"].as_str().unwrap();
    let hook = a.ok("POST", &format!("{x}/webhooks"), Some(json!({ "url": "http://127.0.0.1:9/hook" }))).await;
    let hid = hook["webhook"]["id"].as_str().unwrap();
    let files = json!({ "b.txt": { "content": "b\n" } });
    let cases: Vec<(&str, String, Option<Value>, StatusCode)> = vec![
        // Owners and tokens of an owner that doesn't exist.
        ("PATCH", "/v1/code/owners/nobody".into(), Some(json!({ "bio": "x" })), NOT_FOUND),
        ("POST", "/v1/code/owners/nobody/tokens".into(), Some(json!({ "name": "t" })), NOT_FOUND),
        ("GET", "/v1/code/owners/nobody/tokens".into(), None, NOT_FOUND),
        ("DELETE", "/v1/code/owners/nobody/tokens/tok_x".into(), None, NOT_FOUND),
        // Repos.
        ("POST", "/v1/code/repos".into(), Some(json!({ "name": "z" })), StatusCode::BAD_REQUEST),
        ("POST", "/v1/code/repos".into(), Some(json!({ "owner": "acme", "name": "z", "visibility": "secret" })), StatusCode::BAD_REQUEST),
        ("POST", "/v1/code/repos".into(), Some(json!({ "owner": "acme", "name": "z", "source": "svn" })), StatusCode::BAD_REQUEST),
        ("PATCH", x.into(), Some(json!({ "name": "Y" })), StatusCode::CONFLICT),
        ("PATCH", x.into(), Some(json!({ "name": "x.git" })), StatusCode::UNPROCESSABLE_ENTITY),
        ("PATCH", x.into(), Some(json!({ "required_checks": "ci" })), StatusCode::BAD_REQUEST),
        ("PATCH", x.into(), Some(json!({ "default_branch": "a..b" })), StatusCode::BAD_REQUEST),
        ("PATCH", "/v1/code/repos/acme/nope".into(), Some(json!({ "description": "d" })), NOT_FOUND),
        ("DELETE", "/v1/code/repos/acme/nope".into(), None, NOT_FOUND),
        // Reads of a missing repo or revision.
        ("GET", "/v1/code/repos/acme/nope/refs".into(), None, NOT_FOUND),
        ("GET", format!("{x}/tree/nope/"), None, NOT_FOUND),
        ("GET", format!("{x}/blob/nope/a.txt"), None, NOT_FOUND),
        ("GET", format!("{x}/readme/nope"), None, NOT_FOUND),
        ("GET", format!("{x}/readme/main?path=missing.md"), None, NOT_FOUND),
        ("GET", format!("{x}/blame/nope/a.txt"), None, NOT_FOUND),
        ("GET", format!("{x}/statuses/nope"), None, NOT_FOUND),
        ("GET", format!("{x}/log?rev=nope"), None, NOT_FOUND),
        ("GET", format!("{x}/log?cursor=nope"), None, NOT_FOUND),
        ("GET", format!("{x}/log?limit=lots"), None, StatusCode::BAD_REQUEST),
        ("GET", format!("{x}/compare/main...main?format=xml"), None, StatusCode::BAD_REQUEST),
        ("GET", "/v1/code/repos/acme/nope/events".into(), None, NOT_FOUND),
        ("GET", "/v1/code/repos/acme/nope/audit".into(), None, NOT_FOUND),
        // Writes.
        ("POST", format!("{x}/changesets"), Some(json!({})), StatusCode::BAD_REQUEST),
        ("POST", format!("{x}/changesets"), Some(json!({ "message": "m", "files": { "a": { "content": "x", "mode": "100777" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{x}/changesets"), Some(json!({ "base": "nope", "message": "m", "files": files })), NOT_FOUND),
        ("POST", format!("{x}/land"), Some(json!({ "message": "m", "files": files, "bypass": "yes" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{x}/work/claim"), Some(json!({ "agent": "a", "labels": "ui" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{x}/statuses/main"), Some(json!({ "context": "ci" })), StatusCode::BAD_REQUEST),
        // Webhooks.
        ("PATCH", format!("{x}/webhooks/{hid}"), Some(json!({ "url": "ftp://x" })), StatusCode::BAD_REQUEST),
        ("PATCH", format!("{x}/webhooks/{hid}"), Some(json!({ "events": ["nope"] })), StatusCode::BAD_REQUEST),
        ("PATCH", format!("{x}/webhooks/{hid}"), Some(json!({ "active": "no" })), StatusCode::BAD_REQUEST),
        ("PATCH", format!("{x}/webhooks/{hid}"), Some(json!({ "secret": "short" })), StatusCode::BAD_REQUEST),
        ("GET", format!("{x}/webhooks/wh_nope"), None, NOT_FOUND),
        ("PATCH", format!("{x}/webhooks/wh_nope"), Some(json!({ "active": false })), NOT_FOUND),
        ("DELETE", format!("{x}/webhooks/wh_nope"), None, NOT_FOUND),
        ("POST", format!("{x}/webhooks/wh_nope/ping"), None, NOT_FOUND),
        ("GET", format!("{x}/webhooks/wh_nope/deliveries"), None, NOT_FOUND),
        ("POST", format!("{x}/webhooks/wh_nope/deliveries/dl_nope/redeliver"), None, NOT_FOUND),
        // Another repo's webhook isn't this repo's.
        ("GET", format!("/v1/code/repos/acme/y/webhooks/{hid}"), None, NOT_FOUND),
        // Changes.
        ("PATCH", format!("/v1/code/changes/{cid}"), Some(json!({ "title": "  " })), StatusCode::BAD_REQUEST),
        ("PATCH", format!("/v1/code/changes/{cid}"), Some(json!({ "body": 1 })), StatusCode::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{cid}/land"), Some(json!({ "bypass": "yes" })), StatusCode::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{cid}/land"), Some(json!({ "lane": "vip" })), StatusCode::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{cid}/revise"), Some(json!({ "message": "m", "files": {} })), StatusCode::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{cid}/revise"), Some(json!({ "base": "nope", "message": "m", "files": files })), NOT_FOUND),
        // Work items.
        ("PATCH", "/v1/code/work/wi_nope".into(), Some(json!({ "title": "t" })), NOT_FOUND),
    ];
    let mut cases = cases;
    // Every change and work route on an id that doesn't exist.
    for (m, p) in [
        ("PATCH", ""),
        ("POST", "/enqueue"),
        ("DELETE", "/enqueue"),
        ("PATCH", "/enqueue"),
        ("POST", "/approve-run"),
        ("POST", "/revise"),
        ("POST", "/land"),
        ("POST", "/close"),
        ("POST", "/checks"),
        ("POST", "/preview"),
        ("DELETE", "/preview"),
    ] {
        cases.push((m, format!("/v1/code/changes/chg_nope{p}"), Some(json!({ "lane": "urgent", "name": "t", "status": "success" })), NOT_FOUND));
    }
    for p in ["/heartbeat", "/release", "/complete"] {
        cases.push(("POST", format!("/v1/code/work/wi_nope{p}"), Some(json!({})), NOT_FOUND));
    }
    for (method, path, body, want) in &cases {
        assert_eq!(call(&a, method, path, body.as_ref(), None).await, *want, "{method} {path} {body:?}");
    }

    // A repo has at most 20 webhooks (422).
    for _ in 1..20 {
        a.ok("POST", &format!("{x}/webhooks"), Some(json!({ "url": "http://127.0.0.1:9/hook" }))).await;
    }
    assert_eq!(call(&a, "POST", &format!("{x}/webhooks"), Some(&json!({ "url": "http://127.0.0.1:9/hook" })), None).await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(a.ok("GET", &format!("{x}/webhooks"), None).await.as_array().unwrap().len(), 20);

    // A landed change can't be closed (409).
    let landed = a.ok("POST", &format!("{x}/changes"), Some(json!({ "title": "l", "message": "l", "files": { "l.txt": { "content": "l\n" } } }))).await;
    let lid = landed["id"].as_str().unwrap();
    a.ok("POST", &format!("/v1/code/changes/{lid}/land"), None).await;
    assert_eq!(call(&a, "POST", &format!("/v1/code/changes/{lid}/close"), None, None).await, StatusCode::CONFLICT);
    // An empty repo's log is empty, not an error.
    assert_eq!(a.ok("GET", "/v1/code/repos/acme/y/log", None).await, json!({ "items": [], "cursor": null }));

    // A renamed repo: every route under the old name answers 301 to where it went.
    a.ok("PATCH", "/v1/code/repos/acme/y", Some(json!({ "name": "z" }))).await;
    for (method, path) in [("GET", "/v1/code/repos/acme/y/refs"), ("GET", "/v1/code/repos/acme/y/tree/main/"), ("PATCH", "/v1/code/repos/acme/y"), ("POST", "/v1/code/repos/acme/y/work")] {
        let (s, v) = a.req(method, path, Some(json!({ "title": "t" })), None).await;
        assert_eq!((s, v["redirect"].as_str()), (StatusCode::MOVED_PERMANENTLY, Some("acme/z")), "{method} {path}: {v}");
    }
    let r = a.send(Request::get("/v1/code/repos/acme/y/refs").body(Body::empty()).unwrap()).await;
    assert_eq!(r.headers()["location"], "/v1/code/repos/acme/z");

    // The health check of a single node: the database answers, there's no cluster.
    let (s, v) = a.req("GET", "/v1/code/health", None, None).await;
    assert_eq!((s, v["status"].as_str(), v["database"].as_bool(), v["cluster"].is_null()), (OK, Some("ok"), Some(true), true), "{v}");
}

/// `POST /v1/code/github/webhook` with the App configured: 401 for a bad signature, 400 for a
/// signed payload that isn't one, 200 once processed (and for a delivery seen before).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_webhook_statuses() {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    const SECRET: &str = "webhook-secret";
    let a = app(None);
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap().to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
    let mut config = code_github::AppConfig::new(1, key, SECRET, "roll-test");
    config.api_base = "http://127.0.0.1:9/api".into();
    config.web_base = "http://127.0.0.1:9/git".into();
    let options = code_control::github::Options { mirror: code_github::MirrorConfig::default(), client: code_github::ClientOptions::default() };
    code_control::github::enable(&a.code, config, options).unwrap();
    assert_eq!(a.ok("GET", "/v1/code/github", None).await["configured"], true);
    let hook = |event: &str, delivery: &str, body: &[u8], secret: &str| {
        Request::post("/v1/code/github/webhook")
            .header("x-github-event", event)
            .header("x-github-delivery", delivery)
            .header("x-hub-signature-256", code_github::webhook::sign(secret.as_bytes(), body))
            .header("content-type", "application/json")
            .body(Body::from(body.to_vec()))
            .unwrap()
    };
    let json_error = |r: axum::response::Response| async move {
        let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert!(v["error"].is_string(), "{v}");
    };
    let ping = br#"{"zen":"Keep it logically awesome.","hook_id":1}"#;
    let r = a.send(hook("ping", "d-1", ping, "wrong")).await;
    assert_eq!(r.status(), UNAUTHORIZED);
    json_error(r).await;
    let r = a.send(hook("issues", "d-2", b"not json", SECRET)).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    json_error(r).await;
    assert_eq!(a.send(hook("ping", "d-3", ping, SECRET)).await.status(), OK);
    assert_eq!(a.send(hook("ping", "d-3", ping, SECRET)).await.status(), OK, "a redelivery");
}
