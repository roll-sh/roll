//! Reviews (docs/code/reviews.md): verdicts and the rules matrix, CODEOWNERS, auto-approval,
//! threads and suggestions, landing gates (direct, git, queue), review requests, review apps and
//! their signed deliveries, events, and the auth matrix of every review route.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use code_control::Caller;
use code_control::review::{Origin, Reviewer};
use common::{App, app_with, git, git_raw, write};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const ROOT: &str = "root-token";

struct T {
    a: App,
    /// acme's token named "deploy" (reviews as the user `acme`).
    acme: String,
    /// acme's token named "agent:fixer" (reviews as the agent `fixer`).
    fixer: String,
    /// An unrelated user's token.
    mallory: String,
}

async fn call(a: &App, method: &str, path: &str, body: Option<Value>, token: Option<&str>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = a.send(req).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
    (status, v)
}

async fn ok(a: &App, method: &str, path: &str, body: Option<Value>, token: Option<&str>, headers: &[(&str, &str)]) -> Value {
    let (s, v) = call(a, method, path, body, token, headers).await;
    assert!(s.is_success(), "{method} {path}: {s} {v}");
    v
}

async fn setup_with(f: impl FnOnce(&mut code_control::Config)) -> T {
    let a = app_with(|c| {
        c.admin_token = Some(ROOT.into());
        c.review_app_retry_ms = 30;
        f(c)
    });
    let r = Some(ROOT);
    for (name, kind) in [("acme", "org"), ("alice", "user"), ("mallory", "user")] {
        ok(&a, "POST", "/v1/code/owners", Some(json!({ "name": name, "kind": kind })), r, &[]).await;
    }
    let tok = |o: &str, n: &str| {
        let a = &a;
        let (o, n) = (o.to_string(), n.to_string());
        async move { ok(a, "POST", &format!("/v1/code/owners/{o}/tokens"), Some(json!({ "name": n })), r, &[]).await["token"].as_str().unwrap().to_string() }
    };
    let acme = tok("acme", "deploy").await;
    let fixer = tok("acme", "agent:fixer").await;
    let mallory = tok("mallory", "laptop").await;
    ok(&a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api" })), r, &[]).await;
    let files = json!({
        "src/lib.rs": { "content": "fn a() {}\n\nfn b() {\n    1\n}\n" },
        "src/util.rs": { "content": "pub fn one() -> u32 {\n    1\n}\n" },
        "docs/guide.md": { "content": "# Guide\n" },
        "notes.txt": { "content": "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n" },
    });
    ok(&a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "base", "files": files })), r, &[]).await;
    T { a, acme, fixer, mallory }
}

async fn setup() -> T {
    setup_with(|_| {}).await
}

/// Open a change writing `files` on main.
async fn change(t: &T, title: &str, author: &str, files: Value) -> Value {
    ok(&t.a, "POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": title, "message": title, "author": author, "base": "main", "files": files })), Some(ROOT), &[]).await
}

async fn get_change(t: &T, id: &str) -> Value {
    ok(&t.a, "GET", &format!("/v1/code/changes/{id}"), None, Some(ROOT), &[]).await
}

fn kinds(c: &Value) -> Vec<String> {
    c["blockers"].as_array().unwrap().iter().map(|b| b["kind"].as_str().unwrap().to_string()).collect()
}

async fn settings(t: &T, patch: Value) -> Value {
    ok(&t.a, "PATCH", "/v1/code/repos/acme/api/review-settings", Some(patch), Some(ROOT), &[]).await
}

