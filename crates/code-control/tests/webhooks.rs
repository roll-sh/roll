//! Outbound repo webhooks (events, signatures, retries, the delivery log, redelivery, filters,
//! SSRF, auth), the commit status API for external CI, blame against `git blame --first-parent`,
//! and code search at a change's head.

mod common;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use common::{App, app, app_with, git, write};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
struct Got {
    headers: HeaderMap,
    raw: Vec<u8>,
    body: Value,
}

impl Got {
    fn event(&self) -> &str {
        self.headers["x-roll-event"].to_str().unwrap()
    }
}

/// A webhook receiver: records what it gets; fails the first `fail[delivery]` attempts.
struct Receiver {
    url: String,
    got: Arc<Mutex<Vec<Got>>>,
    fail_first: Arc<Mutex<HashMap<String, u32>>>,
    fail_events: Arc<Mutex<HashMap<String, u32>>>,
}

async fn receiver() -> Receiver {
    let got: Arc<Mutex<Vec<Got>>> = Arc::default();
    let fail_first: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
    let fail_events: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
    let (g, f, fe) = (got.clone(), fail_first.clone(), fail_events.clone());
    let app = axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let (got, fail, fail_events) = (g.clone(), f.clone(), fe.clone());
        async move {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let d = headers.get("x-roll-delivery").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let ev = headers.get("x-roll-event").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            got.lock().push(Got { headers, raw: body.to_vec(), body: v });
            {
                let mut fe = fail_events.lock();
                if let Some(n) = fe.get_mut(&ev)
                    && *n > 0
                {
                    *n -= 1;
                    fail.lock().insert(d.clone(), 0);
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
            StatusCode::OK.into_response()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Receiver { url, got, fail_first, fail_events }
}

impl Receiver {
    async fn wait(&self, event: &str, pred: impl Fn(&Got) -> bool) -> Got {
        for _ in 0..500 {
            if let Some(g) = self.got.lock().iter().find(|g| g.event() == event && pred(g)).cloned() {
                return g;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let seen: Vec<String> = self.got.lock().iter().map(|g| g.event().to_string()).collect();
        panic!("no {event} delivery; got {seen:?}");
    }
    fn count(&self, event: &str) -> usize {
        self.got.lock().iter().filter(|g| g.event() == event).count()
    }
}

const P: &str = "/v1/code/repos/acme/api";

async fn setup() -> App {
    let a = app_with(|c| c.review_app_retry_ms = 20);
    a.repo("acme", "api").await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "base", "files": { "a.txt": { "content": "a\n" } } }))).await;
    a
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_deliver_signed_events() {
    let a = setup().await;
    let rx = receiver().await;
    // Validation.
    for (body, why) in [
        (json!({ "url": "ftp://x" }), "scheme"),
        (json!({}), "no url"),
        (json!({ "url": rx.url, "events": ["landed", "nope"] }), "unknown event"),
        (json!({ "url": rx.url, "events": [] }), "no events"),
        (json!({ "url": rx.url, "secret": "short" }), "short secret"),
        (json!({ "url": "https://user:pw@hooks.example.com/" }), "credentials"),
        (json!({ "url": rx.url, "active": "yes" }), "active"),
    ] {
        let (s, _) = a.req("POST", &format!("{P}/webhooks"), Some(body), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}");
    }
    let (s, created) = a.req("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["landed", "change", "push", "check", "work"] })), None).await;
    assert_eq!(s, StatusCode::CREATED, "{created}");
    let id = created["webhook"]["id"].as_str().unwrap().to_string();
    let secret = created["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("rlw_"));
    assert_eq!(created["webhook"]["events"], json!(["change", "check", "landed", "push", "work"]));
    assert!(created["webhook"].get("secret").is_none(), "the secret isn't part of the webhook");
    // A signed ping.
    let ping = rx.wait("ping", |_| true).await;
    let ts: i64 = ping.headers["x-roll-timestamp"].to_str().unwrap().parse().unwrap();
    assert_eq!(ping.headers["x-roll-signature-256"].to_str().unwrap(), code_control::review::apps::signature(&secret, ts, &ping.raw));
    assert_eq!(ping.body["webhook"], id);
    assert_eq!(ping.body["repo"]["full_name"], "acme/api");
    assert_eq!(ping.body["data"]["zen"], "keep the code rolling");
    // Landings, changes, work items.
    let landed = a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "second", "files": { "b.txt": { "content": "b\n" } } }))).await;
    let got = rx.wait("landed", |g| g.body["data"]["changeset"] == landed["changeset"]).await;
    assert_eq!(got.body["data"]["message"], "second");
    assert_eq!(got.body["event"], "landed");
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "c.txt": { "content": "c\n" } } }))).await;
    rx.wait("change", |g| g.body["data"]["id"] == c["id"]).await;
    let w = a.ok("POST", &format!("{P}/work"), Some(json!({ "title": "w" }))).await;
    rx.wait("work", |g| g.body["data"]["id"] == w["id"]).await;
    // Events it didn't ask for don't come.
    a.ok("POST", &format!("/v1/code/changes/{}/reviews", c["id"].as_str().unwrap()), Some(json!({ "verdict": "comment", "body": "hm" }))).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rx.count("review"), 0);

    // A failing receiver is retried until it answers; the log shows the attempts.
    rx.fail_events.lock().insert("work".into(), 2);
    let w2 = a.ok("POST", &format!("{P}/work"), Some(json!({ "title": "flaky" }))).await;
    rx.wait("work", |g| g.body["data"]["id"] == w2["id"]).await;
    let mut log = Value::Null;
    for _ in 0..300 {
        log = a.ok("GET", &format!("{P}/webhooks/{id}/deliveries"), None).await;
        let d = log.as_array().unwrap().iter().find(|d| d["payload"]["data"]["id"] == w2["id"]).cloned();
        if d.as_ref().is_some_and(|d| d["state"] == "delivered") {
            assert_eq!(d.unwrap()["attempts"], 3);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let d = log.as_array().unwrap().iter().find(|d| d["payload"]["data"]["id"] == w2["id"]).unwrap();
    assert_eq!(d["state"], "delivered", "{d}");
    assert!(!rx.fail_first.lock().is_empty());
    // Redeliver the ping: the same delivery again.
    let ping_id = ping.headers["x-roll-delivery"].to_str().unwrap().to_string();
    let before = rx.got.lock().iter().filter(|g| g.headers["x-roll-delivery"] == ping_id.as_str()).count();
    a.ok("POST", &format!("{P}/webhooks/{id}/deliveries/{ping_id}/redeliver"), None).await;
    for _ in 0..300 {
        if rx.got.lock().iter().filter(|g| g.headers["x-roll-delivery"] == ping_id.as_str()).count() > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(rx.got.lock().iter().filter(|g| g.headers["x-roll-delivery"] == ping_id.as_str()).count() > before);
    let (s, _) = a.req("POST", &format!("{P}/webhooks/{id}/deliveries/dl_nope/redeliver"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // An explicit ping.
    let (s, p) = a.req("POST", &format!("{P}/webhooks/{id}/ping"), None, None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(p["event"], "ping");

    // Update: off, then on with every event and our own secret.
    let off = a.ok("PATCH", &format!("{P}/webhooks/{id}"), Some(json!({ "active": false }))).await;
    assert_eq!(off["active"], false);
    let n = rx.count("landed");
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "quiet", "files": { "q.txt": { "content": "q\n" } } }))).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(rx.count("landed"), n, "an inactive webhook gets nothing");
    let mine = "my-own-secret-0123456789";
    let on = a.ok("PATCH", &format!("{P}/webhooks/{id}"), Some(json!({ "active": true, "events": ["*"], "secret": mine }))).await;
    assert_eq!(on["events"].as_array().unwrap().len(), code_control::webhooks::EVENTS.len());
    let r2 = a.ok("POST", &format!("/v1/code/changes/{}/reviews", c["id"].as_str().unwrap()), Some(json!({ "verdict": "comment", "body": "now" }))).await;
    let g = rx.wait("review", |g| g.body["data"]["id"] == r2["id"]).await;
    let ts: i64 = g.headers["x-roll-timestamp"].to_str().unwrap().parse().unwrap();
    assert_eq!(g.headers["x-roll-signature-256"].to_str().unwrap(), code_control::review::apps::signature(mine, ts, &g.raw));
    assert_eq!(a.ok("GET", &format!("{P}/webhooks"), None).await.as_array().unwrap().len(), 1);
    assert_eq!(a.ok("GET", &format!("{P}/webhooks/{id}"), None).await["id"], id);
    // Delete.
    let (s, _) = a.req("DELETE", &format!("{P}/webhooks/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = a.req("GET", &format!("{P}/webhooks/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_are_webhook_events() {
    let a = app(None);
    a.repo("acme", "web").await;
    let rx = receiver().await;
    a.ok("POST", "/v1/code/repos/acme/web/webhooks", Some(json!({ "url": rx.url, "events": ["push"] }))).await;
    let base = a.serve().await;
    let url = format!("{base}/git/acme/web.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    git(&home, &home, &["clone", "-q", &url, "w"]);
    let w = home.join("w");
    write(&w, "x.txt", "x\n");
    git(&home, &w, &["add", "-A"]);
    git(&home, &w, &["commit", "-qm", "x"]);
    git(&home, &w, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
    let sha = git(&home, &w, &["rev-parse", "HEAD"]).trim().to_string();
    let g = rx.wait("push", |g| g.body["data"]["ref"] == "refs/heads/feature").await;
    assert_eq!(g.body["data"]["after"], sha);
    assert!(g.body["data"]["before"].is_null());
    assert_eq!(g.body["data"]["accepted"], true);
    // CI reports by git commit, which is what it knows: the pushed branch's change gets the check.
    let (s, v) = a.req("POST", &format!("/v1/code/repos/acme/web/statuses/{sha}"), Some(json!({ "state": "success", "context": "ci" })), None).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["git_commit"], sha);
    let change = v["changes"][0].as_str().expect("the branch's change").to_string();
    let c = a.ok("GET", &format!("/v1/code/changes/{change}"), None).await;
    assert!(c["checks"].as_array().unwrap().iter().any(|k| k["name"] == "ci" && k["status"] == "success"));
    assert_eq!(a.ok("GET", &format!("/v1/code/repos/acme/web/statuses/{}", &sha[..12]), None).await["state"], "success");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_need_writers_and_public_addresses() {
    let a = common::app(Some("root"));
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some("root")).await;
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "eve", "kind": "user" })), Some("root")).await;
    a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api", "visibility": "public" })), Some("root")).await;
    let (_, t) = a.req("POST", "/v1/code/owners/eve/tokens", Some(json!({ "name": "t" })), Some("root")).await;
    let eve = t["token"].as_str().unwrap();
    let body = json!({ "url": "http://127.0.0.1:9/hook" });
    let (s, _) = a.req("POST", &format!("{P}/webhooks"), Some(body.clone()), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = a.req("POST", &format!("{P}/webhooks"), Some(body.clone()), Some(eve)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("GET", &format!("{P}/webhooks"), None, Some(eve)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("POST", &format!("{P}/statuses/main"), Some(json!({ "state": "success" })), Some(eve)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // With an admin token configured, private addresses are refused when delivering.
    let (s, created) = a.req("POST", &format!("{P}/webhooks"), Some(body), Some("root")).await;
    assert_eq!(s, StatusCode::CREATED);
    let id = created["webhook"]["id"].as_str().unwrap();
    let mut err = Value::Null;
    for _ in 0..300 {
        let (_, log) = a.req("GET", &format!("{P}/webhooks/{id}/deliveries"), None, Some("root")).await;
        err = log[0]["last_error"].clone();
        if !err.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(err.as_str().unwrap_or("").contains("private address"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_statuses_become_checks() {
    let a = setup().await;
    let rx = receiver().await;
    a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["check"] }))).await;
    // A queue repo that requires "ci" from external CI.
    a.ok("PATCH", P, Some(json!({ "land_policy": "queue", "required_checks": ["ci"] }))).await;
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "c.txt": { "content": "c\n" } } }))).await;
    let id = c["id"].as_str().unwrap().to_string();
    assert!(c["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "checks"), "{}", c["blockers"]);
    let head = c["head"].as_str().unwrap().to_string();
    // Pending → a running check.
    let (s, v) = a.req("POST", &format!("{P}/statuses/{head}"), Some(json!({ "state": "pending", "context": "ci", "target_url": "https://ci.example/1", "description": "building" })), None).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["changes"], json!([id]));
    assert_eq!(v["changeset"], head);
    let ch = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    let k = ch["checks"].as_array().unwrap().iter().find(|k| k["name"] == "ci").unwrap().clone();
    assert_eq!((k["status"].as_str(), k["source"].as_str(), k["url"].as_str()), (Some("running"), Some("external"), Some("https://ci.example/1")));
    rx.wait("check", |g| g.body["data"]["change"] == id.as_str()).await;
    // Error → failure with the reason; success unblocks.
    a.ok("POST", &format!("{P}/statuses/{head}"), Some(json!({ "state": "error", "context": "ci", "description": "agent lost" }))).await;
    let ch = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    let k = ch["checks"].as_array().unwrap().iter().find(|k| k["name"] == "ci").unwrap().clone();
    assert_eq!((k["status"].as_str(), k["summary"].as_str()), (Some("failure"), Some("error: agent lost")));
    a.ok("POST", &format!("{P}/statuses/{head}"), Some(json!({ "state": "success", "context": "ci" }))).await;
    let ch = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    assert!(!ch["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "checks"), "{}", ch["blockers"]);
    // A second context; the combined state.
    a.ok("POST", &format!("{P}/statuses/{head}"), Some(json!({ "state": "pending", "context": "lint" }))).await;
    let all = a.ok("GET", &format!("{P}/statuses/{head}"), None).await;
    assert_eq!((all["state"].as_str(), all["total_count"].as_u64()), (Some("pending"), Some(2)));
    a.ok("POST", &format!("{P}/statuses/{head}"), Some(json!({ "state": "failure", "context": "lint" }))).await;
    assert_eq!(a.ok("GET", &format!("{P}/statuses/{head}"), None).await["state"], "failure");
    // A commit on main with no change: recorded, no check anywhere.
    let main = a.ok("POST", &format!("{P}/statuses/main"), Some(json!({ "state": "success", "context": "deploy" }))).await;
    assert_eq!(main["changes"], json!([]));
    assert_eq!(a.ok("GET", &format!("{P}/statuses/main"), None).await["state"], "success");
    // Errors.
    for (body, want) in [
        (json!({ "state": "done" }), StatusCode::BAD_REQUEST),
        (json!({ "state": "success", "target_url": "javascript:alert(1)" }), StatusCode::BAD_REQUEST),
        (json!({ "state": "success", "context": "x".repeat(101) }), StatusCode::BAD_REQUEST),
    ] {
        let (s, _) = a.req("POST", &format!("{P}/statuses/{head}"), Some(body), None).await;
        assert_eq!(s, want);
    }
    let (s, _) = a.req("POST", &format!("{P}/statuses/{}", "f".repeat(40)), Some(json!({ "state": "success" })), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Deleting the repo removes its webhooks and statuses.
    let repo_id = a.code.find_repo("acme", "api").unwrap().id;
    a.req("DELETE", P, None, None).await;
    assert!(a.code.db.webhooks(&repo_id).unwrap().is_empty());
}

/// Blame through the API equals `git blame --first-parent` of a pushed history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blame_matches_git() {
    let a = app(None);
    a.repo("acme", "web").await;
    let base = a.serve().await;
    let url = format!("{base}/git/acme/web.git");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    git(&home, &home, &["clone", "-q", &url, "w"]);
    let w = home.join("w");
    let mut lines: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
    let save = |lines: &[String]| write(&w, "f.txt", &lines.iter().map(|l| format!("{l}\n")).collect::<String>());
    save(&lines);
    git(&home, &w, &["add", "-A"]);
    git(&home, &w, &["commit", "-qm", "one"]);
    lines[2] = "line 3 changed".into();
    save(&lines);
    git(&home, &w, &["commit", "-qam", "two"]);
    git(&home, &w, &["checkout", "-qb", "side"]);
    lines[6] = "line 7 from side".into();
    save(&lines);
    git(&home, &w, &["commit", "-qam", "side"]);
    git(&home, &w, &["checkout", "-q", "main"]);
    write(&w, "other.txt", "o\n");
    git(&home, &w, &["add", "-A"]);
    git(&home, &w, &["commit", "-qm", "other"]);
    git(&home, &w, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    git(&home, &w, &["push", "-q", "origin", "main"]);
    let porcelain = git(&home, &w, &["blame", "--first-parent", "--porcelain", "f.txt"]);
    let mut want: Vec<String> = Vec::new();
    for l in porcelain.lines() {
        let mut p = l.split(' ');
        let first = p.next().unwrap();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let n: usize = p.nth(1).unwrap().parse().unwrap();
            if want.len() < n {
                want.resize(n, String::new());
            }
            want[n - 1] = first.to_string();
        }
    }
    let b = a.ok("GET", "/v1/code/repos/acme/web/blame/main/f.txt", None).await;
    assert_eq!((b["lines"].as_u64(), b["complete"].as_bool(), b["rev"].as_str()), (Some(8), Some(true), Some("main")));
    let mut ours = Vec::new();
    for r in b["ranges"].as_array().unwrap() {
        assert!(r["author"]["name"].is_string() && r["time"].is_i64() && r["message"].is_string(), "{r}");
        for _ in r["start"].as_u64().unwrap()..=r["end"].as_u64().unwrap() {
            ours.push(r["git_commit"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(ours, want);
    // Cached answers are the same; a revision name is echoed back.
    let again = a.ok("GET", &format!("/v1/code/repos/acme/web/blame/{}/f.txt", b["changeset"].as_str().unwrap()), None).await;
    assert_eq!(again["ranges"], b["ranges"]);
    assert_eq!(again["rev"], b["changeset"]);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/web/blame/main/nope.txt", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/web/blame/nope/f.txt", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// Search at a change's head: its own edits and deletions count, the rest comes from the index.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_at_a_change_head() {
    let a = setup().await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "m", "files": { "src/one.rs": { "content": "fn needle_one() {}\n" }, "src/two.rs": { "content": "fn needle_two() {}\n" } } }))).await;
    let c = a
        .ok(
            "POST",
            &format!("{P}/changes"),
            Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "src/two.rs": { "delete": true }, "src/three.rs": { "content": "fn needle_three() {}\n" } } })),
        )
        .await;
    let head = c["head"].as_str().unwrap();
    let paths = |v: &Value| v["matches"].as_array().unwrap().iter().map(|m| m["path"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let main = a.ok("GET", &format!("{P}/search?q=needle"), None).await;
    assert_eq!(paths(&main), vec!["src/one.rs", "src/two.rs"]);
    assert!(main["overlay"].is_null());
    let main_head = main["rev"].as_str().unwrap().to_string();
    let at = a.ok("GET", &format!("{P}/search?q=needle&rev={head}"), None).await;
    assert_eq!(paths(&at), vec!["src/one.rs", "src/three.rs"], "{at}");
    assert_eq!(at["rev"], head);
    assert_eq!(at["overlay"]["base"], main_head);
    assert_eq!(at["overlay"]["files"], 1);
    // The default branch by name is the plain index search.
    let named = a.ok("GET", &format!("{P}/search?q=needle&rev=main"), None).await;
    assert!(named["overlay"].is_null());
    // Path globs apply to both halves; limits too.
    let only = a.ok("GET", &format!("{P}/search?q=needle&rev={head}&path=src/three.rs"), None).await;
    assert_eq!(paths(&only), vec!["src/three.rs"]);
    let one = a.ok("GET", &format!("{P}/search?q=needle&rev={head}&limit=1"), None).await;
    assert_eq!((paths(&one).len(), one["truncated"].as_bool()), (1, Some(true)));
    let (s, _) = a.req("GET", &format!("{P}/search?q=needle&rev=nope"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = a.req("GET", &format!("{P}/search?q=(&regex=true&rev={head}"), None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// A chain of `n` changesets on top of `base`, one file each (built in the store directly).
async fn chain(a: &App, repo_id: &str, base: code_store::ChangesetId, n: usize) -> code_store::ChangesetId {
    let mut cur = base;
    for i in 0..n {
        let edits = vec![(
            code_store::RepoPath::new(format!("burst/{}.txt", i % 50)).unwrap(),
            code_store::Edit::Write { content: Bytes::from(format!("{i}\n")), mode: code_store::Mode::Regular },
        )];
        cur = a
            .code
            .store
            .create_changeset(
                repo_id,
                code_store::NewChangeset {
                    base: Some(cur),
                    other_parents: vec![],
                    author: code_store::Signature::new("Agent", "agent@roll.sh", 1_760_000_000, 0),
                    committer: None,
                    message: format!("burst {i}"),
                    edits,
                },
            )
            .await
            .unwrap();
    }
    cur
}

async fn wait_count(rx: &Receiver, event: &str, n: usize, secs: u64) {
    for _ in 0..secs * 20 {
        if rx.count(event) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} of {n} {event} deliveries", rx.count(event));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landed_deliveries_survive_a_burst_larger_than_the_broadcast_channel() {
    let a = setup().await;
    let rx = receiver().await;
    a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["landed"] }))).await;
    rx.wait("ping", |_| true).await;
    let repo = a.ok("GET", P, None).await;
    let repo_id = repo["id"].as_str().unwrap().to_string();
    let head = a.code.store.get_ref(&repo_id, "refs/heads/main").unwrap().unwrap();
    // 4,500 landings published at once: more than the store's broadcast channel holds (4,096),
    // so a subscriber that isn't keeping up lags. None may be missed.
    const N: usize = 4_500;
    let tip = chain(&a, &repo_id, head, N).await;
    let landed = a.code.store.advance(&repo_id, "main", Some(head), tip).await.unwrap();
    assert_eq!(landed.len(), N);
    wait_count(&rx, "landed", N, 180).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = rx.got.lock().clone();
    let mut seen: Vec<String> = got.iter().filter(|g| g.event() == "landed").map(|g| g.body["data"]["changeset"].as_str().unwrap().to_string()).collect();
    assert_eq!(seen.len(), N, "each landing exactly once in a clean run");
    let want: Vec<String> = landed.iter().map(|l| code_control::cs_id(&l.changeset)).collect();
    seen.sort();
    let mut want_sorted = want.clone();
    want_sorted.sort();
    assert_eq!(seen, want_sorted, "every landed changeset was delivered");
    // Deliveries go out concurrently (no order promise); `generation` orders them.
    let first = got.iter().filter(|g| g.event() == "landed").min_by_key(|g| g.body["data"]["generation"].as_u64().unwrap()).unwrap();
    assert_eq!(first.body["data"]["changeset"].as_str().unwrap(), want[0]);
    assert_eq!(first.body["data"]["message"], "burst 0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn landings_while_the_daemon_was_down_are_delivered_after_a_restart() {
    let rx = receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = |c: &mut code_control::Config| c.review_app_retry_ms = 20;
    let (repo_id, old) = {
        let code = common::open_in(dir.path(), cfg);
        let a = App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() };
        let r = a.repo("acme", "api").await;
        a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["landed"] }))).await;
        rx.wait("ping", |_| true).await;
        a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "before", "files": { "a.txt": { "content": "1\n" } } }))).await;
        rx.wait("landed", |g| g.body["data"]["message"] == "before").await;
        (r["id"].as_str().unwrap().to_string(), Arc::downgrade(&a.code))
    };
    // The process stops: nothing of it keeps running (its background tasks hold it weakly, so
    // its forwarder can't forward what the next process lands while it's "down").
    for _ in 0..500 {
        if old.upgrade().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(old.upgrade().is_none(), "a dropped Code is kept alive by its own tasks");
    // "Down": a process that lands without delivering webhooks (they aren't started), like a
    // crash between the landing and its deliveries.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let code = common::open_in(dir.path(), cfg);
    let a = App { router: code_control::api::router(code.clone()), code: code.clone(), dir: tempfile::tempdir().unwrap() };
    let head = code.store.get_ref(&repo_id, "refs/heads/main").unwrap().unwrap();
    let tip = chain(&a, &repo_id, head, 3).await;
    code.store.advance(&repo_id, "main", Some(head), tip).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rx.count("landed"), 1, "nothing delivered while webhooks aren't running");
    // Back up: everything after the persisted cursor goes out, and only that.
    code.start_webhooks();
    wait_count(&rx, "landed", 4, 30).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rx.count("landed"), 4);
    let mut msgs: Vec<(u64, Value)> =
        rx.got.lock().iter().filter(|g| g.event() == "landed").map(|g| (g.body["data"]["generation"].as_u64().unwrap(), g.body["data"]["message"].clone())).collect();
    msgs.sort_by_key(|m| m.0);
    let msgs: Vec<Value> = msgs.into_iter().map(|m| m.1).collect();
    assert_eq!(msgs, vec![json!("before"), json!("burst 0"), json!("burst 1"), json!("burst 2")]);
}

