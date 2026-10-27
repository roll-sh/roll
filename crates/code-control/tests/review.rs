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
                    return (StatusCode::INTERNAL_SERVER_ERROR, "boom");
                }
                (StatusCode::NO_CONTENT, "")
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

// ── every rule, on and off, from the repo's settings and from `.roll/review.yaml` ──

/// Every rule off (and no approvals required).
fn all_off() -> Value {
    json!({
        "required_approvals": 0, "require_codeowners": false, "count_ai_approval": false, "block_on_request_changes": false,
        "dismiss_stale": false, "require_resolved_threads": false, "allow_self_approval": false, "auto_approve": [],
    })
}

/// Set the repo's settings to `extra` (every other rule off) and `.roll/review.yaml` to `file`
/// (`None`: a file that sets nothing). Returns `GET review-settings`.
async fn rules_now(t: &T, n: &mut u32, extra: Value, file: Option<&str>) -> Value {
    *n += 1;
    let text = format!("# {n}\n{}", file.unwrap_or(""));
    land_admin(t, &format!("rules {n}"), json!({ ".roll/review.yaml": { "content": text } })).await;
    let mut s = all_off();
    for (k, v) in extra.as_object().unwrap() {
        s[k] = v.clone();
    }
    settings(t, s).await;
    ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(ROOT), &[]).await
}

/// A new change by `author` writing `path`.
async fn fresh(t: &T, n: &mut u32, author: &str, path: &str) -> String {
    *n += 1;
    let mut files = serde_json::Map::new();
    files.insert(path.into(), json!({ "content": format!("v{n}\n") }));
    change(t, &format!("c{n}"), author, Value::Object(files)).await["id"].as_str().unwrap().to_string()
}

fn has(c: &Value, kind: &str) -> bool {
    kinds(c).iter().any(|k| k == kind)
}

async fn status_of(t: &T, id: &str) -> Value {
    ok(&t.a, "GET", &format!("/v1/code/changes/{id}/review-status"), None, Some(ROOT), &[]).await
}

/// What a boolean rule does, given whether it's in effect (each blocker it adds appears and clears).
async fn check_rule(t: &T, n: &mut u32, key: &str, on: bool) {
    match key {
        "require_codeowners" => {
            // CODEOWNERS: `* @acme/core`; the org's token stands for its teams.
            let id = fresh(t, n, "alice", "src/util.rs").await;
            assert_eq!(has(&get_change(t, &id).await, "needs_codeowner"), on, "{key} {on}");
            assert_eq!(status_of(t, &id).await["codeowners"][0]["owners"], json!(["@acme/core"]));
            review(t, &id, "approve", &t.fixer, &[]).await;
            assert_eq!(has(&get_change(t, &id).await, "needs_codeowner"), on, "an agent isn't @acme/core");
            review(t, &id, "approve", &t.acme, &[]).await;
            assert!(!has(&get_change(t, &id).await, "needs_codeowner"));
        }
        "block_on_request_changes" => {
            let id = fresh(t, n, "alice", "notes.txt").await;
            review(t, &id, "request_changes", &t.acme, &[]).await;
            let c = get_change(t, &id).await;
            assert_eq!(has(&c, "changes_requested"), on, "{key} {on}: {c}");
            assert_eq!(c["review"]["state"] == "changes_requested", on);
            assert_eq!(status_of(t, &id).await["changes_requested"][0]["name"], "acme", "listed either way");
            // The same reviewer approving lifts it.
            review(t, &id, "approve", &t.acme, &[]).await;
            assert!(!has(&get_change(t, &id).await, "changes_requested"));
        }
        "dismiss_stale" => {
            // required_approvals: 1
            let id = fresh(t, n, "alice", "notes.txt").await;
            review(t, &id, "approve", &t.fixer, &[]).await;
            assert!(!has(&get_change(t, &id).await, "needs_approval"));
            let files = json!({ "notes.txt": { "content": format!("v{n} again\n") } });
            ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": "main", "message": "v2", "files": files })), Some(ROOT), &[]).await;
            assert_eq!(has(&get_change(t, &id).await, "needs_approval"), on, "{key} {on}");
            let st = status_of(t, &id).await;
            assert_eq!((st["approvals"][0]["stale"].as_bool(), st["approvals"][0]["counted"].as_bool()), (Some(true), Some(!on)), "{st}");
            review(t, &id, "approve", &t.fixer, &[]).await;
            assert!(!has(&get_change(t, &id).await, "needs_approval"), "a fresh approval counts");
        }
        "require_resolved_threads" => {
            let id = fresh(t, n, "alice", "notes.txt").await;
            let th = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "notes.txt", "line": 1, "body": "hm" })), Some(ROOT), &[]).await;
            let c = get_change(t, &id).await;
            assert_eq!(has(&c, "unresolved_threads"), on, "{key} {on}: {c}");
            assert_eq!(status_of(t, &id).await["unresolved_threads"], 1, "counted either way");
            ok(&t.a, "POST", &format!("/v1/code/threads/{}/resolve", th["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
            assert!(!has(&get_change(t, &id).await, "unresolved_threads"));
        }
        "count_ai_approval" => {
            // required_approvals: 1
            let id = fresh(t, n, "alice", "notes.txt").await;
            let (r, cv) = t.a.code.readable_change(&Caller::Admin, &id).unwrap();
            t.a.code.submit_review(&r, &cv, &Reviewer::roll(), &json!({ "verdict": "approve", "body": "LGTM" }), &Origin::ROLL).await.unwrap();
            assert_eq!(has(&get_change(t, &id).await, "needs_approval"), !on, "{key} {on}");
            assert_eq!(status_of(t, &id).await["approvals"][0]["counted"], on);
        }
        "allow_self_approval" => {
            // required_approvals: 1
            let id = fresh(t, n, "alice", "notes.txt").await;
            review(t, &id, "approve", ROOT, &[("x-roll-user", "alice")]).await;
            assert_eq!(has(&get_change(t, &id).await, "needs_approval"), !on, "{key} {on}");
            let st = status_of(t, &id).await;
            assert_eq!(st["approvals"][0]["reason"].is_null(), on, "{st}");
        }
        _ => unreachable!("{key}"),
    }
}