async fn review(t: &T, id: &str, verdict: &str, token: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    call(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": verdict, "body": format!("{verdict}!") })), Some(token), headers).await
}

/// Land straight on main as the admin, around any review rules (setup only).
async fn land_admin(t: &T, message: &str, files: Value) -> Value {
    ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": message, "files": files, "bypass": true })), Some(ROOT), &[]).await
}

#[tokio::test]
async fn rules_matrix() {
    let t = setup().await;
    let c = change(&t, "tweak b", "alice", json!({ "src/lib.rs": { "content": "fn a() {}\n\nfn b() {\n    2\n}\n" } })).await;
    let id = c["id"].as_str().unwrap();
    assert_eq!(c["review"], json!({ "state": "pending", "approvals": 0, "required": 0 }));
    assert!(kinds(&c).is_empty(), "nothing is required by default: {c}");

    let s = settings(&t, json!({ "required_approvals": 2 })).await;
    assert_eq!(s["effective"]["required_approvals"], 2);
    assert_eq!(s["settings"]["required_approvals"], 2);
    let c = get_change(&t, id).await;
    assert_eq!(kinds(&c), vec!["needs_approval"]);
    assert_eq!(c["review"]["required"], 2);

    // The author's own approval doesn't count.
    let (s, v) = review(&t, id, "approve", ROOT, &[("x-roll-user", "alice")]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["reviewer"], json!({ "kind": "user", "name": "alice", "app": null }));
    let st = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/review-status"), None, Some(ROOT), &[]).await;
    assert_eq!(st["approvals"][0]["counted"], false);
    assert!(st["approvals"][0]["reason"].as_str().unwrap().contains("author"), "{st}");

    // An org token reviews as the org; a token named agent:x as that agent; X-Roll-Agent too.
    let (_, v) = review(&t, id, "approve", &t.acme, &[]).await;
    assert_eq!(v["reviewer"]["name"], "acme");
    let (_, v) = review(&t, id, "approve", &t.fixer, &[]).await;
    assert_eq!(v["reviewer"], json!({ "kind": "agent", "name": "fixer", "app": null }));
    let c = get_change(&t, id).await;
    assert!(kinds(&c).is_empty(), "{c}");
    assert_eq!(c["review"]["state"], "approved");
    assert_eq!(c["review"]["approvals"], 2);

    // Requesting changes blocks until that reviewer approves again (or it's dismissed).
    let (_, rc) = review(&t, id, "request_changes", &t.acme, &[("x-roll-agent", "linter")]).await;
    assert_eq!(rc["reviewer"]["kind"], "agent");
    let c = get_change(&t, id).await;
    assert_eq!(kinds(&c), vec!["changes_requested"]);
    assert_eq!(c["review"]["state"], "changes_requested");
    // A comment from the same reviewer doesn't lift it.
    let (s, _) = review(&t, id, "comment", &t.acme, &[("x-roll-agent", "linter")]).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(kinds(&get_change(&t, id).await), vec!["changes_requested"]);
    // Dismissed by a writer: gone.
    let d = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews/{}/dismiss", rc["id"].as_str().unwrap()), Some(json!({ "reason": "outdated advice" })), Some(&t.acme), &[]).await;
    assert_eq!(d["dismissed"], true);
    assert_eq!(d["dismissed_by"], "@acme");
    assert!(kinds(&get_change(&t, id).await).is_empty());
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews/{}/dismiss", rc["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "already dismissed");
    // Two change requests from one reviewer: dismissing the latest doesn't revive the older one.
    review(&t, id, "request_changes", &t.acme, &[("x-roll-agent", "linter2")]).await;
    let (_, newer) = review(&t, id, "request_changes", &t.acme, &[("x-roll-agent", "linter2")]).await;
    assert_eq!(kinds(&get_change(&t, id).await), vec!["changes_requested"]);
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews/{}/dismiss", newer["id"].as_str().unwrap()), Some(json!({ "reason": "noise" })), Some(&t.acme), &[]).await;
    assert!(kinds(&get_change(&t, id).await).is_empty(), "the older request stays superseded");
    let audit = ok(&t.a, "GET", "/v1/code/repos/acme/api/audit", None, Some(ROOT), &[]).await;
    assert!(audit.as_array().unwrap().iter().any(|e| e["action"] == "review_dismiss"));

    // A new revision makes approvals stale (dismiss_stale is on by default).
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": "main", "message": "v2", "files": { "src/lib.rs": { "content": "fn a() {}\n\nfn b() {\n    3\n}\n" } } })), Some(ROOT), &[]).await;
    let c = get_change(&t, id).await;
    assert_eq!(kinds(&c), vec!["needs_approval"]);
    let reviews = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/reviews"), None, Some(ROOT), &[]).await;
    assert!(reviews.as_array().unwrap().iter().filter(|r| r["verdict"] == "approve").all(|r| r["stale"] == true));
    settings(&t, json!({ "dismiss_stale": false })).await;
    assert!(kinds(&get_change(&t, id).await).is_empty(), "stale approvals count again");
    settings(&t, json!({ "dismiss_stale": true, "allow_self_approval": true, "required_approvals": 1 })).await;
    review(&t, id, "approve", ROOT, &[("x-roll-user", "alice")]).await;
    assert!(kinds(&get_change(&t, id).await).is_empty(), "allow_self_approval");

    // Bad input.
    for (body, want) in [
        (json!({}), StatusCode::BAD_REQUEST),
        (json!({ "verdict": "merge" }), StatusCode::BAD_REQUEST),
        (json!({ "verdict": "request_changes" }), StatusCode::BAD_REQUEST),
        (json!({ "verdict": "comment", "body": 5 }), StatusCode::BAD_REQUEST),
        (json!({ "verdict": "comment", "body": "x".repeat(70_000) }), StatusCode::BAD_REQUEST),
        (json!({ "verdict": "approve", "comments": [{ "path": "nope.rs", "line": 1, "body": "?" }] }), StatusCode::BAD_REQUEST),
    ] {
        let (s, v) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews"), Some(body.clone()), Some(ROOT), &[]).await;
        assert_eq!(s, want, "{body}: {v}");
    }
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": "approve" })), Some(ROOT), &[("x-roll-agent", "bad name!")]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    for p in [json!({ "required_approvals": 99 }), json!({ "bogus": true }), json!({ "auto_approve": [{}] }), json!({ "dismiss_stale": "yes" })] {
        let (s, _) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/review-settings", Some(p.clone()), Some(ROOT), &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{p}");
    }

    // A landed change takes no reviews.
    let landed = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/land"), Some(json!({})), Some(ROOT), &[]).await;
    assert_eq!(landed["status"], "landed");
    let (s, _) = review(&t, id, "approve", &t.acme, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn ai_approvals_and_land_change_gate() {
    let t = setup().await;
    settings(&t, json!({ "required_approvals": 1 })).await;
    let c = change(&t, "x", "alice", json!({ "src/util.rs": { "content": "pub fn one() -> u32 {\n    11\n}\n" } })).await;
    let id = c["id"].as_str().unwrap();
    let (s, v) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/land"), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["blockers"][0]["kind"], "needs_approval");
    // An owner can't bypass reviews; the admin can (audited).
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/land"), Some(json!({ "bypass": true })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // roll's reviewer approving doesn't count until count_ai_approval.
    let (r, cv) = t.a.code.readable_change(&Caller::Admin, id).unwrap();
    let rv = t.a.code.submit_review(&r, &cv, &Reviewer::roll(), &json!({ "verdict": "approve", "body": "LGTM" }), &Origin::ROLL).await.unwrap();
    assert_eq!(rv.reviewer.kind, code_control::review::ReviewerKind::Roll);
    assert_eq!(kinds(&get_change(&t, id).await), vec!["needs_approval"]);
    settings(&t, json!({ "count_ai_approval": true })).await;
    assert!(kinds(&get_change(&t, id).await).is_empty());
    let landed = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/land"), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(landed["status"], "landed");

    // Admin bypass of reviews lands and is audited.
    let c2 = change(&t, "y", "alice", json!({ "src/util.rs": { "content": "pub fn one() -> u32 {\n    12\n}\n" } })).await;
    settings(&t, json!({ "count_ai_approval": false })).await;
    let v = ok(&t.a, "POST", &format!("/v1/code/changes/{}/land", c2["id"].as_str().unwrap()), Some(json!({ "bypass": true })), Some(ROOT), &[]).await;
    assert_eq!(v["status"], "landed");
    let audit = ok(&t.a, "GET", "/v1/code/repos/acme/api/audit", None, Some(ROOT), &[]).await;
    assert!(audit.as_array().unwrap().iter().any(|e| e["action"] == "review_bypass"), "{audit}");
}

#[tokio::test]
async fn codeowners_auto_approve_and_the_review_file() {
    let t = setup().await;
    land_admin(
        &t,
        "rules",
        json!({
            ".roll/CODEOWNERS": { "content": "*       @acme\n/src/   agent:fixer\n/docs/\n" },
            ".roll/review.yaml": { "content": "required_approvals: 0\nrequire_codeowners: true\nauto_approve:\n  - paths: [\"docs/**\", \"**/*.md\"]\n  - comment_only: true\nreviewer:\n  mode: auto\nsurprise: 1\n" },
        }),
    )
    .await;
    settings(&t, json!({ "required_approvals": 1 })).await;
    let s = ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(&t.mallory), &[]).await;
    assert_eq!(s["effective"]["required_approvals"], 1, "the file can't lower the repo's settings");
    assert_eq!(s["effective"]["require_codeowners"], true);
    assert_eq!(s["codeowners"]["path"], ".roll/CODEOWNERS");
    assert_eq!(s["codeowners"]["rules"].as_array().unwrap().len(), 3);
    assert_eq!(s["file"]["reviewer"]["mode"], "auto");
    assert!(s["file"]["errors"][0].as_str().unwrap().contains("surprise"), "{s}");

    // Code under src/ needs the agent fixer (owners of /src/) and one approval.
    let c = change(&t, "util", "alice", json!({ "src/util.rs": { "content": "pub fn one() -> u32 {\n    2\n}\n" } })).await;
    let id = c["id"].as_str().unwrap();
    let st = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/review-status"), None, Some(ROOT), &[]).await;
    assert_eq!(st["codeowners"][0]["path_pattern"], "/src/");
    assert_eq!(st["codeowners"][0]["paths"], json!(["src/util.rs"]));
    assert_eq!(st["blockers"].as_array().unwrap().iter().map(|b| b["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["needs_approval", "needs_codeowner"]);
    assert!(st["blockers"][1]["message"].as_str().unwrap().contains("agent:fixer"));
    review(&t, id, "approve", &t.acme, &[]).await;
    assert_eq!(kinds(&get_change(&t, id).await), vec!["needs_codeowner"], "acme isn't an owner of /src/");
    review(&t, id, "approve", &t.fixer, &[]).await;
    assert!(kinds(&get_change(&t, id).await).is_empty());

    // Docs only: auto-approved, no reviews needed.
    let d = change(&t, "docs", "alice", json!({ "docs/guide.md": { "content": "# Guide\n\nMore.\n" }, "README.md": { "content": "hi\n" } })).await;
    let d = get_change(&t, d["id"].as_str().unwrap()).await;
    assert!(kinds(&d).is_empty(), "{d}");
    assert_eq!(d["review"]["state"], "auto_approved");
    let st = ok(&t.a, "GET", &format!("/v1/code/changes/{}/review-status", d["id"].as_str().unwrap()), None, Some(ROOT), &[]).await;
    assert_eq!(st["auto_approved"]["rule"], "paths: docs/**, **/*.md");

    // Comment-only Rust: auto-approved. A real code change next to it: not.
    let cm = change(&t, "comment", "alice", json!({ "src/lib.rs": { "content": "// The a function.\nfn a() {}\n\nfn b() {\n    1\n}\n" } })).await;
    let cm = get_change(&t, cm["id"].as_str().unwrap()).await;
    assert_eq!(cm["review"]["state"], "auto_approved", "{cm}");
    let st = ok(&t.a, "GET", &format!("/v1/code/changes/{}/review-status", cm["id"].as_str().unwrap()), None, Some(ROOT), &[]).await;
    assert_eq!(st["auto_approved"]["rule"], "comment_only");
    let mixed = change(&t, "mixed", "alice", json!({ "src/lib.rs": { "content": "// The a function.\nfn a() { 0; }\n\nfn b() {\n    1\n}\n" } })).await;
    let mixed = ok(&t.a, "GET", &format!("/v1/code/changes/{}/review-status", mixed["id"].as_str().unwrap()), None, Some(ROOT), &[]).await;
    assert!(mixed["auto_approved"].is_null());
    // A requested change still blocks an auto-approved one.
    review(&t, d["id"].as_str().unwrap(), "request_changes", &t.acme, &[]).await;
    assert_eq!(kinds(&get_change(&t, d["id"].as_str().unwrap()).await), vec!["changes_requested"]);

    // The code-owner path list covers everything since the merge base, not just the last commit.
    let stack = change(&t, "stack 1", "alice", json!({ "src/util.rs": { "content": "pub fn one() -> u32 {\n    5\n}\n" } })).await;
    let sid = stack["id"].as_str().unwrap();
    ok(&t.a, "POST", &format!("/v1/code/changes/{sid}/revise"), Some(json!({ "base": stack["head"], "message": "stack 2: docs", "files": { "docs/guide.md": { "content": "# Guide 2\n" } } })), Some(ROOT), &[]).await;
    let st = ok(&t.a, "GET", &format!("/v1/code/changes/{sid}/review-status"), None, Some(ROOT), &[]).await;
    let paths: Vec<&str> = st["codeowners"].as_array().unwrap().iter().flat_map(|g| g["paths"].as_array().unwrap().iter().map(|p| p.as_str().unwrap())).collect();
    assert!(paths.contains(&"src/util.rs"), "a trailing docs commit can't hide the code change: {st}");
    assert!(st["auto_approved"].is_null());

    // A broken review file falls back to the repo's settings and says why.
    land_admin(&t, "break", json!({ ".roll/review.yaml": { "content": "required_approvals: [\n" } })).await;
    let s = ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(ROOT), &[]).await;
    assert!(s["file"]["rules"].is_null());
    assert!(!s["file"]["errors"].as_array().unwrap().is_empty());
    assert_eq!(s["effective"]["required_approvals"], 1);
    assert_eq!(s["effective"]["require_codeowners"], false);
}

#[tokio::test]
async fn threads_suggestions_and_carry_forward() {
    let t = setup().await;
    let c = change(&t, "notes", "alice", json!({ "notes.txt": { "content": "1\n2\n3\n4\nfive\n6\n7\n8\n9\n10\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    let th = |b: Value| {
        let (a, id) = (&t.a, id.clone());
        async move { call(a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(b), Some(ROOT), &[]).await }
    };
    // Validation.
    for (b, want) in [
        (json!({ "path": "nope.txt", "line": 1, "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "path": "notes.txt", "line": 99, "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "path": "notes.txt", "line": 0, "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "path": "notes.txt", "line": 2, "start_line": 3, "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "path": "notes.txt", "line": 2, "body": "   " }), StatusCode::BAD_REQUEST),
        (json!({ "path": "notes.txt", "line": 2, "side": "middle", "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "path": "../etc/passwd", "line": 1, "body": "?" }), StatusCode::BAD_REQUEST),
        (json!({ "line": 1, "body": "?" }), StatusCode::BAD_REQUEST),
    ] {
        let (s, v) = th(b.clone()).await;
        assert_eq!(s, want, "{b}: {v}");
    }
    // A suggestion on line 5, a multi-line one on 8-9, a plain one on 2, one on the old side.
    let (s, five) = th(json!({ "path": "notes.txt", "line": 5, "body": "spell it\n```suggestion\n5\n```" })).await;
    assert_eq!(s, StatusCode::CREATED, "{five}");
    assert_eq!(five["comments"][0]["suggestion"], "5");
    assert_eq!((five["line"].as_u64(), five["start_line"].as_u64(), five["revision"].as_u64()), (Some(5), Some(5), Some(1)));
    let (_, multi) = th(json!({ "path": "notes.txt", "start_line": 8, "line": 9, "body": "```suggestion\neight\nnine\nnine and a half\n```" })).await;
    let (_, plain) = th(json!({ "path": "notes.txt", "line": 2, "body": "why 2?" })).await;
    let (_, old) = th(json!({ "path": "notes.txt", "line": 5, "side": "old", "body": "```suggestion\nV\n```" })).await;
    assert_eq!(old["side"], "old");
    let threads = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/threads"), None, None, &[]).await;
    assert_eq!(threads.as_array().unwrap().len(), 4, "public repo: anyone reads threads");

    // Reply, resolve, unresolve; require_resolved_threads blocks while any is open.
    let r = ok(&t.a, "POST", &format!("/v1/code/threads/{}/comments", plain["id"].as_str().unwrap()), Some(json!({ "body": "because" })), Some(&t.fixer), &[]).await;
    assert_eq!(r["comments"].as_array().unwrap().len(), 2);
    assert_eq!(r["comments"][1]["author"]["name"], "fixer");
    settings(&t, json!({ "require_resolved_threads": true })).await;
    let c = get_change(&t, &id).await;
    assert_eq!(kinds(&c), vec!["unresolved_threads"]);
    assert!(c["blockers"][0]["message"].as_str().unwrap().starts_with("4 unresolved"));
    let v = ok(&t.a, "POST", &format!("/v1/code/threads/{}/resolve", plain["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!((v["resolved"].as_bool(), v["resolved_by"].as_str()), (Some(true), Some("@acme")));
    let v = ok(&t.a, "POST", &format!("/v1/code/threads/{}/unresolve", plain["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(v["resolved"], false);
    ok(&t.a, "POST", &format!("/v1/code/threads/{}/resolve", plain["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;

    // Applying a suggestion: a new revision with the lines replaced; the thread resolves.
    let (s, v) = call(&t.a, "POST", &format!("/v1/code/threads/{}/apply", plain["id"].as_str().unwrap()), Some(json!({})), Some(ROOT), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "no suggestion: {v}");
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/threads/{}/apply", old["id"].as_str().unwrap()), Some(json!({})), Some(ROOT), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "the old side can't be changed");
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/threads/{}/apply", five["id"].as_str().unwrap()), Some(json!({})), Some(&t.mallory), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "applying writes code");
    let base = get_change(&t, &id).await["base"].clone();
    let v = ok(&t.a, "POST", &format!("/v1/code/threads/{}/apply", five["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(v["change"]["revision"], 2);
    assert_eq!(v["change"]["base"], base, "a revision stacked on the head keeps the change's base");
    assert_eq!(v["thread"]["resolved"], true);
    let text = |rev: &str| {
        let a = &t.a;
        let rev = rev.to_string();
        async move { String::from_utf8(a.raw("GET", &format!("/v1/code/repos/acme/api/blob/{rev}/notes.txt"), None, Some(ROOT)).await.1).unwrap() }
    };
    assert_eq!(text(v["change"]["head"].as_str().unwrap()).await, "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    // The multi-line thread was carried to revision 2 (its lines didn't change) and still applies.
    let m = ok(&t.a, "GET", &format!("/v1/code/threads/{}", multi["id"].as_str().unwrap()), None, None, &[]).await;
    assert_eq!((m["revision"].as_u64(), m["line"].as_u64(), m["outdated"].as_bool()), (Some(2), Some(9), Some(false)));
    let v = ok(&t.a, "POST", &format!("/v1/code/threads/{}/apply", multi["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(text(v["change"]["head"].as_str().unwrap()).await, "1\n2\n3\n4\n5\n6\n7\neight\nnine\nnine and a half\n10\n");

    // Lines inserted above move a thread down; changing its line makes it outdated.
    let (_, ten) = th(json!({ "path": "notes.txt", "line": 11, "body": "```suggestion\nTEN\n```" })).await;
    let head = get_change(&t, &id).await["head"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "prepend", "files": { "notes.txt": { "content": "0\n1\n2\n3\n4\n5\n6\n7\neight\nnine\nnine and a half\n10\n" } } })), Some(ROOT), &[]).await;
    let moved = ok(&t.a, "GET", &format!("/v1/code/threads/{}", ten["id"].as_str().unwrap()), None, None, &[]).await;
    assert_eq!((moved["line"].as_u64(), moved["original_line"].as_u64(), moved["revision"].as_u64()), (Some(12), Some(11), Some(4)));
    let head = get_change(&t, &id).await["head"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "edit 10", "files": { "notes.txt": { "content": "0\n1\n2\n3\n4\n5\n6\n7\neight\nnine\nnine and a half\n10!\n" } } })), Some(ROOT), &[]).await;
    let gone = ok(&t.a, "GET", &format!("/v1/code/threads/{}", ten["id"].as_str().unwrap()), None, None, &[]).await;
    assert_eq!(gone["outdated"], true);
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/threads/{}/apply", ten["id"].as_str().unwrap()), Some(json!({})), Some(ROOT), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "an outdated suggestion doesn't apply");
    // Outdated threads still count as unresolved.
    assert!(kinds(&get_change(&t, &id).await).contains(&"unresolved_threads".to_string()));
    // Four stacked revisions later, the base is still where the change left main.
    assert_eq!(get_change(&t, &id).await["base"], base);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_landings_and_pushes_need_review() {
    let t = setup().await;
    land_admin(&t, "owners", json!({ ".roll/review.yaml": { "content": "auto_approve:\n  - paths: [\"docs/**\"]\n" } })).await;
    settings(&t, json!({ "required_approvals": 1 })).await;
    let (s, v) = call(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "sneak", "files": { "src/util.rs": { "content": "x\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["blockers"][0]["kind"], "review_required");
    assert!(v["blockers"][0]["message"].as_str().unwrap().contains("change #"), "{v}");
    assert_eq!(v["blockers"][1]["kind"], "needs_approval");
    let changes = ok(&t.a, "GET", "/v1/code/repos/acme/api/changes?state=open", None, Some(ROOT), &[]).await;
    assert!(changes.as_array().unwrap().iter().any(|c| c["title"] == "sneak"), "the write became a change");
    // Owners can't bypass; docs-only (auto-approved) lands directly; other branches aren't gated.
    let (s, _) = call(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "sneak", "bypass": true, "files": { "src/util.rs": { "content": "x\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let v = ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "docs", "files": { "docs/new.md": { "content": "new\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(v["status"], "landed");
    let v = ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "feature", "ref": "feature", "base": "main", "files": { "src/util.rs": { "content": "x\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(v["status"], "landed");

    // git push to main: rejected, and the push is a change to review.
    let url = t.a.serve().await;
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let w = home.join("w");
    let remote = url.replace("http://", &format!("http://x:{}@", t.acme));
    git(home, home, &["clone", "-q", &format!("{remote}/git/acme/api.git"), "w"]);
    write(&w, "src/util.rs", "pushed\n");
    git(home, &w, &["commit", "-qam", "pushed to main"]);
    let out = git_raw(home, &w, &["push", "origin", "HEAD:main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("review required: opened as change #"), "{err}");
    assert!(err.contains("needs 1 approval"), "{err}");
    let out = git_raw(home, &w, &["push", "origin", "HEAD:refs/land/main"]);
    assert!(!out.status.success(), "refs/land/main is gated too");
    let changes = ok(&t.a, "GET", "/v1/code/repos/acme/api/changes?state=open", None, Some(ROOT), &[]).await;
    let pushed = changes.as_array().unwrap().iter().find(|c| c["title"] == "pushed to main").expect("the push opened a change").clone();
    // Approve it and it lands.
    review(&t, pushed["id"].as_str().unwrap(), "approve", &t.fixer, &[]).await;
    let v = ok(&t.a, "POST", &format!("/v1/code/changes/{}/land", pushed["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(v["status"], "landed");
}

#[tokio::test]
async fn the_queue_waits_for_approval() {
    let t = setup().await;
    ok(&t.a, "PATCH", "/v1/code/repos/acme/api", Some(json!({ "land_policy": "queue" })), Some(ROOT), &[]).await;
    settings(&t, json!({ "required_approvals": 1 })).await;
    let c = change(&t, "q", "alice", json!({ "src/util.rs": { "content": "q\n" } })).await;
    let id = c["id"].as_str().unwrap();
    let (s, v) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/enqueue"), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["blockers"][0]["kind"], "needs_approval");
    review(&t, id, "approve", &t.fixer, &[]).await;
    let (s, v) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/enqueue"), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    // A new revision makes the approval stale: the queued change is blocked again (the queue
    // ejects it when it gets to it).
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": "main", "message": "q2", "files": { "src/util.rs": { "content": "q2\n" } } })), Some(ROOT), &[]).await;
    let c = get_change(&t, id).await;
    assert_eq!(kinds(&c), vec!["needs_approval"]);
    let r = t.a.code.repo_by_id(c["repo_id"].as_str().unwrap()).unwrap();
    let cv = t.a.code.change_or_404(id).unwrap();
    assert!(t.a.code.mergeable(&r, &cv).await.is_err());
    // On a queue repo, /land queues (and the queue checks reviews).
    let (s, v) = call(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "direct", "files": { "a": { "content": "a" } } })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

#[tokio::test]
async fn review_requests_and_pending_reviews() {
    let t = setup().await;
    let c = change(&t, "r", "alice", json!({ "src/util.rs": { "content": "r\n" } })).await;
    let id = c["id"].as_str().unwrap();
    let v = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "roll", "required": true })), Some(&t.acme), &[]).await;
    assert_eq!(v[0]["reviewer"]["kind"], "roll");
    assert_eq!((v[0]["required"].as_bool(), v[0]["current"].as_bool()), (Some(true), Some(true)));
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "@bob" })), Some(&t.acme), &[]).await;
    let c = get_change(&t, id).await;
    assert_eq!(kinds(&c), vec!["review_pending"], "only required requests block");
    assert!(c["blockers"][0]["message"].as_str().unwrap().contains("roll"));
    for (b, want) in [
        (json!({}), StatusCode::BAD_REQUEST),
        (json!({ "reviewer": "!!" }), StatusCode::BAD_REQUEST),
        (json!({ "reviewer": "app:nobody" }), StatusCode::NOT_FOUND),
        (json!({ "reviewer": "roll", "required": "yes" }), StatusCode::BAD_REQUEST),
    ] {
        let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(b.clone()), Some(&t.acme), &[]).await;
        assert_eq!(s, want, "{b}");
    }
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "roll" })), Some(&t.mallory), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // roll reviews it: the request is done.
    let (r, cv) = t.a.code.readable_change(&Caller::Admin, id).unwrap();
    t.a.code.submit_review(&r, &cv, &Reviewer::roll(), &json!({ "verdict": "comment", "body": "looks fine" }), &Origin::ROLL).await.unwrap();
    assert!(kinds(&get_change(&t, id).await).is_empty());
    let v = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/review-requests"), None, Some(ROOT), &[]).await;
    assert!(v[0]["done_at"].is_number());
    // A request for a new head starts pending again; finishing it by hand clears it.
    let cv = t.a.code.change_or_404(id).unwrap();
    t.a.code.request_review(&r, &cv, &Reviewer::roll(), true).unwrap();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": "main", "message": "r2", "files": { "src/util.rs": { "content": "r2\n" } } })), Some(ROOT), &[]).await;
    assert!(kinds(&get_change(&t, id).await).is_empty(), "a request is for the head it was made for");
    let cv = t.a.code.change_or_404(id).unwrap();
    t.a.code.request_review(&r, &cv, &Reviewer::roll(), true).unwrap();
    assert_eq!(kinds(&get_change(&t, id).await), vec!["review_pending"]);
    t.a.code.finish_review_request(&cv, &Reviewer::roll()).unwrap();
    assert!(kinds(&get_change(&t, id).await).is_empty());
}

// ── review apps ──

#[derive(Default)]
struct Receiver {
    got: parking_lot::Mutex<Vec<(axum::http::HeaderMap, Vec<u8>)>>,
    /// Fail this many deliveries first.
    fail: std::sync::atomic::AtomicUsize,
}

async fn receiver() -> (String, Arc<Receiver>) {
    let rx = Arc::new(Receiver::default());
    let state = rx.clone();
    let router = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let rx = state.clone();
            async move {
                rx.got.lock().push((headers, body.to_vec()));
                if rx.fail.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    rx.fail.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    return StatusCode::INTERNAL_SERVER_ERROR;
                }
                StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, rx)
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..300 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn events(rx: &Receiver) -> Vec<String> {
    rx.got.lock().iter().map(|(h, _)| h["x-roll-event"].to_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn review_apps_lifecycle_and_deliveries() {
    let t = setup_with(|c| c.allow_private_webhooks = Some(true)).await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "secret", "visibility": "private" })), Some(ROOT), &[]).await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "other", "visibility": "private" })), Some(ROOT), &[]).await;
    let (url, rx) = receiver().await;

    // Registration and validation.
    for (b, want) in [
        (json!({ "name": "bot", "webhook_url": "ftp://x" }), StatusCode::BAD_REQUEST),
        (json!({ "name": "bot", "webhook_url": "https://u:p@x.example" }), StatusCode::BAD_REQUEST),
        (json!({ "name": "bad name", "webhook_url": url }), StatusCode::BAD_REQUEST),
        (json!({ "name": "bot", "webhook_url": url, "events": ["push"] }), StatusCode::BAD_REQUEST),
        (json!({ "name": "bot", "webhook_url": url, "repos": ["acme/nope"] }), StatusCode::NOT_FOUND),
        (json!({ "name": "bot", "webhook_url": url, "repos": ["mallory/x"] }), StatusCode::BAD_REQUEST),
        (json!({ "name": "bot" }), StatusCode::BAD_REQUEST),
    ] {
        let (s, v) = call(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(b.clone()), Some(&t.acme), &[]).await;
        assert_eq!(s, want, "{b}: {v}");
    }
    let (s, _) = call(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "bot", "webhook_url": url })), Some(&t.mallory), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only the owner registers apps");
    let created = ok(
        &t.a,
        "POST",
        "/v1/code/owners/acme/review-apps",
        Some(json!({ "name": "greptile", "webhook_url": url, "events": ["change.opened", "change.revised", "thread.comment"], "repos": ["acme/api", "secret"], "required": true })),
        Some(&t.acme),
        &[],
    )
    .await;
    let app_id = created["app"]["id"].as_str().unwrap().to_string();
    let token = created["token"].as_str().unwrap().to_string();
    let secret = created["secret"].as_str().unwrap().to_string();
    assert!(token.starts_with("rla_") && secret.starts_with("rls_"));
    assert_eq!(created["app"]["repos"], json!(["acme/api", "acme/secret"]));
    let (s, _) = call(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "Greptile", "webhook_url": url })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "names are unique per owner, case-insensitively");
    let list = ok(&t.a, "GET", "/v1/code/owners/acme/review-apps", None, Some(&t.acme), &[]).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0].get("secret").is_none() && list[0].get("token").is_none(), "secrets are shown once");
    let (s, _) = call(&t.a, "GET", &format!("/v1/code/review-apps/{app_id}"), None, Some(&t.mallory), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // The app's token: reads its repos (private too), not others; reviews; never writes.
    let (s, info) = call(&t.a, "GET", "/v1/code", None, Some(&token), &[]).await;
    assert_eq!((s, info["auth"]["caller"]["kind"].as_str()), (StatusCode::OK, Some("app")));
    ok(&t.a, "GET", "/v1/code/repos/acme/secret", None, Some(&token), &[]).await;
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/other", None, Some(&token), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "x", "files": { "x": { "content": "x" } } })), Some(&token), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "required_approvals": 0 })), Some(&token), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // A new change: change.opened is delivered, signed; the required app owes a review.
    let c = change(&t, "for review", "alice", json!({ "src/util.rs": { "content": "pub fn one() -> u32 {\n    7\n}\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    until("change.opened + review.requested", || {
        let e = events(&rx);
        e.contains(&"change.opened".to_string()) && e.contains(&"review.requested".to_string())
    })
    .await;
    {
        let got = rx.got.lock();
        let (h, body) = got.iter().find(|(h, _)| h["x-roll-event"] == "change.opened").unwrap();
        let ts: i64 = h["x-roll-timestamp"].to_str().unwrap().parse().unwrap();
        assert_eq!(h["x-roll-signature-256"].to_str().unwrap(), code_control::review::apps::signature(&secret, ts, body));
        let p: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(p["event"], "change.opened");
        assert_eq!(p["change"]["id"], id.as_str());
        assert_eq!(p["repo"]["full_name"], "acme/api");
        assert_eq!(p["app"]["name"], "greptile");
        assert!(p["urls"]["diff"].as_str().unwrap().ends_with("?format=diff"));
        assert!(p["urls"]["reviews"].as_str().unwrap().ends_with(&format!("/changes/{id}/reviews")));
        assert_eq!(h["x-roll-delivery"].to_str().unwrap(), p["delivery"].as_str().unwrap());
    }
    let cv = get_change(&t, &id).await;
    assert_eq!(kinds(&cv), vec!["review_pending"]);
    // The diff URL serves text.
    let diff_path = format!("/v1/code/repos/acme/api/compare/{}...{}?format=diff", cv["base"].as_str().unwrap(), cv["head"].as_str().unwrap());
    let (s, diff) = t.a.raw("GET", &diff_path, None, Some(&token)).await;
    assert_eq!(s, StatusCode::OK);
    let diff = String::from_utf8(diff).unwrap();
    assert!(diff.starts_with("diff --git a/src/util.rs b/src/util.rs\n") && diff.contains("+    7"), "{diff}");

    // The app reviews (an inline comment too): the request is done; its own comment isn't sent
    // back to it.
    let before = rx.got.lock().len();
    let v = ok(
        &t.a,
        "POST",
        &format!("/v1/code/changes/{id}/reviews"),
        Some(json!({ "verdict": "approve", "body": "LGTM", "comments": [{ "path": "src/util.rs", "line": 2, "body": "nit: name it SEVEN" }] })),
        Some(&token),
        &[],
    )
    .await;
    assert_eq!(v["reviewer"], json!({ "kind": "app", "name": "greptile", "app": app_id }));
    assert_eq!(v["threads"].as_array().unwrap().len(), 1);
    assert!(kinds(&get_change(&t, &id).await).is_empty());
    // A person replies: thread.comment goes to the app.
    let th = v["threads"][0]["id"].as_str().unwrap().to_string();
    ok(&t.a, "POST", &format!("/v1/code/threads/{th}/comments"), Some(json!({ "body": "fixed?" })), Some(&t.acme), &[]).await;
    until("thread.comment", || events(&rx)[before..].contains(&"thread.comment".to_string())).await;
    assert_eq!(events(&rx)[before..].iter().filter(|e| *e == "thread.comment").count(), 1, "not for the app's own comment");
    // It may resolve the thread it started.
    ok(&t.a, "POST", &format!("/v1/code/threads/{th}/resolve"), Some(json!({})), Some(&token), &[]).await;

    // A failing endpoint is retried with backoff until it answers (with the app no longer
    // required, only change.revised goes out).
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{app_id}"), Some(json!({ "required": false })), Some(&t.acme), &[]).await;
    rx.fail.store(2, std::sync::atomic::Ordering::SeqCst);
    let before = rx.got.lock().len();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": "main", "message": "v2", "files": { "src/util.rs": { "content": "v2\n" } } })), Some(ROOT), &[]).await;
    until("retries", || rx.got.lock()[before..].iter().filter(|(h, _)| h["x-roll-event"] == "change.revised").count() >= 3).await;
    let log = ok(&t.a, "GET", &format!("/v1/code/review-apps/{app_id}/deliveries"), None, Some(&t.acme), &[]).await;
    let mut revised = None;
    for _ in 0..100 {
        let log = ok(&t.a, "GET", &format!("/v1/code/review-apps/{app_id}/deliveries"), None, Some(&t.acme), &[]).await;
        revised = log.as_array().unwrap().iter().find(|d| d["event"] == "change.revised" && d["state"] == "delivered").cloned();
        if revised.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let revised = revised.unwrap_or_else(|| panic!("never delivered: {log}"));
    assert_eq!(revised["attempts"], 3);
    assert_eq!(revised["last_status"], 204);
    // Redelivery.
    let n = rx.got.lock().len();
    let d = ok(&t.a, "POST", &format!("/v1/code/review-apps/{app_id}/deliveries/{}/redeliver", revised["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(d["id"], revised["id"]);
    until("redelivery", || rx.got.lock().len() > n).await;
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/review-apps/{app_id}/deliveries/dl_nope/redeliver"), Some(json!({})), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Update, disable, rotate, delete.
    let v = ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{app_id}"), Some(json!({ "required": true, "events": ["thread.comment"] })), Some(&t.acme), &[]).await;
    assert_eq!((v["required"].as_bool(), v["events"].clone()), (Some(true), json!(["thread.comment"])));
    let (s, _) = call(&t.a, "PATCH", &format!("/v1/code/review-apps/{app_id}"), Some(json!({ "webhook_url": "nope" })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{app_id}"), Some(json!({ "active": false })), Some(&t.acme), &[]).await;
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/api", None, Some(&token), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a disabled app's token stops working");
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{app_id}"), Some(json!({ "active": true })), Some(&t.acme), &[]).await;
    let rotated = ok(&t.a, "POST", &format!("/v1/code/review-apps/{app_id}/rotate"), Some(json!({})), Some(&t.acme), &[]).await;
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/api", None, Some(&token), &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "the old token is gone");
    ok(&t.a, "GET", "/v1/code/repos/acme/api", None, Some(rotated["token"].as_str().unwrap()), &[]).await;
    let (s, _) = call(&t.a, "DELETE", &format!("/v1/code/review-apps/{app_id}"), None, Some(&t.mallory), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&t.a, "DELETE", &format!("/v1/code/review-apps/{app_id}"), None, Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/api", None, Some(rotated["token"].as_str().unwrap()), &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn app_tokens_are_scoped_even_on_an_open_daemon() {
    let a = common::app(None);
    a.repo("acme", "api").await;
    a.ok("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "hidden", "visibility": "private" }))).await;
    let created = a.ok("POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "bot", "webhook_url": "http://127.0.0.1:9/x", "repos": ["api"] }))).await;
    let token = created["token"].as_str().unwrap();
    let (s, _) = a.req("GET", "/v1/code/repos/acme/hidden", None, Some(token)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "an app token isn't the admin, even when the daemon is open");
    let (s, _) = a.req("DELETE", "/v1/code/repos/acme/api", None, Some(token)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/api", None, Some("rla_bogus")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn private_webhook_addresses_are_refused() {
    let t = setup_with(|c| c.allow_private_webhooks = Some(false)).await;
    let (url, rx) = receiver().await;
    let created = ok(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "local", "webhook_url": url })), Some(&t.acme), &[]).await;
    let app_id = created["app"]["id"].as_str().unwrap().to_string();
    let localhost = ok(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "byname", "webhook_url": url.replace("127.0.0.1", "localhost") })), Some(&t.acme), &[]).await;
    change(&t, "c", "alice", json!({ "src/util.rs": { "content": "c\n" } })).await;
    for id in [app_id, localhost["app"]["id"].as_str().unwrap().to_string()] {
        let mut err = None;
        for _ in 0..200 {
            let log = ok(&t.a, "GET", &format!("/v1/code/review-apps/{id}/deliveries"), None, Some(&t.acme), &[]).await;
            if let Some(e) = log.as_array().unwrap().first().and_then(|d| d["last_error"].as_str()) {
                err = Some(e.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(err.as_deref().is_some_and(|e| e.contains("private")), "{err:?}");
    }
    assert!(rx.got.lock().is_empty(), "nothing reached the private address");
}

#[tokio::test]
async fn review_events_and_diff_format() {
    let t = setup().await;
    let mut rx = t.a.code.subscribe();
    let c = change(&t, "ev", "alice", json!({ "notes.txt": { "content": "1\n" } })).await;
    let id = c["id"].as_str().unwrap();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/reviews"), Some(json!({ "verdict": "comment", "body": "hm", "comments": [{ "path": "notes.txt", "line": 1, "body": "one?" }] })), Some(ROOT), &[]).await;
    let mut kinds_seen = Vec::new();
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        if e.kind == "review" {
            assert_eq!(e.data["verdict"], "comment");
            assert_eq!(e.data["change"], id);
        }
        if e.kind == "thread" {
            assert_eq!(e.data["path"], "notes.txt");
            assert_eq!(e.data["comments"][0]["body"], "one?");
        }
        kinds_seen.push(e.kind);
    }
    assert!(kinds_seen.contains(&"review") && kinds_seen.contains(&"thread") && kinds_seen.contains(&"change"), "{kinds_seen:?}");
    let (s, _) = t.a.raw("GET", &format!("/v1/code/repos/acme/api/compare/main...{}?format=yaml", c["head"].as_str().unwrap()), None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, body) = t.a.raw("GET", &format!("/v1/code/repos/acme/api/compare/main...{}?format=json", c["head"].as_str().unwrap()), None, Some(ROOT)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(serde_json::from_slice::<Value>(&body).unwrap()["files"].is_array());
}

#[tokio::test]
async fn auth_matrix() {
    let t = setup().await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "priv", "visibility": "private" })), Some(ROOT), &[]).await;
    ok(&t.a, "POST", "/v1/code/repos/acme/priv/land", Some(json!({ "message": "base", "files": { "a.txt": { "content": "a\n" } } })), Some(ROOT), &[]).await;
    let public = change(&t, "pub", "alice", json!({ "notes.txt": { "content": "x\n" } })).await;
    let private = ok(&t.a, "POST", "/v1/code/repos/acme/priv/changes", Some(json!({ "title": "p", "message": "p", "base": "main", "files": { "a.txt": { "content": "b\n" } } })), Some(ROOT), &[]).await;
    let pt = ok(&t.a, "POST", &format!("/v1/code/changes/{}/threads", public["id"].as_str().unwrap()), Some(json!({ "path": "notes.txt", "line": 1, "body": "t" })), Some(ROOT), &[]).await;
    let qt = ok(&t.a, "POST", &format!("/v1/code/changes/{}/threads", private["id"].as_str().unwrap()), Some(json!({ "path": "a.txt", "line": 1, "body": "t" })), Some(ROOT), &[]).await;
    let (pc, qc) = (public["id"].as_str().unwrap(), private["id"].as_str().unwrap());
    let (ptid, qtid) = (pt["id"].as_str().unwrap(), qt["id"].as_str().unwrap());
    let mallory = t.mallory.as_str();
    let acme = t.acme.as_str();

    // (method, path, body, token, expected)
    let comment = json!({ "verdict": "comment", "body": "hi" });
    let approve = json!({ "verdict": "approve" });
    type Case<'a> = (&'a str, String, Option<Value>, Option<&'a str>, StatusCode);
    let cases: Vec<Case> = vec![
        // Reading a public repo's reviews: anyone; a private one's: only its owner.
        ("GET", format!("/v1/code/changes/{pc}/reviews"), None, None, StatusCode::OK),
        ("GET", format!("/v1/code/changes/{pc}/threads"), None, None, StatusCode::OK),
        ("GET", format!("/v1/code/changes/{pc}/review-status"), None, None, StatusCode::OK),
        ("GET", format!("/v1/code/changes/{pc}/review-requests"), None, None, StatusCode::OK),
        ("GET", format!("/v1/code/threads/{ptid}"), None, None, StatusCode::OK),
        ("GET", "/v1/code/repos/acme/api/review-settings".into(), None, None, StatusCode::OK),
        ("GET", format!("/v1/code/changes/{qc}/reviews"), None, None, StatusCode::NOT_FOUND),
        ("GET", format!("/v1/code/changes/{qc}/reviews"), None, Some(mallory), StatusCode::NOT_FOUND),
        ("GET", format!("/v1/code/changes/{qc}/threads"), None, Some(mallory), StatusCode::NOT_FOUND),
        ("GET", format!("/v1/code/changes/{qc}/review-status"), None, Some(mallory), StatusCode::NOT_FOUND),
        ("GET", format!("/v1/code/threads/{qtid}"), None, Some(mallory), StatusCode::NOT_FOUND),
        ("GET", "/v1/code/repos/acme/priv/review-settings".into(), None, Some(mallory), StatusCode::NOT_FOUND),
        ("GET", format!("/v1/code/changes/{qc}/reviews"), None, Some(acme), StatusCode::OK),
        // Commenting needs a sign-in; approving needs write access.
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(comment.clone()), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(comment.clone()), Some(mallory), StatusCode::CREATED),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(approve.clone()), Some(mallory), StatusCode::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(json!({ "verdict": "request_changes", "body": "no" })), Some(mallory), StatusCode::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(approve.clone()), Some(acme), StatusCode::CREATED),
        ("POST", format!("/v1/code/changes/{qc}/reviews"), Some(comment.clone()), Some(mallory), StatusCode::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/threads"), Some(json!({ "path": "notes.txt", "line": 1, "body": "x" })), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/threads"), Some(json!({ "path": "notes.txt", "line": 1, "body": "x" })), Some(mallory), StatusCode::CREATED),
        ("POST", format!("/v1/code/changes/{qc}/threads"), Some(json!({ "path": "a.txt", "line": 1, "body": "x" })), Some(mallory), StatusCode::NOT_FOUND),
        ("POST", format!("/v1/code/threads/{ptid}/comments"), Some(json!({ "body": "x" })), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{ptid}/comments"), Some(json!({ "body": "x" })), Some(mallory), StatusCode::CREATED),
        ("POST", format!("/v1/code/threads/{ptid}/comments"), Some(json!({ "body": "" })), Some(mallory), StatusCode::BAD_REQUEST),
        ("POST", format!("/v1/code/threads/{qtid}/comments"), Some(json!({ "body": "x" })), Some(mallory), StatusCode::NOT_FOUND),
        // Resolving someone else's thread needs write access.
        ("POST", format!("/v1/code/threads/{ptid}/resolve"), Some(json!({})), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{ptid}/resolve"), Some(json!({})), Some(mallory), StatusCode::FORBIDDEN),
        ("POST", format!("/v1/code/threads/{ptid}/unresolve"), Some(json!({})), Some(mallory), StatusCode::FORBIDDEN),
        ("POST", format!("/v1/code/threads/{ptid}/resolve"), Some(json!({})), Some(acme), StatusCode::OK),
        ("POST", format!("/v1/code/threads/{ptid}/apply"), Some(json!({})), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{ptid}/apply"), Some(json!({})), Some(mallory), StatusCode::FORBIDDEN),
        // Settings and requests: writers.
        ("PATCH", "/v1/code/repos/acme/api/review-settings".into(), Some(json!({ "required_approvals": 1 })), None, StatusCode::UNAUTHORIZED),
        ("PATCH", "/v1/code/repos/acme/api/review-settings".into(), Some(json!({ "required_approvals": 1 })), Some(mallory), StatusCode::FORBIDDEN),
        ("PATCH", "/v1/code/repos/acme/api/review-settings".into(), Some(json!({ "required_approvals": 0 })), Some(acme), StatusCode::OK),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), None, StatusCode::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), Some(mallory), StatusCode::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), Some(acme), StatusCode::CREATED),
        // Dismissing someone else's review needs write access.
        // Review apps: the owner manages them.
        ("GET", "/v1/code/owners/acme/review-apps".into(), None, None, StatusCode::UNAUTHORIZED),
        ("GET", "/v1/code/owners/acme/review-apps".into(), None, Some(mallory), StatusCode::FORBIDDEN),
        ("GET", "/v1/code/owners/acme/review-apps".into(), None, Some(acme), StatusCode::OK),
        ("GET", "/v1/code/owners/nobody/review-apps".into(), None, Some(ROOT), StatusCode::NOT_FOUND),
        ("GET", "/v1/code/review-apps/app_nope".into(), None, Some(ROOT), StatusCode::NOT_FOUND),
        ("GET", "/v1/code/review-apps/app_nope/deliveries".into(), None, Some(acme), StatusCode::NOT_FOUND),
        ("POST", "/v1/code/review-apps/app_nope/rotate".into(), Some(json!({})), None, StatusCode::NOT_FOUND),
        // Unknown things.
        ("GET", "/v1/code/changes/chg_nope/reviews".into(), None, Some(ROOT), StatusCode::NOT_FOUND),
        ("GET", "/v1/code/threads/th_nope".into(), None, Some(ROOT), StatusCode::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/reviews/rv_nope/dismiss"), Some(json!({})), Some(acme), StatusCode::NOT_FOUND),
    ];
    for (m, p, b, tok, want) in cases {
        let (s, v) = call(&t.a, m, &p, b.clone(), tok, &[]).await;
        assert_eq!(s, want, "{m} {p} as {tok:?}: {v}");
    }
    // Dismissing: a reviewer withdraws their own; others' need write access.
    let mine = ok(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews"), Some(json!({ "verdict": "approve" })), Some(acme), &[("x-roll-agent", "a1")]).await;
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews/{}/dismiss", mine["id"].as_str().unwrap()), Some(json!({})), Some(mallory), &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews/{}/dismiss", mine["id"].as_str().unwrap()), Some(json!({})), None, &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let comment_review = ok(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews"), Some(json!({ "verdict": "comment", "body": "note" })), Some(mallory), &[]).await;
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews/{}/dismiss", comment_review["id"].as_str().unwrap()), Some(json!({})), Some(mallory), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "comments aren't dismissed");
    // The reviewer themself (same agent) can withdraw.
    let v = ok(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews/{}/dismiss", mine["id"].as_str().unwrap()), Some(json!({ "reason": "wrong change" })), Some(acme), &[("x-roll-agent", "a1")]).await;
    assert_eq!(v["dismissed"], true);
    // A thread starter (not a writer) can resolve their own thread.
    let own = ok(&t.a, "POST", &format!("/v1/code/changes/{pc}/threads"), Some(json!({ "path": "notes.txt", "line": 1, "body": "mine" })), Some(mallory), &[]).await;
    ok(&t.a, "POST", &format!("/v1/code/threads/{}/resolve", own["id"].as_str().unwrap()), Some(json!({})), Some(mallory), &[]).await;
}

#[tokio::test]
async fn contract_details_caller_comment_index_renames_and_apps() {
    let t = setup().await;
    // GET /v1/code: who the caller reviews as.
    let me = ok(&t.a, "GET", "/v1/code", None, Some(&t.fixer), &[]).await;
    assert_eq!(me["auth"]["caller"]["reviewer"], json!({ "kind": "agent", "name": "fixer" }));
    let me = ok(&t.a, "GET", "/v1/code", None, Some(&t.acme), &[("x-roll-agent", "bot")]).await;
    assert_eq!(me["auth"]["caller"]["reviewer"], json!({ "kind": "agent", "name": "bot" }));
    let me = ok(&t.a, "GET", "/v1/code", None, Some(ROOT), &[("x-roll-user", "alice")]).await;
    assert_eq!(me["auth"]["caller"]["reviewer"], json!({ "kind": "user", "name": "alice" }));
    let anon = ok(&t.a, "GET", "/v1/code", None, None, &[]).await;
    assert!(anon["auth"]["caller"]["reviewer"].is_null());

    // A rename is a delete plus an add: the old side keeps the old path, the new side the new one.
    let c = change(&t, "Rename", "alice", json!({ "src/util.rs": { "delete": true }, "src/helpers.rs": { "content": "pub fn one() -> u32 {\n    1\n}\n" } })).await;
    let id = c["id"].as_str().unwrap();
    let old = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "src/util.rs", "side": "old", "line": 1, "body": "why move it?" })), Some(ROOT), &[]).await;
    assert_eq!((old["path"].as_str(), old["side"].as_str()), (Some("src/util.rs"), Some("old")));
    let new = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "src/helpers.rs", "line": 2, "body": "ok" })), Some(ROOT), &[]).await;
    assert_eq!((new["path"].as_str(), new["side"].as_str()), (Some("src/helpers.rs"), Some("new")));
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "src/helpers.rs", "side": "old", "line": 1, "body": "x" })), Some(ROOT), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the new path isn't on the old side");
    let (s, _) = call(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "src/util.rs", "line": 1, "body": "x" })), Some(ROOT), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "the old path isn't on the new side");

    // A review whose second inline comment can't be placed: 400 naming it, nothing written.
    let before = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/reviews"), None, Some(ROOT), &[]).await.as_array().unwrap().len();
    let (s, v) = call(
        &t.a,
        "POST",
        &format!("/v1/code/changes/{id}/reviews"),
        Some(json!({ "verdict": "comment", "body": "two notes", "comments": [{ "path": "src/helpers.rs", "line": 1, "body": "fine" }, { "path": "src/helpers.rs", "line": 99, "body": "nope" }] })),
        Some(ROOT),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["comment"], 1);
    assert!(v["error"].as_str().unwrap().starts_with("comment 1:"), "{v}");
    assert_eq!(ok(&t.a, "GET", &format!("/v1/code/changes/{id}/reviews"), None, Some(ROOT), &[]).await.as_array().unwrap().len(), before);

    // Review requests carry the Reviewer object.
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "agent:fixer" })), Some(ROOT), &[]).await;
    let reqs = ok(&t.a, "GET", &format!("/v1/code/changes/{id}/review-requests"), None, Some(ROOT), &[]).await;
    assert_eq!(reqs[0]["reviewer"], json!({ "kind": "agent", "name": "fixer", "app": null }));

    // review-settings: file and codeowners shapes when absent.
    let rs = ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(ROOT), &[]).await;
    assert!(rs["file"].is_null());
    assert!(rs["codeowners"]["path"].is_null());
    assert_eq!(rs["codeowners"]["rules"], json!([]));

    // Review apps: no `repos` means every repo of the owner.
    let app = ok(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "bot", "webhook_url": "https://example.com/hook" })), Some(ROOT), &[]).await;
    assert_eq!(app["app"]["all_repos"], true);
    assert_eq!(app["app"]["repos"], json!([]));
}