/// A delivery queued by a process that stopped before sending it goes out once the next process
/// starts its webhooks, without waiting for an event of its own to wake the delivery worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deliveries_left_queued_are_sent_after_a_restart() {
    let rx = counter(false, None).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = |c: &mut code_control::Config| c.review_app_retry_ms = 20;
    let (hook_id, old) = {
        let code = common::open_in(dir.path(), cfg);
        let a = App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() };
        a.repo("acme", "api").await;
        let id = hook(&a, &rx.url, json!(["change"])).await;
        rx.wait("ping", 1, 30).await;
        (id, Arc::downgrade(&a.code))
    };
    for _ in 0..500 {
        if old.upgrade().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(old.upgrade().is_none(), "the old process is gone");
    let code = common::open_in(dir.path(), cfg);
    // What the old process queued and never sent.
    code.db
        .sql()
        .execute(
            "INSERT INTO review_deliveries (id, app_id, event, payload, state, next_at, created_at) VALUES ('dl_left', ?, 'change', ?, 'pending', 0, 0)",
            code_sql::params![hook_id.as_str(), json!({ "event": "change", "delivery": "dl_left" }).to_string()],
        )
        .unwrap();
    code.start_webhooks();
    for _ in 0..1000 {
        if rx.attempts.lock().contains_key("dl_left") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rx.attempts.lock().get("dl_left"), Some(&1), "the left-over delivery was sent once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_webhook_gets_no_history() {
    let a = setup().await;
    // Landings before the webhook existed.
    for i in 0..3 {
        a.ok("POST", &format!("{P}/land"), Some(json!({ "message": format!("old {i}"), "files": { "o.txt": { "content": format!("{i}\n") } } }))).await;
    }
    let rx = receiver().await;
    a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["landed"] }))).await;
    rx.wait("ping", |_| true).await;
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": "new", "files": { "n.txt": { "content": "1\n" } } }))).await;
    rx.wait("landed", |g| g.body["data"]["message"] == "new").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rx.count("landed"), 1, "only landings after the webhook was created");
}