#[tokio::test]
async fn every_rule_on_and_off_from_settings_and_the_file() {
    let t = setup().await;
    let mut n = 0u32;
    land_admin(&t, "owners", json!({ "CODEOWNERS": { "content": "* @acme/core\n" } })).await;

    // Defaults: nothing required, block_on_request_changes and dismiss_stale on.
    let s = ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, None, &[]).await;
    assert_eq!(
        s["effective"],
        json!({ "required_approvals": 0, "require_codeowners": false, "count_ai_approval": false, "block_on_request_changes": true,
                "dismiss_stale": true, "require_resolved_threads": false, "allow_self_approval": false, "auto_approve": [] })
    );

    // Requirements add up (the file turns them on, never off); count_ai_approval and
    // allow_self_approval hold only when both allow them.
    let or_keys = ["require_codeowners", "block_on_request_changes", "dismiss_stale", "require_resolved_threads"];
    let and_keys = ["count_ai_approval", "allow_self_approval"];
    for key in or_keys.iter().chain(&and_keys) {
        let both = and_keys.contains(key);
        for (s, f, want) in [(true, None, true), (false, None, false), (false, Some(true), !both), (true, Some(false), !both)] {
            let mut extra = if matches!(*key, "dismiss_stale" | "count_ai_approval" | "allow_self_approval") { json!({ "required_approvals": 1 }) } else { json!({}) };
            extra[*key] = json!(s);
            let file = f.map(|f| format!("{key}: {f}\n"));
            let v = rules_now(&t, &mut n, extra, file.as_deref()).await;
            let what = format!("{key}: settings {s}, file {f:?}");
            assert_eq!(v["settings"][*key], s, "{what}");
            assert_eq!(v["file"]["rules"].get(*key).cloned(), f.map(Value::Bool), "{what}: keys the file doesn't set are left out");
            assert!(v["file"]["errors"].as_array().unwrap().is_empty(), "{what}: {v}");
            assert_eq!(v["effective"][*key], want, "{what}");
            let id = fresh(&t, &mut n, "alice", "notes.txt").await;
            assert_eq!(status_of(&t, &id).await["rules"][*key], want, "{what}: review-status shows the effective rules");
            check_rule(&t, &mut n, key, want).await;
        }
    }

    // required_approvals: the larger count wins.
    for (s, f, want) in [(0u32, None, 0u32), (1, None, 1), (0, Some(2u32), 2), (2, Some(1), 2), (3, Some(0), 3)] {
        let file = f.map(|f| format!("required_approvals: {f}\n"));
        let v = rules_now(&t, &mut n, json!({ "required_approvals": s }), file.as_deref()).await;
        assert_eq!(v["effective"]["required_approvals"], want, "settings {s}, file {f:?}");
        let id = fresh(&t, &mut n, "alice", "notes.txt").await;
        let c = get_change(&t, &id).await;
        assert_eq!(c["review"]["required"], want);
        if want == 0 {
            assert!(kinds(&c).is_empty(), "{c}");
            continue;
        }
        for i in 0..want {
            let c = get_change(&t, &id).await;
            assert_eq!(kinds(&c), vec!["needs_approval"], "{i} of {want}");
            let plural = if want == 1 { "" } else { "s" };
            assert_eq!(c["blockers"][0]["message"], format!("needs {want} approval{plural} ({i} so far)"));
            review(&t, &id, "approve", &t.acme, &[("x-roll-agent", &format!("r{i}"))]).await;
        }
        let c = get_change(&t, &id).await;
        assert!(kinds(&c).is_empty(), "{c}");
        assert_eq!((c["review"]["state"].as_str(), c["review"]["approvals"].as_u64()), (Some("approved"), Some(want as u64)));
    }
    // Bounds: 0-20, in settings and in the file.
    rules_now(&t, &mut n, json!({}), None).await;
    let v = settings(&t, json!({ "required_approvals": 20 })).await;
    assert_eq!(v["effective"]["required_approvals"], 20);
    for bad in [json!(21), json!(-1), json!("1"), json!(1.5)] {
        let (s, e) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "required_approvals": bad })), Some(ROOT), &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {e}");
    }
    let v = settings(&t, json!({ "required_approvals": null })).await;
    assert_eq!(v["settings"]["required_approvals"], 20, "null leaves a rule as it is");
    let v = rules_now(&t, &mut n, json!({ "required_approvals": 1 }), Some("required_approvals: 20\n")).await;
    assert_eq!(v["effective"]["required_approvals"], 20);
    let v = rules_now(&t, &mut n, json!({ "required_approvals": 1 }), Some("required_approvals: 21\nrequire_codeowners: true\n")).await;
    assert!(v["file"]["rules"].is_null() && v["file"]["errors"][0].as_str().unwrap().contains("0-20"), "{v}");
    assert_eq!(
        (v["effective"]["required_approvals"].as_u64(), v["effective"]["require_codeowners"].as_bool()),
        (Some(1), Some(false)),
        "an invalid file falls back to the settings"
    );
    // A review file that isn't a regular file is reported and ignored.
    land_admin(&t, "link", json!({ ".roll/review.yaml": { "content": "CODEOWNERS", "mode": "120000" } })).await;
    let v = ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(ROOT), &[]).await;
    assert!(v["file"]["errors"][0].as_str().unwrap().contains("regular file"), "{v}");
    assert_eq!(v["effective"]["required_approvals"], 1);

    // auto_approve: rules from both apply.
    let comment_only = |n: u32| json!({ "src/lib.rs": { "content": format!("// note {n}\nfn a() {{}}\n\nfn b() {{\n    1\n}}\n") } });
    for (s, f, docs, comments) in [
        (json!([]), None, false, false),
        (json!([{ "paths": ["docs/**"] }]), None, true, false),
        (json!([]), Some("auto_approve:\n  - comment_only: true\n"), false, true),
        (json!([{ "paths": ["docs/**"] }]), Some("auto_approve:\n  - comment_only: true\n"), true, true),
    ] {
        let v = rules_now(&t, &mut n, json!({ "required_approvals": 1, "auto_approve": s }), f).await;
        assert_eq!(v["effective"]["auto_approve"].as_array().unwrap().len(), docs as usize + comments as usize, "{v}");
        let d = fresh(&t, &mut n, "alice", "docs/guide.md").await;
        let c = get_change(&t, &d).await;
        assert_eq!(c["review"]["state"] == "auto_approved", docs, "{c}");
        assert_eq!(has(&c, "needs_approval"), !docs);
        assert_eq!(status_of(&t, &d).await["auto_approved"], if docs { json!({ "rule": "paths: docs/**" }) } else { Value::Null });
        n += 1;
        let cm = change(&t, "comments", "alice", comment_only(n)).await["id"].as_str().unwrap().to_string();
        let c = get_change(&t, &cm).await;
        assert_eq!(c["review"]["state"] == "auto_approved", comments, "{c}");
        assert_eq!(has(&c, "needs_approval"), !comments);
        if docs {
            // An auto-approved change still waits for a required review it was asked for.
            ok(&t.a, "POST", &format!("/v1/code/changes/{d}/review-requests"), Some(json!({ "reviewer": "roll", "required": true })), Some(&t.acme), &[]).await;
            assert_eq!(kinds(&get_change(&t, &d).await), vec!["review_pending"]);
        }
    }
    // One rule with both: comment-only edits under src/.
    rules_now(&t, &mut n, json!({ "required_approvals": 1, "auto_approve": [{ "paths": ["src/**"], "comment_only": true }] }), None).await;
    n += 1;
    let cm = change(&t, "comments", "alice", comment_only(n)).await["id"].as_str().unwrap().to_string();
    assert_eq!(status_of(&t, &cm).await["auto_approved"], json!({ "rule": "paths: src/** (comment_only)" }));
    let code = fresh(&t, &mut n, "alice", "src/lib.rs").await;
    assert!(status_of(&t, &code).await["auto_approved"].is_null());
    assert_eq!(kinds(&get_change(&t, &code).await), vec!["needs_approval"]);
    // auto_approve validation, in settings and in the file.
    let many: Vec<Value> = (0..51).map(|i| json!({ "paths": [format!("d{i}/**")] })).collect();
    let paths: Vec<String> = (0..101).map(|i| format!("d{i}/**")).collect();
    for bad in [json!(many), json!([{ "paths": paths }]), json!([{ "paths": [""] }]), json!([{ "paths": "docs/**" }]), json!([{ "comment_only": false }])] {
        let (s, e) = call(&t.a, "PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "auto_approve": bad })), Some(ROOT), &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    }
    let v = rules_now(&t, &mut n, json!({}), Some("auto_approve:\n  - comment_only: false\n")).await;
    assert!(v["file"]["rules"].is_null() && v["file"]["errors"][0].as_str().unwrap().contains("auto_approve"), "{v}");
}

// ── CODEOWNERS ──

async fn codeowners_of(t: &T) -> Value {
    ok(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(ROOT), &[]).await["codeowners"].clone()
}

fn owner_kinds(st: &Value) -> Vec<(String, Vec<String>, bool)> {
    st["codeowners"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let owners = g["owners"].as_array().unwrap().iter().map(|o| o.as_str().unwrap().to_string()).collect();
            (g["path_pattern"].as_str().unwrap().to_string(), owners, g["satisfied"].as_bool().unwrap())
        })
        .collect()
}

