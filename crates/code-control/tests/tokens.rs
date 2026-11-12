//! Owner tokens: listed without their secrets, last use noted, revoked at once for the API, git
//! and MCP sessions bound to them, and only by their owner (or admin).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app, git_raw};
use serde_json::{Value, json};

const ADMIN: Option<&str> = Some("admin-secret");

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_are_listed_and_revoked_everywhere_at_once() {
    let a = app(ADMIN);
    for o in ["acme", "other"] {
        let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": o, "kind": "org" })), ADMIN).await;
        assert_eq!(s, StatusCode::CREATED);
    }
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api", "visibility": "private" })), ADMIN).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, t1) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "ci" })), ADMIN).await;
    let (_, t2) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "agent" })), ADMIN).await;
    let (_, other) = a.req("POST", "/v1/code/owners/other/tokens", Some(json!({ "name": "theirs" })), ADMIN).await;
    let tok = t1["token"].as_str().unwrap().to_string();
    let other_tok = other["token"].as_str().unwrap().to_string();
    assert!(t1["id"].as_str().unwrap().starts_with("tok_"));
    assert_eq!(t1["prefix"], json!(&tok[..8]));
    let (s, v) = a.req("POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "init", "files": { "a.txt": { "content": "a\n" } } })), Some(&tok)).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // Listed by the owner (and admin): ids, names, prefixes, last use; never the secret.
    let (s, list) = a.req("GET", "/v1/code/owners/acme/tokens", None, Some(&tok)).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let list = list.as_array().unwrap().clone();
    assert_eq!(list.len(), 2);
    let text = Value::Array(list.clone()).to_string();
    assert!(!text.contains(&tok) && !text.contains(t2["token"].as_str().unwrap()), "no secrets in {text}");
    let mine = list.iter().find(|t| t["id"] == t1["id"]).unwrap();
    assert_eq!((mine["name"].as_str(), mine["prefix"].as_str()), (Some("ci"), Some(&tok[..8])));
    assert!(mine["last_used_at"].as_i64().is_some(), "used: {mine}");
    let unused = list.iter().find(|t| t["id"] == t2["id"]).unwrap();
    assert!(unused["last_used_at"].is_null());
    for k in ["id", "name", "created_at", "last_used_at", "prefix"] {
        assert!(mine.get(k).is_some(), "{k}");
    }
    assert_eq!(a.req("GET", "/v1/code/owners/acme/tokens", None, ADMIN).await.0, StatusCode::OK);
    // Not anyone else's to see or revoke.
    assert_eq!(a.req("GET", "/v1/code/owners/acme/tokens", None, Some(&other_tok)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.req("GET", "/v1/code/owners/acme/tokens", None, None).await.0, StatusCode::NOT_FOUND);
    let path = format!("/v1/code/owners/acme/tokens/{}", t1["id"].as_str().unwrap());
    assert_eq!(a.req("DELETE", &path, None, Some(&other_tok)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(a.req("DELETE", &path, None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(a.req("DELETE", "/v1/code/owners/acme/tokens/tok_nope", None, Some(&tok)).await.0, StatusCode::NOT_FOUND);
    // Another owner's token id under this owner: not found here.
    assert_eq!(a.req("DELETE", &format!("/v1/code/owners/acme/tokens/{}", other["id"].as_str().unwrap()), None, ADMIN).await.0, StatusCode::NOT_FOUND);

    // Working before: API, git, an MCP session.
    assert_eq!(a.req("GET", "/v1/code/repos/acme/api", None, Some(&tok)).await.0, StatusCode::OK);
    let base = a.serve().await;
    let home = tempfile::tempdir().unwrap();
    let url = format!("{}/git/acme/api.git", base.replace("http://", &format!("http://x:{tok}@")));
    let out = git_raw(home.path(), home.path(), &["clone", "-q", &url, "before"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mcp = |body: Value, session: Option<&str>| {
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/code/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", format!("Bearer {tok}"));
        if let Some(s) = session {
            b = b.header("mcp-session-id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } });
    let resp = a.send(mcp(init.clone(), None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let session = resp.headers()["mcp-session-id"].to_str().unwrap().to_string();
    assert_eq!(a.send(mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }), Some(&session))).await.status(), StatusCode::OK);

    // Revoked: gone from the list, and refused everywhere at once.
    assert_eq!(a.req("DELETE", &path, None, Some(&tok)).await.0, StatusCode::NO_CONTENT);
    let (_, list) = a.req("GET", "/v1/code/owners/acme/tokens", None, ADMIN).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(a.req("GET", "/v1/code/repos/acme/api", None, Some(&tok)).await.0, StatusCode::UNAUTHORIZED);
    let out = git_raw(home.path(), home.path(), &["clone", "-q", &url, "after"]);
    assert!(!out.status.success(), "git refuses a revoked token");
    assert_eq!(a.send(mcp(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }), Some(&session))).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(a.send(mcp(init, None)).await.status(), StatusCode::UNAUTHORIZED, "no new session either");
    // The other token still works.
    assert_eq!(a.req("GET", "/v1/code/repos/acme/api", None, t2["token"].as_str()).await.0, StatusCode::OK);
}

/// A token named `agent:<name>` is that agent: it works code (land, changes, reviews) but can't
/// manage its owner (tokens, billing, providers, review apps, webhooks, repo and review
/// settings, the queue) and can't review as another agent, so one agent can't mint approvals
/// from several "agents" or escape its binding by creating itself an unbound token.
#[tokio::test]
async fn agent_tokens_work_code_but_cant_manage_or_impersonate() {
    let a = app(ADMIN);
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), ADMIN).await;
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "api", "visibility": "private" })), ADMIN).await;
    assert_eq!(s, StatusCode::CREATED);
    let tok = |name: &str| {
        let a = &a;
        let name = name.to_string();
        async move { a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": name })), ADMIN).await.1["token"].as_str().unwrap().to_string() }
    };
    let alice = tok("agent:alice").await;
    let bob = tok("agent:bob").await;
    let owner = tok("deploy").await;
    let (s, v) = a.req("POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "init", "files": { "a.txt": { "content": "a\n" } } })), Some(&alice)).await;
    assert_eq!(s, StatusCode::OK, "agents land code: {v}");
    let (s, v) = a.req("PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "required_approvals": 2 })), Some(&owner)).await;
    assert_eq!(s, StatusCode::OK, "the owner's own token manages: {v}");

    // Management: refused to the agent.
    let forbidden: &[(&str, &str, Option<Value>)] = &[
        ("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "escape" }))),
        ("PATCH", "/v1/code/owners/acme", Some(json!({ "bio": "x" }))),
        ("PATCH", "/v1/code/repos/acme/api", Some(json!({ "visibility": "public" }))),
        ("DELETE", "/v1/code/repos/acme/api", None),
        ("PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "required_approvals": 0 }))),
        ("PATCH", "/v1/code/repos/acme/api/reviewer", Some(json!({ "enabled": true }))),
        ("POST", "/v1/code/repos/acme/api/webhooks", Some(json!({ "url": "https://example.com/hook", "events": ["landed"] }))),
        ("GET", "/v1/code/repos/acme/api/webhooks", None),
        ("POST", "/v1/code/repos/acme/api/queue/pause", None),
        ("PUT", "/v1/code/owners/acme/billing", Some(json!({ "monthly_cap_usd": 1000 }))),
        ("PUT", "/v1/code/owners/acme/review-provider", Some(json!({ "provider": "anthropic", "api_key": "sk-x" }))),
        ("DELETE", "/v1/code/owners/acme/review-provider", None),
        ("POST", "/v1/code/owners/acme/review-apps", Some(json!({ "name": "bot", "webhook_url": "https://example.com/r" }))),
    ];
    for (m, p, b) in forbidden {
        let (s, v) = a.req(m, p, b.clone(), Some(&alice)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{m} {p}: {v}");
    }
    for p in ["/v1/code/owners/acme/tokens", "/v1/code/owners/acme/billing", "/v1/code/owners/acme/usage", "/v1/code/owners/acme/review-provider"] {
        assert_eq!(a.req("GET", p, None, Some(&alice)).await.0, StatusCode::NOT_FOUND, "{p}");
    }
    assert_eq!(a.req("GET", "/v1/code/repos/acme/api", None, Some(&alice)).await.0, StatusCode::OK, "still reads the repo");

    // Reviews: as itself only.
    let (s, c) = a.req("POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": "t", "message": "t", "base": "main", "author": "carol", "files": { "a.txt": { "content": "b\n" } } })), ADMIN).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let reviews = format!("/v1/code/changes/{}/reviews", c["id"].as_str().unwrap());
    let approve = |token: &str, as_agent: Option<&str>| {
        let mut b = Request::builder().method("POST").uri(&reviews).header("authorization", format!("Bearer {token}")).header("content-type", "application/json");
        if let Some(x) = as_agent {
            b = b.header("x-roll-agent", x);
        }
        b.body(Body::from(json!({ "verdict": "approve", "body": "" }).to_string())).unwrap()
    };
    assert_eq!(a.send(approve(&alice, Some("bob"))).await.status(), StatusCode::FORBIDDEN, "alice can't approve as bob");
    assert_eq!(a.send(approve(&alice, Some("mallory"))).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(a.send(approve(&alice, Some("alice"))).await.status(), StatusCode::CREATED);
    assert_eq!(a.send(approve(&bob, None)).await.status(), StatusCode::CREATED);
    let (_, list) = a.req("GET", &reviews, None, ADMIN).await;
    let mut who: Vec<String> = list.as_array().unwrap().iter().map(|r| format!("{}:{}", r["reviewer"]["kind"].as_str().unwrap(), r["reviewer"]["name"].as_str().unwrap())).collect();
    who.sort();
    assert_eq!(who, ["agent:alice", "agent:bob"]);
    // An unbound owner token may still say which agent it runs (the owner's own call).
    let mut req = approve(&owner, Some("carol-bot"));
    req.headers_mut().insert("x-roll-agent", "carol-bot".parse().unwrap());
    assert_eq!(a.send(req).await.status(), StatusCode::CREATED);
}