/// The delivery log keeps the start of each attempt's response (2 KB at most).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deliveries_keep_the_start_of_the_response() {
    let a = setup().await;
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let c = calls.clone();
    let app = axum::Router::new().fallback(move || {
        let c = c.clone();
        async move {
            if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                (StatusCode::BAD_GATEWAY, "é".repeat(3000)).into_response()
            } else {
                (StatusCode::OK, "thanks").into_response()
            }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let created = a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": url, "events": ["landed"] }))).await;
    let id = created["webhook"]["id"].as_str().unwrap().to_string();
    // The ping fails twice (a long error page), then is retried and succeeds.
    let mut seen_failure = None;
    for _ in 0..300 {
        let ds = a.ok("GET", &format!("{P}/webhooks/{id}/deliveries"), None).await;
        let d = &ds[0];
        if d["last_status"] == 502 {
            seen_failure = Some(d.clone());
        }
        if d["state"] == "delivered" {
            let failure = seen_failure.expect("saw the failed attempt");
            let body = failure["last_response"].as_str().unwrap();
            assert!(body.len() <= 2048 && body.len() >= 2046, "{}", body.len());
            assert!(body.chars().all(|c| c == 'é'), "cut at a character");
            assert_eq!(d["last_response"], "thanks");
            assert_eq!(d["last_status"], 200);
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("never delivered");
}

async fn land_file(a: &App, msg: &str, path: &str, content: &str) -> String {
    a.ok("POST", &format!("{P}/land"), Some(json!({ "message": msg, "files": { path: { "content": content } } }))).await["changeset"].as_str().unwrap().to_string()
}

async fn status(a: &App, rev: &str, context: &str, state: &str) {
    let (s, v) = a.req("POST", &format!("{P}/statuses/{rev}"), Some(json!({ "state": state, "context": context, "description": format!("{context} {state}") })), None).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

async fn broken(a: &App) -> Vec<Value> {
    a.ok("GET", &format!("{P}/work?label=broken-main"), None).await.as_array().unwrap().clone()
}

async fn wait_broken(a: &App, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    for _ in 0..400 {
        let items = broken(a).await;
        if pred(&items) {
            return items;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("broken-main items never matched: {:?}", broken(a).await);
}

/// External CI failing on the default branch: one broken-main item per context, bisected from
/// the statuses CI already reported (a revert when the culprit is certain, the range when it
/// isn't), deduplicated, and closed by a later success for that context.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_statuses_on_the_default_branch_break_main() {
    let a = setup().await;
    let c1 = land_file(&a, "one", "one.txt", "1\n").await;
    status(&a, &c1, "ci/teamcity", "success").await;
    status(&a, &c1, "ci/jenkins", "success").await;
    let c2 = land_file(&a, "two", "two.txt", "2\n").await;
    status(&a, &c2, "ci/teamcity", "success").await;
    let c3 = land_file(&a, "three breaks it", "three.txt", "3\n").await;

    // teamcity passed on c2 and fails on c3, its child: c3 broke it, and gets a revert.
    status(&a, &c3, "ci/teamcity", "failure").await;
    let items = wait_broken(&a, |i| i.len() == 1 && !i[0]["changes"].as_array().unwrap().is_empty()).await;
    let item = items[0].clone();
    assert_eq!(item["labels"], json!(["broken-main", "status:ci/teamcity"]));
    assert_eq!(item["priority"], 0);
    assert_eq!(item["state"], "open", "the revert is a suggestion without auto_revert");
    assert!(item["title"].as_str().unwrap().contains("ci/teamcity"), "{item}");
    let body = item["body"].as_str().unwrap();
    assert!(body.contains("CI reported a failure") && body.contains("ci/teamcity failure") && body.contains("Bisected to"), "{body}");
    let revert = a.ok("GET", &format!("/v1/code/changes/{}", item["changes"][0].as_str().unwrap()), None).await;
    assert_eq!(revert["title"], "Revert: three breaks it");
    assert_eq!(revert["author"], "roll");

    // Reported again, or on a later commit: still one item.
    status(&a, &c3, "ci/teamcity", "failure").await;
    let c4 = land_file(&a, "four", "four.txt", "4\n").await;
    status(&a, &c4, "ci/teamcity", "error").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(broken(&a).await.len(), 1);

    // jenkins passed on c1 and nothing is known about c2: the item names the range, no revert.
    status(&a, &c4, "ci/jenkins", "failure").await;
    let items = wait_broken(&a, |i| i.len() == 2).await;
    let jenkins = items.iter().find(|w| w["labels"].as_array().unwrap().contains(&json!("status:ci/jenkins"))).unwrap().clone();
    let body = jenkins["body"].as_str().unwrap();
    assert!(body.contains(&format!("passed at `{c1}`")) && body.contains(&format!("failed at `{c4}`")) && body.contains("3 changesets"), "{body}");
    assert!(jenkins["changes"].as_array().unwrap().is_empty(), "no revert without a certain culprit");

    // A success on an older commit doesn't close anything; one on the failing commit's
    // successor closes that context's item (and roll's unneeded revert), not the other.
    status(&a, &c2, "ci/teamcity", "success").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(broken(&a).await.iter().all(|w| w["state"] != "done"));
    let c5 = land_file(&a, "five fixes it", "three.txt", "fixed\n").await;
    status(&a, &c5, "ci/teamcity", "success").await;
    let items = wait_broken(&a, |i| i.iter().any(|w| w["id"] == item["id"] && w["state"] == "done")).await;
    assert!(items.iter().any(|w| w["id"] == jenkins["id"] && w["state"] == "open"), "jenkins is still broken");
    let revert = a.ok("GET", &format!("/v1/code/changes/{}", revert["id"].as_str().unwrap()), None).await;
    assert_eq!(revert["state"], "closed", "the revert isn't needed any more");

    // A status on a commit that isn't on the default branch (a change's head) breaks nothing.
    let ch = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "x.txt": { "content": "x\n" } } }))).await;
    status(&a, ch["head"].as_str().unwrap(), "ci/buildkite", "failure").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(broken(&a).await.iter().all(|w| !w["labels"].as_array().unwrap().contains(&json!("status:ci/buildkite"))));
}