#[tokio::test]
async fn codeowners_locations_last_match_and_every_owner_kind() {
    let t = setup().await;
    settings(&t, json!({ "require_codeowners": true })).await;

    // Read from the first of CODEOWNERS, .roll/CODEOWNERS, .github/CODEOWNERS, docs/CODEOWNERS.
    for (file, owner) in [("docs/CODEOWNERS", "@docs"), (".github/CODEOWNERS", "@gh"), (".roll/CODEOWNERS", "@rolldir")] {
        let mut files = serde_json::Map::new();
        files.insert(file.into(), json!({ "content": format!("* {owner}\n") }));
        land_admin(&t, file, Value::Object(files)).await;
        let co = codeowners_of(&t).await;
        assert_eq!(co["path"], file, "{co}");
        assert_eq!(co["rules"][0]["owners"], json!([owner]));
    }
    // A symlink called CODEOWNERS isn't read.
    land_admin(&t, "link", json!({ "CODEOWNERS": { "content": ".roll/CODEOWNERS", "mode": "120000" } })).await;
    assert_eq!(codeowners_of(&t).await["path"], ".roll/CODEOWNERS");
    let text = "# owners\n*               @acme/core\n/src/           agent:fixer\n/src/util.rs    app:greptile\n/docs/          roll\n/docs/free/\nnotes.txt       @bob\nnope.txt        nobody!\n";
    land_admin(&t, "root", json!({ "CODEOWNERS": { "content": text } })).await;
    let co = codeowners_of(&t).await;
    assert_eq!(co["path"], "CODEOWNERS");
    assert_eq!(co["rules"].as_array().unwrap().len(), 7);
    assert_eq!(co["rules"][1], json!({ "pattern": "/src/", "owners": ["agent:fixer"], "line": 3 }));
    assert_eq!(co["errors"].as_array().unwrap().len(), 1, "{co}");
    assert!(co["errors"][0].as_str().unwrap().starts_with("line 8: \"nobody!\" isn't an owner"), "{co}");

    let app = ok(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "greptile", "webhook_url": "http://127.0.0.1:9/x" })), Some(&t.acme), &[]).await;
    let app_token = app["token"].as_str().unwrap().to_string();
    let roll_approves = |id: String| {
        let a = &t.a;
        async move {
            let (r, cv) = a.code.readable_change(&Caller::Admin, &id).unwrap();
            a.code.submit_review(&r, &cv, &Reviewer::roll(), &json!({ "verdict": "approve" }), &Origin::ROLL).await.unwrap();
        }
    };
    let s = |p: &str, o: &[&str], sat: bool| (p.to_string(), o.iter().map(|x| x.to_string()).collect::<Vec<_>>(), sat);

    // `@org/team`: the org's token.
    let id = change(&t, "readme", "alice", json!({ "README.md": { "content": "hi\n" } })).await["id"].as_str().unwrap().to_string();
    let st = status_of(&t, &id).await;
    assert_eq!(owner_kinds(&st), vec![s("*", &["@acme/core"], false)]);
    assert_eq!(st["blockers"][0], json!({ "kind": "needs_codeowner", "message": "needs approval from @acme/core (code owners of *)" }));
    review(&t, &id, "approve", &t.fixer, &[]).await;
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner"]);
    review(&t, &id, "approve", &t.acme, &[]).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty());
    assert_eq!(get_change(&t, &id).await["review"]["state"], "approved", "approved by its code owners");

    // The last matching line wins (`/src/` over `*`); `agent:name` is that agent.
    let id = change(&t, "lib", "alice", json!({ "src/lib.rs": { "content": "fn a() { 1; }\n" } })).await["id"].as_str().unwrap().to_string();
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("/src/", &["agent:fixer"], false)]);
    review(&t, &id, "approve", &t.acme, &[]).await;
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner"], "@acme/core doesn't own /src/");
    review(&t, &id, "approve", &t.acme, &[("x-roll-agent", "fixer")]).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty());

    // `app:name`: that review app, whatever count_ai_approval says; roll isn't it.
    let id = change(&t, "util", "alice", json!({ "src/util.rs": { "content": "x\n" } })).await["id"].as_str().unwrap().to_string();
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("/src/util.rs", &["app:greptile"], false)]);
    roll_approves(id.clone()).await;
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner"]);
    let (code, v) = review(&t, &id, "approve", &app_token, &[]).await;
    assert_eq!(code, StatusCode::CREATED, "{v}");
    let st = status_of(&t, &id).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty(), "{st}");
    assert!(st["approvals"].as_array().unwrap().iter().all(|a| a["counted"] == false), "AI approvals still don't count as approvals: {st}");

    // `roll`: roll's reviewer.
    let id = change(&t, "docs", "alice", json!({ "docs/guide.md": { "content": "# Guide 2\n" } })).await["id"].as_str().unwrap().to_string();
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("/docs/", &["roll"], false)]);
    review(&t, &id, "approve", &app_token, &[]).await;
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner"]);
    roll_approves(id.clone()).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty());

    // A line without owners un-owns its paths.
    let id = change(&t, "free", "alice", json!({ "docs/free/new.md": { "content": "free\n" } })).await["id"].as_str().unwrap().to_string();
    let st = status_of(&t, &id).await;
    assert!(owner_kinds(&st).is_empty() && kinds(&get_change(&t, &id).await).is_empty(), "{st}");

    // `@name`: that user, but not when they wrote the change (unless allow_self_approval).
    let id = change(&t, "notes", "bob", json!({ "notes.txt": { "content": "bob\n" } })).await["id"].as_str().unwrap().to_string();
    review(&t, &id, "approve", ROOT, &[("x-roll-user", "bob")]).await;
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("notes.txt", &["@bob"], false)]);
    settings(&t, json!({ "allow_self_approval": true })).await;
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("notes.txt", &["@bob"], true)]);
    settings(&t, json!({ "allow_self_approval": false })).await;

    // Two groups block separately; a new revision makes their approvals stale (dismiss_stale).
    let id = change(&t, "both", "alice", json!({ "README.md": { "content": "both\n" }, "src/lib.rs": { "content": "fn both() {}\n" } })).await["id"].as_str().unwrap().to_string();
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner", "needs_codeowner"]);
    review(&t, &id, "approve", &t.acme, &[]).await;
    assert_eq!(owner_kinds(&status_of(&t, &id).await), vec![s("*", &["@acme/core"], true), s("/src/", &["agent:fixer"], false)]);
    review(&t, &id, "approve", &t.fixer, &[]).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty());
    let head = get_change(&t, &id).await["head"].clone();
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "base": head, "message": "more", "files": { "README.md": { "content": "both 2\n" } } })), Some(ROOT), &[]).await;
    assert_eq!(kinds(&get_change(&t, &id).await), vec!["needs_codeowner", "needs_codeowner"]);
    settings(&t, json!({ "dismiss_stale": false })).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty(), "stale code-owner approvals count without dismiss_stale");
    settings(&t, json!({ "require_codeowners": false, "dismiss_stale": true })).await;
    let st = status_of(&t, &id).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty() && st["codeowners"].as_array().unwrap().len() == 2, "listed, not required: {st}");
}