/// With `[main] auto_revert`, a certain culprit's revert lands at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_statuses_auto_revert() {
    let a = setup().await;
    let c1 = land_file(&a, "settings", "roll.toml", "[main]\nauto_revert = true\n").await;
    status(&a, &c1, "ci", "success").await;
    let c2 = land_file(&a, "bad", "bad.txt", "bad\n").await;
    status(&a, &c2, "ci", "failure").await;
    let items = wait_broken(&a, |i| i.len() == 1 && !i[0]["changes"].as_array().unwrap().is_empty()).await;
    let revert_id = items[0]["changes"][0].as_str().unwrap().to_string();
    for _ in 0..400 {
        if a.ok("GET", &format!("/v1/code/changes/{revert_id}"), None).await["state"] == "landed" {
            let (s, _) = a.raw("GET", &format!("{P}/blob/main/bad.txt"), None, None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "reverted");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the revert never landed");
}

/// Reads `GET …/events`: every event in the order it came; `next(kind)` is the oldest event of
/// `kind` not returned yet.
struct Sse {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    buf: String,
    all: Vec<(String, Value)>,
    taken: Vec<bool>,
}

impl Sse {
    async fn open(a: &App, path: &str) -> Sse {
        let r = a.send(axum::http::Request::get(path).body(axum::body::Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        Sse { body: Box::pin(r.into_body().into_data_stream()), buf: String::new(), all: Vec::new(), taken: Vec::new() }
    }

    async fn next(&mut self, kind: &str) -> Value {
        use futures::StreamExt;
        loop {
            if let Some(i) = (0..self.all.len()).find(|&i| !self.taken[i] && self.all[i].0 == kind) {
                self.taken[i] = true;
                return self.all[i].1.clone();
            }
            while let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let (mut ev, mut data) = (String::new(), String::new());
                for line in block.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        ev = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                if !ev.is_empty() {
                    self.all.push((ev, serde_json::from_str(&data).unwrap()));
                    self.taken.push(false);
                }
            }
            if self.all.iter().zip(&self.taken).any(|((k, _), t)| !t && k == kind) {
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.body.next())
                .await
                .unwrap_or_else(|_| panic!("no {kind} event; got {:?}", self.all.iter().map(|e| &e.0).collect::<Vec<_>>()))
                .unwrap()
                .unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

struct Previews;

#[async_trait::async_trait]
impl code_control::PreviewBackend for Previews {
    async fn create(&self, _r: &code_control::model::RepoRow, c: &code_control::model::Change, _dir: &std::path::Path) -> anyhow::Result<code_control::model::Preview> {
        Ok(code_control::model::Preview {
            state: code_control::model::PreviewState::Running,
            url: Some(format!("http://{}.preview.test", c.id)),
            postgres: None,
            redis: None,
            error: None,
            created_at: 1,
        })
    }
    async fn delete(&self, _r: &code_control::model::RepoRow, _c: &code_control::model::Change) -> anyhow::Result<()> {
        Ok(())
    }
}

fn keys(v: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = v.as_object().unwrap_or_else(|| panic!("not an object: {v}")).keys().map(String::as_str).collect();
    k.sort();
    k
}

fn sorted(mut k: Vec<&str>) -> Vec<&str> {
    k.sort();
    k
}

/// Every event kind on `GET …/events`, with the shape docs/code/http-api.md gives it, and the
/// same data in a `"*"` webhook's deliveries (`{event, delivery, webhook, repo, data}`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_event_is_the_same_on_the_stream_and_in_webhooks() {
    let a = setup().await;
    a.code.set_preview_backend(Arc::new(Previews));
    let rx = receiver().await;
    let created = a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": rx.url, "events": ["*"] }))).await;
    let hook = created["webhook"]["id"].as_str().unwrap().to_string();
    rx.wait("ping", |_| true).await;
    let mut ev = Sse::open(&a, &format!("{P}/events")).await;

    // work = WorkItem
    let w = a.ok("POST", &format!("{P}/work"), Some(json!({ "title": "next", "labels": ["ui"], "priority": 1 }))).await;
    let got = ev.next("work").await;
    assert_eq!(got, w);
    assert_eq!(
        keys(&got),
        sorted(vec!["id", "repo_id", "repo", "number", "title", "body", "labels", "priority", "state", "source", "github_url", "claimed_by", "lease_until", "changes", "created_at", "updated_at"])
    );

    // change = Change
    let c = a
        .ok("POST", &format!("{P}/changes"), Some(json!({ "title": "c", "work_item": w["id"], "message": "c", "files": { "c.txt": { "content": "one\ntwo\n" } } })))
        .await;
    let id = c["id"].as_str().unwrap().to_string();
    let got = ev.next("change").await;
    assert_eq!(got["id"], id);
    for k in [
        "id", "repo_id", "repo", "number", "title", "body", "author", "state", "base", "head", "revision", "work_item", "branch", "github_pr", "github_url", "trusted",
        "run_approved", "draft", "checks", "preview", "landed_as", "conflict", "queue", "blockers", "review", "created_at", "updated_at",
    ] {
        assert!(got.get(k).is_some(), "Change.{k}: {got}");
    }
    assert_eq!(ev.next("work").await["state"], "review");

    // check = {change, check: Check}
    a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "success", "url": "https://ci/1", "summary": "ok" }))).await;
    let got = ev.next("check").await;
    assert_eq!(keys(&got), ["change", "check"]);
    assert_eq!(got["change"], id);
    assert_eq!(keys(&got["check"]), sorted(vec!["name", "status", "source", "url", "summary", "started_at", "finished_at"]));
    assert_eq!((got["check"]["status"].as_str(), got["check"]["source"].as_str()), (Some("success"), Some("external")));

    // check = {changeset, status: Status}: a commit status (on main, the head of no change).
    let posted = a.ok("POST", &format!("{P}/statuses/main"), Some(json!({ "state": "pending", "context": "ci/x", "target_url": "https://ci/2", "description": "d" }))).await;
    let got = ev.next("check").await;
    assert_eq!(keys(&got), ["changeset", "status"]);
    assert_eq!(got["changeset"], posted["changeset"]);
    assert_eq!(got["status"], posted["status"]);
    assert_eq!(keys(&got["status"]), sorted(vec!["context", "state", "target_url", "description", "creator", "created_at", "updated_at"]));
    assert_eq!((got["status"]["context"].as_str(), got["status"]["state"].as_str()), (Some("ci/x"), Some("pending")));

    // preview = {change, preview: Preview | null}
    a.ok("POST", &format!("/v1/code/changes/{id}/preview"), None).await;
    let got = ev.next("preview").await;
    assert_eq!(keys(&got), ["change", "preview"]);
    assert_eq!(got["change"], id);
    assert_eq!(keys(&got["preview"]), sorted(vec!["state", "url", "postgres", "redis", "error", "created_at"]));
    assert_eq!(got["preview"]["state"], "running");
    a.ok("DELETE", &format!("/v1/code/changes/{id}/preview"), None).await;
    assert_eq!(ev.next("preview").await, json!({ "change": id, "preview": null }));

    // review = Review, thread = Thread
    let r = a
        .ok("POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": "comment", "body": "a note", "comments": [{ "path": "c.txt", "line": 2, "body": "why two?" }] })))
        .await;
    let got = ev.next("review").await;
    assert_eq!(got["id"], r["id"]);
    assert_eq!(
        keys(&got),
        sorted(vec!["id", "change", "revision", "head", "reviewer", "verdict", "body", "source", "url", "stale", "dismissed", "dismissed_by", "dismiss_reason", "created_at", "updated_at", "ai"])
    );
    let got = ev.next("thread").await;
    assert_eq!(got["id"], r["threads"][0]["id"]);
    assert_eq!((got["review"].as_str(), got["path"].as_str(), got["line"].as_u64()), (r["id"].as_str(), Some("c.txt"), Some(2)));
    assert_eq!(
        keys(&got),
        sorted(vec![
            "id", "change", "review", "path", "side", "line", "start_line", "revision", "original_line", "original_start_line", "original_revision", "outdated", "resolved",
            "resolved_by", "resolved_at", "source", "created_at", "updated_at", "comments",
        ])
    );
    assert_eq!(got["comments"][0]["body"], "why two?");

    // queue = {state, paused, depth, batch}
    a.ok("POST", &format!("{P}/queue/pause"), None).await;
    assert_eq!(ev.next("queue").await, json!({ "state": "paused", "paused": true, "depth": 0, "batch": null }));
    a.ok("POST", &format!("{P}/queue/resume"), None).await;
    assert_eq!(ev.next("queue").await, json!({ "state": "idle", "paused": false, "depth": 0, "batch": null }));

    // landed = {changeset, git_commit, ref, generation, message, author, change, files, at, count}
    let landed = a.ok("POST", &format!("/v1/code/changes/{id}/land"), None).await;
    let got = ev.next("landed").await;
    assert_eq!(keys(&got), sorted(vec!["changeset", "git_commit", "ref", "generation", "message", "author", "change", "files", "at", "count"]));
    assert_eq!((&got["changeset"], &got["git_commit"], &got["generation"]), (&landed["changeset"], &landed["git_commit"], &landed["generation"]));
    assert_eq!(
        (got["change"].as_str(), got["ref"].as_str(), got["message"].as_str(), got["files"].as_u64(), got["count"].as_u64()),
        (Some(id.as_str()), Some("main"), Some("c"), Some(1), Some(1))
    );

    // push = {ref, before, after, pusher, accepted, reason}: created, refused, deleted.
    let base = a.serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    git(&home, &home, &["clone", "-q", &format!("{base}/git/acme/api.git"), "w"]);
    let wd = home.join("w");
    write(&wd, "p.txt", "p\n");
    git(&home, &wd, &["add", "-A"]);
    git(&home, &wd, &["commit", "-qm", "p"]);
    let sha = git(&home, &wd, &["rev-parse", "HEAD"]).trim().to_string();
    git(&home, &wd, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
    let got = ev.next("push").await;
    assert_eq!(keys(&got), sorted(vec!["ref", "before", "after", "pusher", "accepted", "reason"]));
    assert!(got["pusher"].is_string(), "{got}");
    assert_eq!(got, json!({ "ref": "refs/heads/feature", "before": null, "after": sha, "pusher": got["pusher"], "accepted": true, "reason": null }));
    let out = common::git_raw(&home, &wd, &["push", "-q", "origin", "HEAD:refs/notes/x"]);
    assert!(!out.status.success());
    let got = ev.next("push").await;
    assert_eq!((got["ref"].as_str(), got["accepted"].as_bool()), (Some("refs/notes/x"), Some(false)));
    assert!(got["reason"].as_str().unwrap().contains("only branches"), "{got}");
    git(&home, &wd, &["push", "-q", "origin", ":refs/heads/feature"]);
    let got = ev.next("push").await;
    assert_eq!(
        (got["ref"].as_str(), got["before"].as_str(), &got["after"], got["accepted"].as_bool()),
        (Some("refs/heads/feature"), Some(sha.as_str()), &Value::Null, Some(true))
    );

    // Every kind came, and each event reached the webhook with the same data.
    let mut kinds: Vec<&str> = ev.all.iter().map(|(k, _)| k.as_str()).collect();
    kinds.sort();
    kinds.dedup();
    assert_eq!(kinds, sorted(code_control::webhooks::EVENTS.to_vec()));
    for (kind, data) in &ev.all {
        let g = rx.wait(kind, |g| g.body["data"] == *data).await;
        assert_eq!(keys(&g.body), ["data", "delivery", "event", "repo", "webhook"]);
        assert_eq!((g.body["event"].as_str(), g.body["webhook"].as_str()), (Some(kind.as_str()), Some(hook.as_str())));
        assert_eq!(g.body["delivery"], g.headers["x-roll-delivery"].to_str().unwrap());
        assert_eq!(keys(&g.body["repo"]), ["default_branch", "full_name", "id", "visibility"]);
    }
}

// ── many nodes on one database (docs/code/cluster.md, "Work that isn't per repo") ──

/// Another node on `a`'s databases (control.db and the store), with connections of its own.
fn node(a: &App) -> App {
    let code = common::open_in(a.dir.path(), |c| c.review_app_retry_ms = 20);
    App { router: code_control::api::router(code.clone()), code, dir: tempfile::tempdir().unwrap() }
}

/// A receiver that counts attempts per delivery id: it fails each delivery's first attempt
/// (`fail_first`) and holds each first attempt `slow_first` long before answering.
#[derive(Clone)]
struct Counter {
    url: String,
    attempts: Arc<Mutex<HashMap<String, u32>>>,
    /// When each delivery's first attempt arrived.
    first_at: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    /// Bodies of the attempts it answered 200.
    ok: Arc<Mutex<Vec<Value>>>,
}

async fn counter(fail_first: bool, slow_first: Option<Duration>) -> Counter {
    let attempts: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
    let ok: Arc<Mutex<Vec<Value>>> = Arc::default();
    let first_at: Arc<Mutex<HashMap<String, std::time::Instant>>> = Arc::default();
    let (at, okc, fa) = (attempts.clone(), ok.clone(), first_at.clone());
    let app = axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
        let (attempts, ok, first_at) = (at.clone(), okc.clone(), fa.clone());
        async move {
            let d = headers.get("x-roll-delivery").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            first_at.lock().entry(d.clone()).or_insert_with(std::time::Instant::now);
            let n = {
                let mut a = attempts.lock();
                let n = a.entry(d).or_insert(0);
                *n += 1;
                *n
            };
            if n == 1
                && let Some(s) = slow_first
            {
                tokio::time::sleep(s).await;
            }
            if n == 1 && fail_first {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            ok.lock().push(serde_json::from_slice(&body).unwrap_or(Value::Null));
            StatusCode::OK.into_response()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Counter { url, attempts, first_at, ok }
}

impl Counter {
    fn answered(&self, event: &str) -> Vec<Value> {
        self.ok.lock().iter().filter(|b| b["event"] == event).cloned().collect()
    }
    async fn wait(&self, event: &str, n: usize, secs: u64) {
        for _ in 0..secs * 100 {
            if self.answered(event).len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} of {n} {event} deliveries", self.answered(event).len());
    }
}

/// A webhook on `P` made through `a`; its id.
async fn hook(a: &App, url: &str, events: Value) -> String {
    let created = a.ok("POST", &format!("{P}/webhooks"), Some(json!({ "url": url, "events": events }))).await;
    created["webhook"]["id"].as_str().unwrap().to_string()
}

/// Every landed changeset delivered exactly once.
fn each_once(got: &[Value], want: &[String]) {
    let mut seen: Vec<String> = got.iter().map(|b| b["data"]["changeset"].as_str().unwrap().to_string()).collect();
    seen.sort();
    let mut want = want.to_vec();
    want.sort();
    assert_eq!(seen.len(), want.len(), "each landing once");
    assert_eq!(seen, want, "every landing, each once");
}

/// Land `n` more changesets on main in one advance; their ids.
async fn land_burst(a: &App, repo_id: &str, n: usize) -> Vec<String> {
    let head = a.code.store.get_ref(repo_id, "refs/heads/main").unwrap().unwrap();
    let tip = chain(a, repo_id, head, n).await;
    let landed = a.code.store.advance(repo_id, "main", Some(head), tip).await.unwrap();
    assert_eq!(landed.len(), n);
    landed.iter().map(|l| code_control::cs_id(&l.changeset)).collect()
}

/// Run a node's forwarder once, off the runtime's threads (it blocks on the database).
async fn forward(a: &App) -> code_control::Result<code_control::webhooks::Forwarded> {
    let code = a.code.clone();
    tokio::task::spawn_blocking(move || code.forward_landings()).await.unwrap()
}

/// Two nodes forwarding the same land log at once (their background forwarders, plus four
/// more rounds racing in a loop) while landings keep arriving: each landing is queued and
/// delivered exactly once per webhook. Each page's deliveries go in with the cursor's
/// compare-and-swap, so the loser of a race queues nothing and re-reads from the new cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_forwarders_deliver_each_landing_once() {
    let a = setup().await;
    let b = node(&a);
    let rx = counter(false, None).await;
    let id = hook(&b, &rx.url, json!(["landed"])).await;
    rx.wait("ping", 1, 10).await;
    a.code.start_webhooks();
    b.code.start_webhooks();
    let repo_id = a.ok("GET", P, None).await["id"].as_str().unwrap().to_string();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut racers = Vec::new();
    for code in [a.code.clone(), b.code.clone(), a.code.clone(), b.code.clone()] {
        let stop = stop.clone();
        racers.push(tokio::task::spawn_blocking(move || {
            let mut rounds = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                code.forward_landings().unwrap();
                rounds += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            rounds
        }));
    }
    let mut want = Vec::new();
    for _ in 0..6 {
        want.extend(land_burst(&a, &repo_id, 200).await);
    }
    // Every landing forwarded, then stop the racers.
    let head = a.code.store.land_log_head().unwrap();
    for _ in 0..1000 {
        if a.code.db.land_cursor().unwrap() == Some(head) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(b.code.db.land_cursor().unwrap(), Some(head), "the cursor reached the head");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for r in racers {
        assert!(r.await.unwrap() > 0);
    }
    rx.wait("landed", want.len(), 60).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    each_once(&rx.answered("landed"), &want);
    let queued = a.code.db.deliveries(&id, 100_000).unwrap();
    assert_eq!(queued.iter().filter(|d| d.event == "landed").count(), want.len(), "queued once each");
    assert!(queued.iter().all(|d| d.state == "delivered" && d.attempts == 1), "and sent once each");
}

/// A forwarder that fails after queueing a page's deliveries (as a crash would) leaves nothing
/// of the page behind and the cursor where it was; the pages before it stay forwarded, and the
/// next round forwards the rest: nothing is dropped and nothing is queued twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forwarder_failing_mid_page_neither_drops_nor_duplicates() {
    let a = setup().await;
    let rx = counter(false, None).await;
    let id = hook(&a, &rx.url, json!(["landed"])).await;
    rx.wait("ping", 1, 10).await;
    let repo_id = a.ok("GET", P, None).await["id"].as_str().unwrap().to_string();
    let landed = |a: &App| a.code.db.deliveries(&id, 100_000).unwrap().into_iter().filter(|d| d.event == "landed").count();
    // A full page that goes through.
    let mut want = land_burst(&a, &repo_id, 500).await;
    rx.wait("landed", 500, 60).await;
    let page1 = a.code.db.land_cursor().unwrap().unwrap();
    // Then every round fails after queueing.
    a.code.fail_land_forwarding(u32::MAX);
    let more = land_burst(&a, &repo_id, 200).await;
    for _ in 0..3 {
        assert!(forward(&a).await.is_err(), "the injected failure");
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a.code.db.land_cursor().unwrap(), Some(page1), "the cursor didn't move");
    assert_eq!(landed(&a), 500, "nothing of the failed page was kept");
    assert_eq!(rx.answered("landed").len(), 500);
    // One more failure, then it works.
    a.code.fail_land_forwarding(1);
    let head = a.code.store.land_log_head().unwrap();
    for _ in 0..100 {
        let _ = forward(&a).await;
        if a.code.db.land_cursor().unwrap() == Some(head) {
            break;
        }
    }
    assert_eq!(a.code.db.land_cursor().unwrap(), Some(head));
    want.extend(more);
    rx.wait("landed", want.len(), 60).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(landed(&a), want.len(), "queued once each");
    each_once(&rx.answered("landed"), &want);
}

/// Which webhooks an event goes to is read from the database, not a node's memory: a webhook
/// made through one node gets the events emitted on another at once, a change to it applies at
/// once, and one deleted through one node stops deliveries from every node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhooks_made_on_one_node_apply_to_events_on_another() {
    let a = setup().await;
    let b = node(&a);
    let rx = counter(false, None).await;
    let change = |n: usize| json!({ "title": format!("t{n}"), "message": "t", "base": "main", "files": { format!("c{n}.txt"): { "content": "c\n" } } });
    // `a` emits (and looks for webhooks) before any exists.
    a.ok("POST", &format!("{P}/changes"), Some(change(0))).await;
    let id = hook(&b, &rx.url, json!(["change", "landed"])).await;
    rx.wait("ping", 1, 10).await;
    // An event on a: delivered to b's webhook.
    let c = a.ok("POST", &format!("{P}/changes"), Some(change(1))).await;
    for _ in 0..1000 {
        if rx.answered("change").iter().any(|v| v["data"]["id"] == c["id"]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(rx.answered("change").iter().any(|v| v["data"]["id"] == c["id"]), "{:?}", rx.answered("change"));
    // A landing through a: once.
    let cs = land_file(&a, "landed through a", "l.txt", "1\n").await;
    rx.wait("landed", 1, 10).await;
    // Changed through b: a stops sending `change` at once.
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.ok("PATCH", &format!("{P}/webhooks/{id}"), Some(json!({ "events": ["landed"] }))).await;
    let changes = rx.answered("change").len();
    a.ok("POST", &format!("{P}/changes"), Some(change(2))).await;
    // Deleted through b: nothing more from a.
    let (s, _) = b.req("DELETE", &format!("{P}/webhooks/{id}"), None, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    a.ok("POST", &format!("{P}/changes"), Some(change(3))).await;
    land_file(&a, "after the delete", "l.txt", "2\n").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rx.answered("change").len(), changes, "no change events after the update, nor after the delete");
    let landed = rx.answered("landed");
    assert_eq!(landed.len(), 1, "one landing, delivered once, none after the delete");
    assert_eq!(landed[0]["data"]["changeset"], cs.as_str());
    // A node taking a repo over can drop what it knew about its webhooks (nothing is kept).
    b.code.forget_repo_webhooks(a.ok("GET", P, None).await["id"].as_str().unwrap());
}

/// Two delivery workers (one per node) over one delivery queue: each delivery is sent by one
/// of them at a time, so with every first attempt failing, the receiver gets each delivery
/// exactly twice (the failure and the retry), whichever node sends it, and the log says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_delivery_workers_send_each_delivery_once_including_retries() {
    let a = setup().await;
    let b = node(&a);
    let rx = counter(true, None).await;
    let id = hook(&a, &rx.url, json!(["change"])).await;
    for n in 0..20 {
        let node = if n % 2 == 0 { &a } else { &b };
        node.ok("POST", &format!("{P}/changes"), Some(json!({ "title": format!("t{n}"), "message": "t", "base": "main", "files": { format!("c{n}.txt"): { "content": "c\n" } } }))).await;
    }
    let mut ds = Vec::new();
    for _ in 0..1000 {
        ds = a.code.db.deliveries(&id, 1000).unwrap();
        if ds.len() >= 21 && ds.iter().all(|d| d.state == "delivered") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ds.len() >= 21 && ds.iter().all(|d| d.state == "delivered"), "{:?}", ds.iter().map(|d| (&d.state, d.attempts)).collect::<Vec<_>>());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ds = a.code.db.deliveries(&id, 1000).unwrap();
    assert!(ds.iter().all(|d| d.state == "delivered"));
    let attempts = rx.attempts.lock().clone();
    for d in &ds {
        assert_eq!(d.attempts, 2, "{} {}", d.id, d.event);
        assert_eq!(attempts.get(&d.id), Some(&2), "{} was sent exactly twice (a failure, then the retry)", d.id);
    }
    assert_eq!(attempts.len(), ds.len());
}

/// A delivery claimed by a node that died before sending it is due again once the claim
/// lapses, and another node sends it then (once).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claim_held_by_a_dead_node_is_retried_after_it_lapses() {
    let a = setup().await;
    let b = node(&a);
    let rx = counter(false, None).await;
    let id = hook(&a, &rx.url, json!(["change"])).await;
    rx.wait("ping", 1, 10).await;
    // The dead node's delivery: queued, then claimed for 400 ms, and never sent.
    let sql = a.code.db.sql();
    let far = i64::MAX / 2;
    sql.execute(
        "INSERT INTO review_deliveries (id, app_id, event, payload, state, next_at, created_at) VALUES ('dl_dead', ?, 'ping', ?, 'pending', ?, 0)",
        code_sql::params![id.as_str(), json!({ "event": "ping", "delivery": "dl_dead" }).to_string(), far],
    )
    .unwrap();
    let claimed = std::time::Instant::now();
    let now = sql.now_ms().unwrap();
    assert!(a.code.db.claim_delivery("dl_dead", far, now + 400).unwrap());
    // b's worker, woken by a ping of its own, sends it when the claim lapses, not before
    // (measured when it arrives: the ping itself may take longer than the claim under load).
    b.ok("POST", &format!("{P}/webhooks/{id}/ping"), None).await;
    rx.wait("ping", 2, 30).await;
    for _ in 0..3000 {
        if rx.attempts.lock().contains_key("dl_dead") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sent_at = *rx.first_at.lock().get("dl_dead").expect("the dead node's delivery was sent");
    assert!(sent_at.duration_since(claimed) >= Duration::from_millis(390), "not while it's claimed: sent {:?} after the claim", sent_at.duration_since(claimed));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rx.attempts.lock().get("dl_dead"), Some(&1), "sent once");
    let d = a.code.db.delivery("dl_dead").unwrap().unwrap();
    assert_eq!((d.state.as_str(), d.attempts), ("delivered", 1));
}