// ── review apps: events, scopes, requests, failures ──

/// Register a review app for `owner`: its id and token.
async fn new_app(t: &T, owner: &str, token: &str, body: Value) -> (String, String) {
    let v = ok(&t.a, "POST", &format!("/v1/code/owners/{owner}/review-apps"), Some(body), Some(token), &[]).await;
    (v["app"]["id"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
}

/// The payloads of `event` a receiver got.
fn payloads(rx: &Receiver, event: &str) -> Vec<Value> {
    rx.got.lock().iter().filter(|(h, _)| h["x-roll-event"] == event).map(|(_, b)| serde_json::from_slice(b).unwrap()).collect()
}

async fn delivery_where(t: &T, app: &str, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..500 {
        let log = ok(&t.a, "GET", &format!("/v1/code/review-apps/{app}/deliveries"), None, Some(&t.acme), &[]).await;
        if let Some(d) = log.as_array().unwrap().iter().find(|d| f(d)) {
            return d.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn review_apps_events_scopes_requests_and_failed_deliveries() {
    let t = setup_with(|c| {
        c.allow_private_webhooks = Some(true);
        c.review_app_retry_ms = 1;
    })
    .await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "other" })), Some(ROOT), &[]).await;
    ok(&t.a, "POST", "/v1/code/repos/acme/other/land", Some(json!({ "message": "base", "files": { "a.txt": { "content": "a\n" } } })), Some(ROOT), &[]).await;
    let (all_url, all) = receiver().await;
    let (notes_url, notes) = receiver().await;
    let (else_url, elsewhere) = receiver().await;
    let (off_url, off) = receiver().await;
    let (bad_url, bad) = receiver().await;
    bad.fail.store(1000, std::sync::atomic::Ordering::SeqCst);
    let (all_id, all_token) = new_app(&t, "acme", &t.acme, json!({ "name": "all", "webhook_url": all_url })).await;
    new_app(&t, "acme", &t.acme, json!({ "name": "notes", "webhook_url": notes_url, "events": ["thread.comment"] })).await;
    let (_, else_token) = new_app(&t, "acme", &t.acme, json!({ "name": "elsewhere", "webhook_url": else_url, "repos": ["other"] })).await;
    let (off_id, _) = new_app(&t, "acme", &t.acme, json!({ "name": "off", "webhook_url": off_url, "required": true })).await;
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{off_id}"), Some(json!({ "active": false })), Some(&t.acme), &[]).await;
    let (bad_id, _) = new_app(&t, "acme", &t.acme, json!({ "name": "failing", "webhook_url": bad_url, "events": ["change.opened"] })).await;
    let (_, mallory_app) = new_app(&t, "mallory", &t.mallory, json!({ "name": "mbot", "webhook_url": "http://127.0.0.1:9/m" })).await;

    // change.opened goes to the apps that asked for it on that repo, and only to them.
    let c = change(&t, "for apps", "alice", json!({ "notes.txt": { "content": "apps\n" } })).await;
    let id = c["id"].as_str().unwrap().to_string();
    until("change.opened", || !payloads(&all, "change.opened").is_empty()).await;
    let p = &payloads(&all, "change.opened")[0];
    assert_eq!((p["change"]["id"].as_str(), p["required"].is_null(), p["thread"].is_null()), (Some(id.as_str()), true, true), "{p}");
    for key in ["repo", "change", "compare", "diff", "blob", "reviews", "threads", "web"] {
        assert!(p["urls"][key].is_string(), "urls.{key}: {p}");
    }
    assert!(kinds(&get_change(&t, &id).await).is_empty(), "a disabled required app owes nothing");

    // A delivery that never gets a 2xx: 8 attempts with backoff, then failed, with the last answer.
    let failed = delivery_where(&t, &bad_id, "a failed delivery", |d| d["state"] == "failed").await;
    assert_eq!((failed["attempts"].as_u64(), failed["last_status"].as_u64()), (Some(8), Some(500)), "{failed}");
    assert_eq!((failed["last_error"].as_str(), failed["last_response"].as_str()), (Some("HTTP 500"), Some("boom")));
    assert!(failed["next_at"].is_null() && failed["delivered_at"].is_null());
    assert_eq!(bad.got.lock().len(), 8);
    // Redelivered once the endpoint works.
    bad.fail.store(0, std::sync::atomic::Ordering::SeqCst);
    ok(&t.a, "POST", &format!("/v1/code/review-apps/{bad_id}/deliveries/{}/redeliver", failed["id"].as_str().unwrap()), Some(json!({})), Some(&t.acme), &[]).await;
    let d = delivery_where(&t, &bad_id, "the redelivery", |d| d["state"] == "delivered").await;
    assert_eq!((d["id"].clone(), d["last_status"].as_u64()), (failed["id"].clone(), Some(204)));
    assert!(d["delivered_at"].is_number());

    // thread.comment: the thread and the comment, to apps that asked for it.
    let th = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(json!({ "path": "notes.txt", "line": 1, "body": "why apps?" })), Some(&t.acme), &[]).await;
    until("thread.comment", || !payloads(&notes, "thread.comment").is_empty() && !payloads(&all, "thread.comment").is_empty()).await;
    let p = &payloads(&notes, "thread.comment")[0];
    assert_eq!((p["thread"]["id"].clone(), p["comment"]["body"].as_str(), p["comment"]["author"]["name"].as_str()), (th["id"].clone(), Some("why apps?"), Some("acme")));
    assert_eq!(events(&notes), vec!["thread.comment"], "an app gets only the events it asked for");

    // Scope: an app installed elsewhere, or another owner's app, can read and comment on a public
    // repo but never approve or request changes there.
    for token in [&else_token, &mallory_app] {
        for verdict in ["approve", "request_changes"] {
            let (s, v) = review(&t, &id, verdict, token, &[]).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{verdict}: {v}");
        }
        let (s, v) = review(&t, &id, "comment", token, &[]).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    assert!(kinds(&get_change(&t, &id).await).is_empty(), "nothing an outside app said blocks");
    let (s, _) = call(&t.a, "GET", "/v1/code/repos/acme/api/review-settings", None, Some(&mallory_app), &[]).await;
    assert_eq!(s, StatusCode::OK, "public repos are public");
    let other = ok(&t.a, "POST", "/v1/code/repos/acme/other/changes", Some(json!({ "title": "o", "message": "o", "base": "main", "files": { "a.txt": { "content": "b\n" } } })), Some(ROOT), &[]).await;
    let (s, v) = review(&t, other["id"].as_str().unwrap(), "approve", &else_token, &[]).await;
    assert_eq!(s, StatusCode::CREATED, "on the repo it's installed on: {v}");
    until("change.opened elsewhere", || !payloads(&elsewhere, "change.opened").is_empty()).await;
    assert!(payloads(&elsewhere, "change.opened").iter().all(|p| p["repo"]["full_name"] == "acme/other"), "nothing from repos it isn't on");
    assert!(off.got.lock().is_empty(), "nothing to a disabled app");
    assert_eq!(ok(&t.a, "GET", &format!("/v1/code/review-apps/{off_id}/deliveries"), None, Some(&t.acme), &[]).await, json!([]));

    // review.requested by hand (names are case-insensitive); a required request clears when the
    // app reviews.
    let reqs = ok(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "app:ALL" })), Some(&t.acme), &[]).await;
    assert_eq!(reqs[0]["reviewer"], json!({ "kind": "app", "name": "all", "app": all_id }));
    until("review.requested", || !payloads(&all, "review.requested").is_empty()).await;
    assert_eq!(payloads(&all, "review.requested")[0]["required"], false);
    assert!(kinds(&get_change(&t, &id).await).is_empty());
    ok(&t.a, "POST", &format!("/v1/code/changes/{id}/review-requests"), Some(json!({ "reviewer": "app:all", "required": true })), Some(&t.acme), &[]).await;
    let c = get_change(&t, &id).await;
    assert_eq!(kinds(&c), vec!["review_pending"]);
    assert_eq!(c["blockers"][0]["message"], "waiting for a review from app:all");
    review(&t, &id, "comment", &all_token, &[]).await;
    assert!(kinds(&get_change(&t, &id).await).is_empty(), "any review of the head settles it");

    // A required app owes every new change a review, landing straight on main included.
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{all_id}"), Some(json!({ "required": true })), Some(&t.acme), &[]).await;
    let c2 = change(&t, "owed", "alice", json!({ "notes.txt": { "content": "owed\n" } })).await;
    let id2 = c2["id"].as_str().unwrap().to_string();
    assert_eq!(kinds(&get_change(&t, &id2).await), vec!["review_pending"]);
    until("required review.requested", || payloads(&all, "review.requested").iter().any(|p| p["required"] == true && p["change"]["id"] == id2.as_str())).await;
    let (s, v) = call(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "direct", "files": { "notes.txt": { "content": "direct\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["blockers"].as_array().unwrap().iter().map(|b| b["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["review_required", "review_pending"]);
    // Disabling the app stops it blocking (it can't review anymore); deleting it too.
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{all_id}"), Some(json!({ "active": false })), Some(&t.acme), &[]).await;
    assert!(kinds(&get_change(&t, &id2).await).is_empty(), "a disabled app's request doesn't block");
    ok(&t.a, "PATCH", &format!("/v1/code/review-apps/{all_id}"), Some(json!({ "active": true })), Some(&t.acme), &[]).await;
    assert_eq!(kinds(&get_change(&t, &id2).await), vec!["review_pending"]);
    let (s, _) = call(&t.a, "DELETE", &format!("/v1/code/review-apps/{all_id}"), None, Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(kinds(&get_change(&t, &id2).await).is_empty(), "a deleted app's request doesn't block");
    let v = ok(&t.a, "POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "direct 2", "files": { "notes.txt": { "content": "direct 2\n" } } })), Some(&t.acme), &[]).await;
    assert_eq!(v["status"], "landed");
}

// ── every route, every status ──

#[tokio::test]
async fn every_review_route_answers_every_status() {
    let t = setup().await;
    ok(&t.a, "POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "priv", "visibility": "private" })), Some(ROOT), &[]).await;
    ok(&t.a, "POST", "/v1/code/repos/acme/priv/land", Some(json!({ "message": "base", "files": { "a.txt": { "content": "a\n" } } })), Some(ROOT), &[]).await;
    // Apps first, so the changes below are delivered to them (to an address that refuses).
    let (bot, _) = new_app(&t, "acme", &t.acme, json!({ "name": "bot", "webhook_url": "http://127.0.0.1:9/x" })).await;
    let (bot2, _) = new_app(&t, "acme", &t.acme, json!({ "name": "bot2", "webhook_url": "http://127.0.0.1:9/y" })).await;
    let pc = change(&t, "pub", "alice", json!({ "notes.txt": { "content": "x\n" } })).await["id"].as_str().unwrap().to_string();
    let cc = change(&t, "closed", "alice", json!({ "notes.txt": { "content": "c\n" } })).await["id"].as_str().unwrap().to_string();
    let qc = ok(&t.a, "POST", "/v1/code/repos/acme/priv/changes", Some(json!({ "title": "p", "message": "p", "base": "main", "files": { "a.txt": { "content": "b\n" } } })), Some(ROOT), &[]).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let thread = |id: &str, path: &str, body: &str| {
        let (a, id, b) = (&t.a, id.to_string(), json!({ "path": path, "line": 1, "body": body }));
        async move { ok(a, "POST", &format!("/v1/code/changes/{id}/threads"), Some(b), Some(ROOT), &[]).await["id"].as_str().unwrap().to_string() }
    };
    let pt = thread(&pc, "notes.txt", "plain").await;
    let sug = thread(&pc, "notes.txt", "```suggestion\nX\n```").await;
    let qt = thread(&qc, "a.txt", "private").await;
    let ct = thread(&cc, "notes.txt", "```suggestion\nC\n```").await;
    ok(&t.a, "POST", &format!("/v1/code/changes/{cc}/close"), Some(json!({})), Some(ROOT), &[]).await;
    let own = ok(&t.a, "POST", &format!("/v1/code/changes/{pc}/reviews"), Some(json!({ "verdict": "approve" })), Some(&t.acme), &[("x-roll-agent", "d1")]).await["id"].as_str().unwrap().to_string();
    let rq = ok(&t.a, "POST", &format!("/v1/code/changes/{qc}/reviews"), Some(json!({ "verdict": "approve" })), Some(ROOT), &[]).await["id"].as_str().unwrap().to_string();
    let dl = delivery_where(&t, &bot, "a delivery to bot", |_| true).await["id"].as_str().unwrap().to_string();
    let dl2 = delivery_where(&t, &bot2, "a delivery to bot2", |_| true).await["id"].as_str().unwrap().to_string();

    let (acme, mallory) = (Some(t.acme.as_str()), Some(t.mallory.as_str()));
    let (anon, root): (Option<&str>, Option<&str>) = (None, Some(ROOT));
    let comment = || Some(json!({ "verdict": "comment", "body": "hi" }));
    let approve = || Some(json!({ "verdict": "approve" }));
    let th_body = |p: &str| Some(json!({ "path": p, "line": 1, "body": "x" }));
    let url = "http://127.0.0.1:9/z";
    type H = &'static [(&'static str, &'static str)];
    let none: H = &[];
    let s = |p: &str| p.to_string();
    use StatusCode as C;
    type Case<'a> = (&'a str, String, Option<Value>, Option<&'a str>, H, StatusCode);
    let cases: Vec<Case> = vec![
        // GET/PATCH /v1/code/repos/{owner}/{repo}/review-settings
        ("GET", s("/v1/code/repos/acme/api/review-settings"), None, anon, none, C::OK),
        ("GET", s("/v1/code/repos/acme/priv/review-settings"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/repos/acme/nope/review-settings"), None, root, none, C::NOT_FOUND),
        ("PATCH", s("/v1/code/repos/acme/api/review-settings"), Some(json!({ "required_approvals": 0 })), acme, none, C::OK),
        ("PATCH", s("/v1/code/repos/acme/api/review-settings"), Some(json!({ "required_approvals": -1 })), acme, none, C::BAD_REQUEST),
        ("PATCH", s("/v1/code/repos/acme/api/review-settings"), Some(json!({})), anon, none, C::UNAUTHORIZED),
        ("PATCH", s("/v1/code/repos/acme/api/review-settings"), Some(json!({})), mallory, none, C::FORBIDDEN),
        ("PATCH", s("/v1/code/repos/acme/priv/review-settings"), Some(json!({})), mallory, none, C::NOT_FOUND),
        ("PATCH", s("/v1/code/repos/acme/nope/review-settings"), Some(json!({})), root, none, C::NOT_FOUND),
        // GET/POST /v1/code/changes/{id}/reviews
        ("GET", format!("/v1/code/changes/{pc}/reviews"), None, anon, none, C::OK),
        ("GET", format!("/v1/code/changes/{qc}/reviews"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/changes/chg_nope/reviews"), None, root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), comment(), mallory, none, C::CREATED),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), Some(json!({})), acme, none, C::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), comment(), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/reviews"), approve(), mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{qc}/reviews"), comment(), mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/changes/chg_nope/reviews"), comment(), root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{cc}/reviews"), approve(), acme, none, C::CONFLICT),
        // POST /v1/code/changes/{id}/reviews/{rid}/dismiss
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({ "reason": "x".repeat(2001) })), acme, none, C::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({})), acme, &[("x-roll-agent", "bad name!")], C::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({})), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({})), mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{pc}/reviews/rv_nope/dismiss"), Some(json!({})), acme, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{rq}/dismiss"), Some(json!({})), acme, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{qc}/reviews/{rq}/dismiss"), Some(json!({})), mallory, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({ "reason": "x".repeat(2000) })), acme, &[("x-roll-agent", "d1")], C::OK),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss"), Some(json!({})), acme, none, C::CONFLICT),
        // GET/POST /v1/code/changes/{id}/threads
        ("GET", format!("/v1/code/changes/{pc}/threads"), None, anon, none, C::OK),
        ("GET", format!("/v1/code/changes/{qc}/threads"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/changes/chg_nope/threads"), None, root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/threads"), th_body("notes.txt"), mallory, none, C::CREATED),
        ("POST", format!("/v1/code/changes/{pc}/threads"), th_body("nope.txt"), acme, none, C::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{pc}/threads"), th_body("notes.txt"), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{qc}/threads"), th_body("a.txt"), mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/changes/chg_nope/threads"), th_body("a.txt"), root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{cc}/threads"), th_body("notes.txt"), acme, none, C::CONFLICT),
        // GET /v1/code/changes/{id}/review-status
        ("GET", format!("/v1/code/changes/{pc}/review-status"), None, anon, none, C::OK),
        ("GET", format!("/v1/code/changes/{qc}/review-status"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/changes/chg_nope/review-status"), None, root, none, C::NOT_FOUND),
        // GET/POST /v1/code/changes/{id}/review-requests
        ("GET", format!("/v1/code/changes/{pc}/review-requests"), None, anon, none, C::OK),
        ("GET", format!("/v1/code/changes/{qc}/review-requests"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/changes/chg_nope/review-requests"), None, root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), acme, none, C::CREATED),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({})), acme, none, C::BAD_REQUEST),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "roll" })), mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/changes/{pc}/review-requests"), Some(json!({ "reviewer": "app:nobody" })), acme, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/changes/{qc}/review-requests"), Some(json!({ "reviewer": "roll" })), mallory, none, C::NOT_FOUND),
        // GET /v1/code/threads/{id}
        ("GET", format!("/v1/code/threads/{pt}"), None, anon, none, C::OK),
        ("GET", format!("/v1/code/threads/{qt}"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/threads/th_nope"), None, root, none, C::NOT_FOUND),
        // POST /v1/code/threads/{id}/comments
        ("POST", format!("/v1/code/threads/{pt}/comments"), Some(json!({ "body": "x" })), mallory, none, C::CREATED),
        ("POST", format!("/v1/code/threads/{pt}/comments"), Some(json!({ "body": " " })), mallory, none, C::BAD_REQUEST),
        ("POST", format!("/v1/code/threads/{pt}/comments"), Some(json!({ "body": "x" })), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{qt}/comments"), Some(json!({ "body": "x" })), mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/threads/th_nope/comments"), Some(json!({ "body": "x" })), root, none, C::NOT_FOUND),
        // POST /v1/code/threads/{id}/resolve and /unresolve
        ("POST", format!("/v1/code/threads/{pt}/resolve"), None, acme, none, C::OK),
        ("POST", format!("/v1/code/threads/{pt}/resolve"), None, anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{pt}/resolve"), None, mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/threads/{qt}/resolve"), None, mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/threads/th_nope/resolve"), None, root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/threads/{pt}/unresolve"), None, acme, none, C::OK),
        ("POST", format!("/v1/code/threads/{pt}/unresolve"), None, anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{pt}/unresolve"), None, mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/threads/{qt}/unresolve"), None, mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/threads/th_nope/unresolve"), None, root, none, C::NOT_FOUND),
        // POST /v1/code/threads/{id}/apply
        ("POST", format!("/v1/code/threads/{pt}/apply"), Some(json!({})), acme, none, C::UNPROCESSABLE_ENTITY),
        ("POST", format!("/v1/code/threads/{sug}/apply"), Some(json!({ "comment": "cm_nope" })), acme, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/threads/{ct}/apply"), Some(json!({})), acme, none, C::CONFLICT),
        ("POST", format!("/v1/code/threads/{sug}/apply"), Some(json!({})), anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/threads/{sug}/apply"), Some(json!({})), mallory, none, C::FORBIDDEN),
        ("POST", format!("/v1/code/threads/{qt}/apply"), Some(json!({})), mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/threads/th_nope/apply"), Some(json!({})), root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/threads/{sug}/apply"), Some(json!({})), acme, none, C::OK),
        // GET/POST /v1/code/owners/{owner}/review-apps
        ("GET", s("/v1/code/owners/acme/review-apps"), None, acme, none, C::OK),
        ("GET", s("/v1/code/owners/acme/review-apps"), None, anon, none, C::UNAUTHORIZED),
        ("GET", s("/v1/code/owners/acme/review-apps"), None, mallory, none, C::FORBIDDEN),
        ("GET", s("/v1/code/owners/nobody/review-apps"), None, root, none, C::NOT_FOUND),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "bot3", "webhook_url": url })), acme, none, C::CREATED),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "bot4" })), acme, none, C::BAD_REQUEST),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "bot4", "webhook_url": url })), anon, none, C::UNAUTHORIZED),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "bot4", "webhook_url": url })), mallory, none, C::FORBIDDEN),
        ("POST", s("/v1/code/owners/nobody/review-apps"), Some(json!({ "name": "bot4", "webhook_url": url })), root, none, C::NOT_FOUND),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "bot4", "webhook_url": url, "repos": ["nope"] })), acme, none, C::NOT_FOUND),
        ("POST", s("/v1/code/owners/acme/review-apps"), Some(json!({ "name": "BOT", "webhook_url": url })), acme, none, C::CONFLICT),
        // GET/PATCH/DELETE /v1/code/review-apps/{id}
        ("GET", format!("/v1/code/review-apps/{bot}"), None, acme, none, C::OK),
        ("GET", format!("/v1/code/review-apps/{bot}"), None, anon, none, C::UNAUTHORIZED),
        ("GET", format!("/v1/code/review-apps/{bot}"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/review-apps/app_nope"), None, root, none, C::NOT_FOUND),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({ "required": false })), acme, none, C::OK),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({ "events": ["push"] })), acme, none, C::BAD_REQUEST),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({ "active": "yes" })), acme, none, C::BAD_REQUEST),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({ "repos": ["mallory/x"] })), acme, none, C::BAD_REQUEST),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({})), anon, none, C::UNAUTHORIZED),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({})), mallory, none, C::NOT_FOUND),
        ("PATCH", s("/v1/code/review-apps/app_nope"), Some(json!({})), root, none, C::NOT_FOUND),
        ("PATCH", format!("/v1/code/review-apps/{bot}"), Some(json!({ "name": "Bot2" })), acme, none, C::CONFLICT),
        // GET /v1/code/review-apps/{id}/deliveries, POST …/deliveries/{did}/redeliver
        ("GET", format!("/v1/code/review-apps/{bot}/deliveries"), None, acme, none, C::OK),
        ("GET", format!("/v1/code/review-apps/{bot}/deliveries"), None, anon, none, C::UNAUTHORIZED),
        ("GET", format!("/v1/code/review-apps/{bot}/deliveries"), None, mallory, none, C::NOT_FOUND),
        ("GET", s("/v1/code/review-apps/app_nope/deliveries"), None, root, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/{dl}/redeliver"), None, acme, none, C::OK),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/{dl}/redeliver"), None, anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/{dl}/redeliver"), None, mallory, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/dl_nope/redeliver"), None, acme, none, C::NOT_FOUND),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/{dl2}/redeliver"), None, acme, none, C::NOT_FOUND),
        // POST /v1/code/review-apps/{id}/rotate
        ("POST", format!("/v1/code/review-apps/{bot}/rotate"), None, acme, none, C::OK),
        ("POST", format!("/v1/code/review-apps/{bot}/rotate"), None, anon, none, C::UNAUTHORIZED),
        ("POST", format!("/v1/code/review-apps/{bot}/rotate"), None, mallory, none, C::NOT_FOUND),
        ("POST", s("/v1/code/review-apps/app_nope/rotate"), None, root, none, C::NOT_FOUND),
        // DELETE /v1/code/review-apps/{id}
        ("DELETE", format!("/v1/code/review-apps/{bot2}"), None, anon, none, C::UNAUTHORIZED),
        ("DELETE", format!("/v1/code/review-apps/{bot2}"), None, mallory, none, C::NOT_FOUND),
        ("DELETE", format!("/v1/code/review-apps/{bot2}"), None, acme, none, C::NO_CONTENT),
        ("DELETE", format!("/v1/code/review-apps/{bot2}"), None, acme, none, C::NOT_FOUND),
    ];
    for (m, p, b, tok, h, want) in cases {
        let (got, v) = call(&t.a, m, &p, b.clone(), tok, h).await;
        assert_eq!(got, want, "{m} {p} as {tok:?} {h:?} with {b:?}: {v}");
        // Every error says what went wrong.
        if !got.is_success() {
            assert!(v["error"].is_string(), "{m} {p}: {v}");
        }
    }
    // A token roll doesn't know is 401 on every route.
    for (m, p) in [
        ("GET", "/v1/code/repos/acme/api/review-settings".to_string()),
        ("PATCH", "/v1/code/repos/acme/api/review-settings".into()),
        ("GET", format!("/v1/code/changes/{pc}/reviews")),
        ("POST", format!("/v1/code/changes/{pc}/reviews")),
        ("POST", format!("/v1/code/changes/{pc}/reviews/{own}/dismiss")),
        ("GET", format!("/v1/code/changes/{pc}/threads")),
        ("POST", format!("/v1/code/changes/{pc}/threads")),
        ("GET", format!("/v1/code/changes/{pc}/review-status")),
        ("GET", format!("/v1/code/changes/{pc}/review-requests")),
        ("POST", format!("/v1/code/changes/{pc}/review-requests")),
        ("GET", format!("/v1/code/threads/{pt}")),
        ("POST", format!("/v1/code/threads/{pt}/comments")),
        ("POST", format!("/v1/code/threads/{pt}/resolve")),
        ("POST", format!("/v1/code/threads/{pt}/unresolve")),
        ("POST", format!("/v1/code/threads/{pt}/apply")),
        ("GET", "/v1/code/owners/acme/review-apps".into()),
        ("POST", "/v1/code/owners/acme/review-apps".into()),
        ("GET", format!("/v1/code/review-apps/{bot}")),
        ("PATCH", format!("/v1/code/review-apps/{bot}")),
        ("DELETE", format!("/v1/code/review-apps/{bot}")),
        ("POST", format!("/v1/code/review-apps/{bot}/rotate")),
        ("GET", format!("/v1/code/review-apps/{bot}/deliveries")),
        ("POST", format!("/v1/code/review-apps/{bot}/deliveries/{dl}/redeliver")),
    ] {
        let body = (m != "GET" && m != "DELETE").then(|| json!({}));
        let (got, v) = call(&t.a, m, &p, body, Some("garbage"), &[]).await;
        assert_eq!(got, StatusCode::UNAUTHORIZED, "{m} {p}: {v}");
    }
    assert_eq!(call(&t.a, "GET", &format!("/v1/code/review-apps/{bot}"), None, Some(&t.acme), &[]).await.0, StatusCode::OK, "still there");

    // At most 50 review apps per owner: 422.
    let have = ok(&t.a, "GET", "/v1/code/owners/acme/review-apps", None, Some(&t.acme), &[]).await.as_array().unwrap().len();
    for i in have..50 {
        new_app(&t, "acme", &t.acme, json!({ "name": format!("many{i}"), "webhook_url": url })).await;
    }
    let (s, v) = call(&t.a, "POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "one-too-many", "webhook_url": url })), Some(&t.acme), &[]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}