/// A receiver slower than the claim can't get a delivery from two senders at once: the
/// request's total timeout is well inside the claim, so the slow attempt ends (and is recorded
/// as a failed attempt) before anyone else may send; the retry is the only other send, and
/// every send the receiver saw is an attempt in the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_receiver_never_gets_an_unrecorded_second_send() {
    let a = setup().await;
    let b = node(&a);
    for n in [&a, &b] {
        n.code.set_delivery_timing(Duration::from_millis(300), Duration::from_millis(1_500));
    }
    // Each first attempt takes 3 s, twice the claim.
    let rx = counter(false, Some(Duration::from_secs(3))).await;
    let id = hook(&a, &rx.url, json!(["change"])).await;
    b.ok("POST", &format!("{P}/webhooks/{id}/ping"), None).await;
    for _ in 0..500 {
        let ds = a.code.db.deliveries(&id, 10).unwrap();
        if ds.len() == 2 && ds.iter().all(|d| d.state == "delivered") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Past the slow attempts and their claims: no more sends.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let attempts = rx.attempts.lock().clone();
    let ds = a.code.db.deliveries(&id, 10).unwrap();
    assert_eq!(ds.len(), 2);
    for d in &ds {
        assert_eq!((d.state.as_str(), d.attempts), ("delivered", 2), "{}: the timed-out attempt, then the retry", d.id);
        assert_eq!(attempts.get(&d.id), Some(&2), "{}: the receiver saw exactly the recorded attempts", d.id);
    }
}

/// Webhooks created through two nodes at the moment the forwarder starts for the first time
/// (no cursor yet, on either side of the race): every creation succeeds, and each webhook gets
/// the next landing exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn webhooks_created_while_the_forwarder_starts() {
    let a = setup().await;
    let b = node(&a);
    let rx = counter(false, None).await;
    let r = a.code.find_repo("acme", "api").unwrap();
    let repo_id = r.id.clone();
    let mut hooks = Vec::new();
    for _ in 0..5 {
        a.code.db.sql().execute("DELETE FROM webhook_land_cursor", ()).unwrap();
        let mut tasks = Vec::new();
        for i in 0..4 {
            let (code, url, r) = (if i % 2 == 0 { a.code.clone() } else { b.code.clone() }, rx.url.clone(), r.clone());
            tasks.push(tokio::task::spawn_blocking(move || code.create_webhook(&code_control::Caller::Admin, &r, &json!({ "url": url, "events": ["landed"] }))));
        }
        let forwarders: Vec<_> = [a.code.clone(), b.code.clone()].into_iter().map(|c| tokio::task::spawn_blocking(move || c.forward_landings())).collect();
        for t in tasks {
            let v = t.await.unwrap().expect("created");
            hooks.push(v["webhook"]["id"].as_str().unwrap().to_string());
        }
        for f in forwarders {
            f.await.unwrap().unwrap();
        }
    }
    let cs = land_burst(&a, &repo_id, 1).await;
    for _ in 0..1000 {
        if rx.answered("landed").len() >= hooks.len() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got = rx.answered("landed");
    assert_eq!(got.len(), hooks.len(), "one landing, once per webhook");
    let mut per: Vec<String> = got.iter().map(|g| g["webhook"].as_str().unwrap().to_string()).collect();
    per.sort();
    hooks.sort();
    assert_eq!(per, hooks);
    assert!(got.iter().all(|g| g["data"]["changeset"] == cs[0].as_str()));
}

fn outbox(a: &App) -> i64 {
    a.code.db.sql().query_row("SELECT COUNT(*) FROM webhook_outbox", (), |r| r.get::<_, i64>(0)).unwrap()
}

/// A node that dies right after a state change commits (before anything else happens for its
/// event) loses no webhook delivery: the state change's transaction promised the event
/// (`webhook_outbox`), and the node running the land forwarder delivers it, once, with the
/// change or review as it is then. Covers a change opened, closed and landed, a review, checks
/// and a commit status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_promised_by_a_state_change_survive_a_crash_after_its_commit() {
    let a = setup().await;
    let rx = counter(false, None).await;
    hook(&a, &rx.url, json!(["change", "review", "check"])).await;
    rx.wait("ping", 1, 30).await;
    let change = |n: usize| json!({ "title": format!("t{n}"), "message": "t", "base": "main", "files": { format!("c{n}.txt"): { "content": "c\n" } } });
    // Every promised event from here on "dies" right after its commit.
    a.code.crash_after_commit(u32::MAX);
    let c1 = a.ok("POST", &format!("{P}/changes"), Some(change(1))).await;
    let c2 = a.ok("POST", &format!("{P}/changes"), Some(change(2))).await;
    let (id1, id2) = (c1["id"].as_str().unwrap().to_string(), c2["id"].as_str().unwrap().to_string());
    a.ok("POST", &format!("/v1/code/changes/{id2}/close"), None).await;
    let review = a.ok("POST", &format!("/v1/code/changes/{id1}/reviews"), Some(json!({ "verdict": "comment", "body": "looks fine" }))).await;
    a.ok("POST", &format!("/v1/code/changes/{id1}/checks"), Some(json!({ "name": "ci", "status": "success" }))).await;
    let (s, v) = a.req("POST", &format!("{P}/statuses/{}", c1["head"].as_str().unwrap()), Some(json!({ "state": "success", "context": "lint" })), None).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let landed = a.ok("POST", &format!("/v1/code/changes/{id1}/land"), None).await;
    assert!(landed.to_string().contains("landed"), "{landed}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    // c1 and c2 opened, c2 closed, the review, the ci check (and c1's change event), the
    // status (and its check on c1, and c1's change event), c1 landed.
    let promised = outbox(&a);
    assert_eq!(promised, 10, "the promises are in the outbox");
    assert_eq!(rx.answered("review").len(), 0, "the review's event died with the node");
    let before = rx.ok.lock().len();
    // Another node (the dead one never comes back) delivers what was promised.
    let b = node(&a);
    b.code.set_outbox_grace(Duration::ZERO);
    b.code.start_webhooks();
    for _ in 0..3000 {
        if outbox(&a) == 0 && rx.ok.lock().len() >= before + promised as usize {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(outbox(&a), 0, "every promise kept");
    assert_eq!(rx.ok.lock().len() - before, promised as usize, "one delivery per promise, none twice");
    assert_eq!(rx.answered("review").iter().filter(|v| v["data"]["id"] == review["id"]).count(), 1, "the review, once");
    let checks = rx.answered("check");
    assert_eq!(checks.iter().filter(|v| v["data"]["change"] == id1.as_str() && v["data"]["check"]["name"] == "ci").count(), 1, "{checks:?}");
    assert_eq!(checks.iter().filter(|v| v["data"]["change"] == id1.as_str() && v["data"]["check"]["name"] == "lint").count(), 1, "{checks:?}");
    assert_eq!(checks.iter().filter(|v| v["data"]["status"]["context"] == "lint").count(), 1, "the commit status, once: {checks:?}");
    // Recovered change events carry the change as it is when they're recovered.
    let state_of = |id: &str, state: &str| rx.answered("change").into_iter().filter(|v| v["data"]["id"] == id && v["data"]["state"] == state).count();
    assert_eq!(state_of(&id2, "closed"), 2, "c2's opening and closing, both read after the close");
    assert!(state_of(&id1, "landed") >= 4, "c1's promised events, read after it landed");
}

/// Promises are kept by the node that made them as soon as it emits the event: nothing waits
/// for the recovery, and nothing is left in the outbox.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promised_events_are_delivered_at_once_and_leave_nothing_behind() {
    let a = setup().await;
    let rx = counter(false, None).await;
    hook(&a, &rx.url, json!(["change"])).await;
    rx.wait("ping", 1, 30).await;
    let c = a.ok("POST", &format!("{P}/changes"), Some(json!({ "title": "t", "message": "t", "base": "main", "files": { "x.txt": { "content": "x\n" } } }))).await;
    a.ok("POST", &format!("/v1/code/changes/{}/close", c["id"].as_str().unwrap()), None).await;
    rx.wait("change", 2, 10).await;
    assert_eq!(outbox(&a), 0);
    let states: Vec<String> = rx.answered("change").iter().map(|v| v["data"]["state"].as_str().unwrap().to_string()).collect();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(states.contains(&"open".to_string()) && states.contains(&"closed".to_string()), "{states:?}");
}
