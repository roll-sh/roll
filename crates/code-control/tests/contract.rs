//! The rest of the /v1/code contract (docs/code/http-api.md): the auth matrix, CORS, GitHub
//! endpoints without an App, error statuses, writes and binary content, blob caching, `/paths`
//! truncation, stats (merges_daily, languages, contributors), event shapes, previews through a
//! backend, lookups by number and id, and state surviving a restart.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use code_control::model::{Change, Preview, PreviewState, RepoRow};
use code_control::{Caller, PreviewBackend};
use common::{App, app, app_with, open_in};
use futures::StreamExt;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const ADMIN: &str = "admin-secret";

/// `acme` (with a token) owns a public and a private repo with one landing each; `other` has its
/// own token. Returns (acme token, other token).
async fn two_owners(a: &App) -> (String, String) {
    let admin = Some(ADMIN);
    let mut tokens = Vec::new();
    for owner in ["acme", "other"] {
        let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": owner, "kind": "org" })), admin).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let (s, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": "ci" })), admin).await;
        assert_eq!(s, StatusCode::CREATED, "{t}");
        assert_eq!(t["owner"], owner);
        assert_eq!(t["name"], "ci");
        tokens.push(t["token"].as_str().unwrap().to_string());
    }
    for (name, visibility) in [("pub", "public"), ("priv", "private")] {
        let (s, v) = a
            .req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": visibility })), admin)
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["visibility"], visibility);
        let (s, v) = a
            .req("POST", &format!("/v1/code/repos/acme/{name}/land"), Some(json!({ "message": "init", "files": { "a.txt": { "content": "a" } } })), admin)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    (tokens.remove(0), tokens.remove(0))
}

#[tokio::test]
async fn auth_matrix() {
    let a = app(Some(ADMIN));
    let (acme, other) = two_owners(&a).await;
    let land = |n: usize| json!({ "message": format!("w{n}"), "files": { format!("f{n}.txt"): { "content": "x" } } });

    // (caller, read public, read private, write public, write private)
    let ok = StatusCode::OK;
    let cases: [(&str, Option<&str>, [StatusCode; 4]); 6] = [
        ("admin", Some(ADMIN), [ok, ok, ok, ok]),
        ("owner", Some(acme.as_str()), [ok, ok, ok, ok]),
        ("other owner", Some(other.as_str()), [ok, StatusCode::NOT_FOUND, StatusCode::FORBIDDEN, StatusCode::NOT_FOUND]),
        ("anonymous", None, [ok, StatusCode::NOT_FOUND, StatusCode::UNAUTHORIZED, StatusCode::NOT_FOUND]),
        ("unknown roll token", Some("rlc_0000"), [StatusCode::UNAUTHORIZED; 4]),
        ("garbage token", Some("hunter2"), [StatusCode::UNAUTHORIZED; 4]),
    ];
    let mut n = 0;
    for (who, token, [rp, rv, wp, wv]) in cases {
        let (s, _) = a.req("GET", "/v1/code/repos/acme/pub/tree/main", None, token).await;
        assert_eq!(s, rp, "{who} reads public");
        let (s, _) = a.req("GET", "/v1/code/repos/acme/priv/tree/main", None, token).await;
        assert_eq!(s, rv, "{who} reads private");
        n += 1;
        let (s, _) = a.req("POST", "/v1/code/repos/acme/pub/land", Some(land(n)), token).await;
        assert_eq!(s, wp, "{who} writes public");
        n += 1;
        let (s, _) = a.req("POST", "/v1/code/repos/acme/priv/land", Some(land(n)), token).await;
        assert_eq!(s, wv, "{who} writes private");
    }

    // The token also works as the password of HTTP Basic (what git sends).
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("anything:{acme}"));
    let req = Request::get("/v1/code/repos/acme/priv").header("authorization", format!("Basic {basic}")).body(Body::empty()).unwrap();
    assert_eq!(a.send(req).await.status(), StatusCode::OK);

    // Owner-scoped actions.
    let (s, _) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "x" })), Some(&other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("PATCH", "/v1/code/owners/acme", Some(json!({ "bio": "x" })), Some(&other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("PATCH", "/v1/code/owners/acme", Some(json!({ "bio": "ours" })), Some(&acme)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = a.req("PATCH", "/v1/code/repos/acme/pub", Some(json!({ "description": "x" })), Some(&other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("DELETE", "/v1/code/repos/acme/pub", None, Some(&other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "mine" })), Some(&acme)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only the admin creates owners");
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "theirs" })), Some(&other)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("GET", "/v1/code/github/installations", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = a.req("GET", "/v1/code/github/installations", None, Some(&acme)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "installations span tenants: admin only");
    let (s, v) = a.req("GET", "/v1/code/github/installations", None, Some(ADMIN)).await;
    assert_eq!((s, v), (StatusCode::OK, json!([])));

    // A private repo's existence and new name stay hidden from callers who can't read it.
    for (token, taken) in [(None, false), (Some(other.as_str()), false), (Some(acme.as_str()), true), (Some(ADMIN), true)] {
        let (s, v) = a.req("GET", "/v1/code/names/acme/priv", None, token).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["available"], !taken, "{token:?}: {v}");
        let (_, owners) = a.req("GET", "/v1/code/owners", None, token).await;
        let count = owners.as_array().unwrap().iter().find(|o| o["name"] == "acme").unwrap()["repos_count"].clone();
        assert_eq!(count, if taken { 2 } else { 1 }, "{token:?}");
    }

    // Changes and work items in a private repo, by id.
    let (_, ch) = a
        .req("POST", "/v1/code/repos/acme/priv/changes", Some(json!({ "title": "t", "message": "t", "files": { "c.txt": { "content": "c" } } })), Some(&acme))
        .await;
    let cid = ch["id"].as_str().unwrap().to_string();
    let (_, w) = a.req("POST", "/v1/code/repos/acme/priv/work", Some(json!({ "title": "w" })), Some(&acme)).await;
    let wid = w["id"].as_str().unwrap().to_string();
    for (token, read, write) in [
        (None, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
        (Some(other.as_str()), StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
        (Some(acme.as_str()), StatusCode::OK, StatusCode::OK),
    ] {
        assert_eq!(a.req("GET", &format!("/v1/code/changes/{cid}"), None, token).await.0, read);
        assert_eq!(a.req("GET", &format!("/v1/code/work/{wid}"), None, token).await.0, read);
        assert_eq!(a.req("POST", &format!("/v1/code/changes/{cid}/checks"), Some(json!({ "name": "t", "status": "running" })), token).await.0, write);
        assert_eq!(a.req("POST", &format!("/v1/code/work/{wid}/release"), Some(json!({})), token).await.0, write);
    }
    // On the public repo another owner can read but not write.
    let (_, ch) = a
        .req("POST", "/v1/code/repos/acme/pub/changes", Some(json!({ "title": "t", "message": "t", "files": { "c.txt": { "content": "c" } } })), Some(&acme))
        .await;
    let pid = ch["id"].as_str().unwrap();
    assert_eq!(a.req("GET", &format!("/v1/code/changes/{pid}"), None, Some(&other)).await.0, StatusCode::OK);
    assert_eq!(a.req("POST", &format!("/v1/code/changes/{pid}/land"), None, Some(&other)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(a.req("POST", &format!("/v1/code/changes/{pid}/close"), None, None).await.0, StatusCode::UNAUTHORIZED);

    // GET /v1/code says who a token is (the dashboard's sign-in), and never fails on a bad one.
    // `reviewer` is who it reviews and comments as.
    for (token, kind, owner, reviewer) in [
        (Some(ADMIN), "admin", Value::Null, json!({ "kind": "user", "name": "admin" })),
        (Some(acme.as_str()), "owner", json!("acme"), json!({ "kind": "user", "name": "acme" })),
        (None, "anonymous", Value::Null, Value::Null),
        (Some("rlc_nope"), "invalid", Value::Null, Value::Null),
    ] {
        let (s, v) = a.req("GET", "/v1/code", None, token).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["auth"]["required"], true);
        assert_eq!(v["auth"]["caller"], json!({ "kind": kind, "owner": owner, "reviewer": reviewer }), "{token:?}");
    }

    // The default branch can move to another branch; landings and HEAD follow it.
    let (s, v) = a.req("POST", "/v1/code/repos/acme/pub/land", Some(json!({ "ref": "trunk", "message": "t", "files": { "t.txt": { "content": "t" } } })), Some(&acme)).await;
    assert_eq!((s, v["ref"].as_str()), (StatusCode::OK, Some("trunk")));
    let (_, r) = a.req("PATCH", "/v1/code/repos/acme/pub", Some(json!({ "default_branch": "refs/heads/trunk" })), Some(&acme)).await;
    assert_eq!(r["default_branch"], "trunk");
    let (_, v) = a.req("POST", "/v1/code/repos/acme/pub/land", Some(json!({ "message": "on trunk", "files": { "u.txt": { "content": "u" } } })), Some(&acme)).await;
    assert_eq!(v["ref"], "trunk");
    let (_, r) = a.req("GET", "/v1/code/repos/acme/pub", None, Some(&acme)).await;
    assert_eq!(r["head"], v["changeset"]);

    // Listings hide what the caller can't read.
    let (_, list) = a.req("GET", "/v1/code/repos", None, Some(&other)).await;
    assert_eq!(list.as_array().unwrap().iter().map(|r| r["full_name"].as_str().unwrap()).collect::<Vec<_>>(), ["acme/pub"]);
    let (_, list) = a.req("GET", "/v1/code/repos", None, Some(&acme)).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (s, _) = a.req("GET", "/v1/code/repos?owner=nobody", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn open_mode_makes_everyone_admin() {
    let a = app(None);
    assert_eq!(a.code.caller(None).unwrap(), Caller::Admin);
    assert_eq!(a.code.caller(Some("anything")).unwrap(), Caller::Admin);
    let info = a.ok("GET", "/v1/code", None).await;
    assert_eq!(info["auth"], json!({ "required": false, "caller": { "kind": "admin", "owner": null, "reviewer": { "kind": "user", "name": "admin" } } }));
    a.repo("acme", "x").await;
    a.ok("PATCH", "/v1/code/repos/acme/x", Some(json!({ "visibility": "private" }))).await;
    a.ok("GET", "/v1/code/repos/acme/x", None).await;
    let owners = a.ok("GET", "/v1/code/owners", None).await;
    assert_eq!(owners[0]["name"], "acme");
    assert_eq!(owners[0]["repos_count"], 1);
}

#[tokio::test]
async fn cors_for_the_browser() {
    let a = app(None);
    a.repo("acme", "x").await;
    a.ok("POST", "/v1/code/repos/acme/x/land", Some(json!({ "message": "m", "files": { "a.txt": { "content": "a" } } }))).await;
    let pre = Request::builder()
        .method("OPTIONS")
        .uri("/v1/code/repos/acme/x/land")
        .header("origin", "https://roll.sh")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization,content-type")
        .body(Body::empty())
        .unwrap();
    let r = a.send(pre).await;
    assert!(r.status().is_success(), "{}", r.status());
    let h = r.headers();
    assert_eq!(h["access-control-allow-origin"], "*");
    let allowed = h["access-control-allow-headers"].to_str().unwrap().to_ascii_lowercase();
    assert!(allowed.contains("authorization") && allowed.contains("content-type"), "{allowed}");
    assert!(h["access-control-allow-methods"].to_str().unwrap().contains('*') || h["access-control-allow-methods"].to_str().unwrap().contains("POST"));

    let get = Request::get("/v1/code/repos/acme/x/blob/main/a.txt").header("origin", "http://localhost:3000").body(Body::empty()).unwrap();
    let r = a.send(get).await;
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    let exposed = r.headers()["access-control-expose-headers"].to_str().unwrap().to_ascii_lowercase();
    assert!(exposed.contains("x-roll-changeset"), "{exposed}");
}

#[tokio::test]
async fn github_without_an_app() {
    let a = app(None);
    let v = a.ok("GET", "/v1/code/github", None).await;
    assert_eq!(v, json!({ "configured": false, "app_slug": null, "install_url": null }));
    assert_eq!(a.ok("GET", "/v1/code", None).await["github"]["configured"], false);
    assert_eq!(a.ok("GET", "/v1/code/github/installations", None).await, json!([]));
    let r = a.send(Request::get("/v1/code/github/setup?installation_id=1&setup_action=install").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::FOUND);
    assert_eq!(r.headers()["location"], "https://roll.sh/dashboard/code?github=installed");
    let (s, v) = a.req("POST", "/v1/code/github/webhook", Some(json!({ "zen": "x" })), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    // Connecting a GitHub repo needs the App.
    a.repo("acme", "seed").await;
    let (s, v) = a
        .req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "gh", "source": { "github": { "installation_id": 1, "owner": "o", "repo": "r" } } })), None)
        .await;
    assert!(s.is_client_error(), "{s} {v}");
    assert!(v["error"].is_string());
}

#[tokio::test]
async fn errors_are_json_with_the_right_status() {
    let a = app(None);
    a.repo("acme", "x").await;
    let p = "/v1/code/repos/acme/x";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a.txt": { "content": "a" } } }))).await;

    let bad_json = Request::post(format!("{p}/land")).header("content-type", "application/json").body(Body::from("{nope")).unwrap();
    let r = a.send(bad_json).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert!(v["error"].as_str().unwrap().contains("invalid JSON"), "{v}");

    let cases: Vec<(&str, String, Option<Value>, StatusCode)> = vec![
        ("GET", "/v1/code/owners/nobody".into(), None, StatusCode::NOT_FOUND),
        ("GET", "/v1/code/repos/acme/nope".into(), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/tree/nope"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/blob/main/missing.txt"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/changesets/cs_ffffffff"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/compare/main"), None, StatusCode::BAD_REQUEST),
        ("GET", format!("{p}/compare/main...nope"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/readme/main"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/changes?state=bogus"), None, StatusCode::BAD_REQUEST),
        ("GET", format!("{p}/work?state=bogus"), None, StatusCode::BAD_REQUEST),
        ("GET", format!("{p}/changes/99"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/changes/abc"), None, StatusCode::NOT_FOUND),
        ("GET", format!("{p}/work/99"), None, StatusCode::NOT_FOUND),
        ("GET", "/v1/code/work/wi_missing".into(), None, StatusCode::NOT_FOUND),
        ("POST", format!("{p}/land"), Some(json!({ "files": { "a.txt": { "content": "b" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m", "files": {} })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m", "files": { "../etc/passwd": { "content": "x" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m", "files": { ".git/config": { "content": "x" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m", "files": { "b": { "content_b64": "%%%" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "message": "m", "files": { "b": { "content": "x", "content_b64": "eA==" } } })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/land"), Some(json!({ "base": "nope", "message": "m", "files": { "b": { "content": "x" } } })), StatusCode::NOT_FOUND),
        ("POST", format!("{p}/land"), Some(json!({ "changeset": "cs_ffffffff" })), StatusCode::NOT_FOUND),
        ("POST", format!("{p}/changes"), Some(json!({ "body": "no title", "changeset": "main" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/changes"), Some(json!({ "title": "t", "work_item": "wi_missing", "message": "m", "files": { "b": { "content": "x" } } })), StatusCode::NOT_FOUND),
        ("POST", format!("{p}/work"), Some(json!({ "body": "no title" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/work"), Some(json!({ "title": "t", "labels": "notalist" })), StatusCode::BAD_REQUEST),
        ("POST", format!("{p}/work/claim"), Some(json!({})), StatusCode::BAD_REQUEST),
        ("POST", "/v1/code/owners".into(), Some(json!({ "name": "x", "kind": "robot" })), StatusCode::BAD_REQUEST),
        ("POST", "/v1/code/repos".into(), Some(json!({ "owner": "nobody", "name": "x" })), StatusCode::NOT_FOUND),
        ("POST", "/v1/code/repos".into(), Some(json!({ "owner": "acme", "name": "y", "default_branch": "a..b" })), StatusCode::BAD_REQUEST),
        ("PATCH", p.to_string(), Some(json!({ "visibility": "secret" })), StatusCode::BAD_REQUEST),
        ("PATCH", p.to_string(), Some(json!({ "land_policy": "yolo" })), StatusCode::BAD_REQUEST),
    ];
    for (method, path, body, want) in cases {
        let (s, v) = a.req(method, &path, body.clone(), None).await;
        assert_eq!(s, want, "{method} {path} {body:?}: {v}");
        assert!(v["error"].is_string(), "{method} {path}: errors carry a message: {v}");
    }

    // Change and work state rules.
    let ch = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "t", "message": "m", "files": { "c": { "content": "c" } } }))).await;
    let id = ch["id"].as_str().unwrap();
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "t", "status": "great" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "status": "success" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "x".repeat(101), "status": "success" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/preview"), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "no preview backend on this server");
    a.ok("POST", &format!("/v1/code/changes/{id}/close"), None).await;
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "message": "m", "files": { "c": { "content": "d" } } })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/land"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    let w = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "w" }))).await;
    let wid = w["id"].as_str().unwrap();
    let (s, _) = a.req("POST", &format!("/v1/code/work/{wid}/heartbeat"), Some(json!({ "agent": "a" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "an unclaimed item has no lease to renew");
    let (s, _) = a.req("PATCH", &format!("/v1/code/work/{wid}"), Some(json!({ "state": "claimed" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("PATCH", &format!("/v1/code/work/{wid}"), Some(json!({ "state": "nope" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("PATCH", &format!("/v1/code/work/{wid}"), Some(json!({ "title": "  " })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = a.req("PATCH", &format!("/v1/code/work/{wid}"), Some(json!({ "priority": -1 })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn writes_binary_content_deletes_and_blob_caching() {
    let a = app(None);
    a.repo("acme", "x").await;
    let p = "/v1/code/repos/acme/x";
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 255, 0];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let v = a
        .ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "author": { "name": "Ada", "email": "ada@x.dev" }, "files": {
            "logo.png": { "content_b64": b64 },
            "bin/run": { "content": "#!/bin/sh\n", "mode": "100755" },
            "link": { "content": "bin/run", "mode": "120000" },
            "old.txt": { "content": "old" },
        } })))
        .await;
    let first = v["changeset"].as_str().unwrap().to_string();

    // Files past axum's 2 MB default body limit go through (the API takes up to 32 MiB a file).
    let big: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
    let (s, v) = a
        .req("POST", &format!("{p}/changesets"), Some(json!({ "base": first, "message": "big", "files": { "big.bin": { "content_b64": base64::engine::general_purpose::STANDARD.encode(&big) } } })), None)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // A changeset written without landing: the ref doesn't move until it lands.
    let (s, cs) = a
        .req("POST", &format!("{p}/changesets"), Some(json!({ "base": first, "message": "rm old", "files": { "old.txt": { "delete": true }, "new.txt": { "content": "new" } } })), None)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{cs}");
    let cs_id = cs["id"].as_str().unwrap().to_string();
    assert_eq!(cs["parents"], json!([first]));
    assert_eq!(cs["author"]["name"], "roll", "no author given: roll");
    let changes: Vec<(&str, &str)> = cs["files"].as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["change"].as_str().unwrap())).collect();
    assert_eq!(changes, [("new.txt", "added"), ("old.txt", "deleted")]);
    assert_eq!(a.ok("GET", p, None).await["head"], first);
    let t = a.ok("GET", &format!("{p}/tree/{cs_id}"), None).await;
    assert!(t["entries"].as_array().unwrap().iter().any(|e| e["name"] == "new.txt"));
    let v = a.ok("POST", &format!("{p}/land"), Some(json!({ "changeset": cs_id }))).await;
    assert_eq!(v["status"], "landed");
    assert_eq!(v["fast_forward"], true);
    assert_eq!(a.ok("GET", p, None).await["head"], cs_id);

    // Raw bytes, content types, kinds and modes.
    let r = a.send(Request::get(format!("{p}/blob/main/logo.png")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.headers()["cache-control"], "no-cache", "a branch moves");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(etag.len(), 40);
    assert_eq!(axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec(), png);
    let r = a.send(Request::get(format!("{p}/blob/{cs_id}/logo.png")).body(Body::empty()).unwrap()).await;
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable", "a full changeset id never moves");
    assert_eq!(r.headers()["etag"].to_str().unwrap(), etag);
    let tree = a.ok("GET", &format!("{p}/tree/main/"), None).await;
    let kinds: Vec<(&str, &str, &str)> =
        tree["entries"].as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap(), e["kind"].as_str().unwrap(), e["mode"].as_str().unwrap())).collect();
    assert_eq!(kinds, [("bin", "dir", "040000"), ("link", "symlink", "120000"), ("logo.png", "file", "100644"), ("new.txt", "file", "100644")]);
    let png_entry = tree["entries"].as_array().unwrap().iter().find(|e| e["name"] == "logo.png").unwrap();
    assert_eq!(png_entry["size"], png.len());
    let bin = a.ok("GET", &format!("{p}/tree/main/bin"), None).await;
    assert_eq!(bin["entries"][0]["mode"], "100755");
    let (s, _) = a.req("GET", &format!("{p}/blob/main/old.txt"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let diff = a.ok("GET", &format!("{p}/changesets/{first}"), None).await;
    let logo = diff["files"].as_array().unwrap().iter().find(|f| f["path"] == "logo.png").unwrap();
    assert_eq!(logo["binary"], true);
    assert_eq!(logo["patch"], Value::Null);
    assert_eq!(diff["author"]["name"], "Ada");
    assert_eq!(diff["author"]["email"], "ada@x.dev");
    assert!(diff["committer"]["time"].is_i64());
    assert_eq!(diff["git_commit"].as_str().unwrap().len(), 40);
    // Any unique prefix and the git commit id resolve to the same changeset.
    let git = diff["git_commit"].as_str().unwrap();
    assert_eq!(a.ok("GET", &format!("{p}/changesets/{git}"), None).await["id"], first);
    assert_eq!(a.ok("GET", &format!("{p}/changesets/{}", &first[3..13]), None).await["id"], first);
}

#[tokio::test]
async fn paths_are_capped() {
    let a = app_with(|c| c.max_paths = 3);
    a.repo("acme", "big").await;
    a.repo("acme", "fits").await;
    let files = |n: usize| (0..n).map(|i| (format!("d{}/f{i}.txt", i % 2), json!({ "content": "x" }))).collect::<serde_json::Map<_, _>>();
    a.ok("POST", "/v1/code/repos/acme/big/land", Some(json!({ "message": "m", "files": files(5) }))).await;
    a.ok("POST", "/v1/code/repos/acme/fits/land", Some(json!({ "message": "m", "files": files(3) }))).await;
    let v = a.ok("GET", "/v1/code/repos/acme/big/paths/main", None).await;
    assert_eq!(v["paths"].as_array().unwrap().len(), 3);
    assert_eq!(v["truncated"], true);
    assert!(v["changeset"].as_str().unwrap().starts_with("cs_"));
    let v = a.ok("GET", "/v1/code/repos/acme/fits/paths/main", None).await;
    assert_eq!(v["paths"], json!(["d0/f0.txt", "d0/f2.txt", "d1/f1.txt"]));
    assert_eq!(v["truncated"], false, "exactly the cap isn't truncated");
    let (s, _) = a.req("GET", "/v1/code/repos/acme/fits/paths/nope", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stats_languages_and_contributors() {
    let a = app(None);
    a.repo("acme", "x").await;
    let p = "/v1/code/repos/acme/x";
    let empty = a.ok("GET", p, None).await;
    assert_eq!(empty["stats"]["merges_today"], 0);
    assert_eq!(empty["stats"]["merges_daily"], json!(vec![0; 30]));
    assert_eq!(empty["languages"], json!({}));
    assert_eq!(empty["readme"], Value::Null);
    assert_eq!(empty["generation"], 0);

    let lands = [
        ("Ada", "ada@x.dev", "src/a.rs", "fn a() {}\n"),
        ("Bob", "bob@x.dev", "web/b.ts", "export {}\n"),
        ("Ada Lovelace", "ADA@x.dev", "src/c.rs", "fn c() {}\n"),
        ("Cy", "cy@x.dev", "vendor/v.rs", "fn vendored() {}\n"),
        ("Cy", "cy@x.dev", "Cargo.lock", "# lock\n"),
    ];
    for (name, email, path, content) in lands {
        a.ok("POST", &format!("{p}/land"), Some(json!({ "message": path, "author": { "name": name, "email": email }, "files": { path: { "content": content } } }))).await;
    }
    let r = a.ok("GET", p, None).await;
    let daily = r["stats"]["merges_daily"].as_array().unwrap();
    assert_eq!(daily.len(), 30);
    assert_eq!(daily[29], 5, "today is last");
    assert!(daily[..29].iter().all(|d| d == 0));
    assert_eq!(r["stats"]["merges_today"], 5);
    assert_eq!(r["stats"]["contributors"], 3, "emails count once, case-insensitively");
    assert_eq!(r["languages"], json!({ "Rust": 20, "TypeScript": 10 }), "vendored and lock files don't count");
    assert_eq!(r["stats"]["open_changes"], 0);
    assert_eq!(r["stats"]["open_work"], 0);
    assert!(r["stats"]["merges_per_s"].as_f64().unwrap() >= 0.0);
    assert_eq!(r["stats"]["queue_depth"], 0);
    a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "w" }))).await;
    a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "z": { "content": "z" } } }))).await;
    let r = a.ok("GET", p, None).await;
    assert_eq!(r["stats"]["open_work"], 1);
    assert_eq!(r["stats"]["open_changes"], 1);
    assert_eq!(r["generation"], 4);
}

/// Reads SSE events off a response body. Events of other kinds wait their turn, so each kind
/// comes out in the order it was sent.
struct Events {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Send>>,
    buf: String,
    pending: Vec<(String, Value)>,
}

impl Events {
    async fn open(a: &App, path: &str) -> Events {
        let r = a.send(Request::get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        Events { body: Box::pin(r.into_body().into_data_stream()), buf: String::new(), pending: Vec::new() }
    }

    /// The next event of `kind`.
    async fn next(&mut self, kind: &str) -> Value {
        loop {
            if let Some(i) = self.pending.iter().position(|(k, _)| k == kind) {
                return self.pending.remove(i).1;
            }
            while let Some(end) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..end + 2).collect();
                let mut ev = String::new();
                let mut data = String::new();
                for line in block.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        ev = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                if !ev.is_empty() {
                    self.pending.push((ev, serde_json::from_str(&data).unwrap()));
                }
            }
            if self.pending.iter().any(|(k, _)| k == kind) {
                continue;
            }
            let chunk = tokio::time::timeout(Duration::from_secs(10), self.body.next())
                .await
                .unwrap_or_else(|_| panic!("no {kind} event in time; have {:?}", self.pending))
                .unwrap()
                .unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

/// A preview backend that hands out a URL and records deletes.
#[derive(Default)]
struct FakePreviews {
    deleted: parking_lot::Mutex<Vec<String>>,
    dirs: parking_lot::Mutex<Vec<bool>>,
    paths: parking_lot::Mutex<Vec<std::path::PathBuf>>,
}

#[async_trait]
impl PreviewBackend for FakePreviews {
    async fn create(&self, _repo: &RepoRow, change: &Change, dir: &Path) -> anyhow::Result<Preview> {
        self.dirs.lock().push(dir.join("c.txt").exists());
        self.paths.lock().push(dir.to_path_buf());
        Ok(Preview { state: PreviewState::Sleeping, url: Some(format!("http://{}.preview.test", change.id)), postgres: None, redis: None, error: None, created_at: 1 })
    }
    async fn delete(&self, _repo: &RepoRow, change: &Change) -> anyhow::Result<()> {
        self.deleted.lock().push(change.id.clone());
        Ok(())
    }
}

/// Previews read their tree from the repo's pooled work trees (moved by diff between changes),
/// not a full copy per change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn previews_use_pooled_work_trees() {
    let a = app(None);
    let previews = Arc::new(FakePreviews::default());
    a.code.set_preview_backend(previews.clone());
    a.repo("acme", "pv").await;
    let p = "/v1/code/repos/acme/pv";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a.txt": { "content": "a" } } }))).await;
    let mut ids = Vec::new();
    for n in 0..2 {
        let ch = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "c.txt": { "content": format!("{n}") } } }))).await;
        ids.push(ch["id"].as_str().unwrap().to_string());
    }
    let before = a.code.checkout_counts();
    for id in &ids {
        a.ok("POST", &format!("/v1/code/changes/{id}/preview"), None).await;
    }
    let paths = previews.paths.lock().clone();
    assert_eq!(paths.len(), 2);
    let pool = a.dir.path().join("code").join("worktrees");
    assert!(paths.iter().all(|d| d.starts_with(&pool)), "{paths:?}");
    assert_eq!(paths[0], paths[1], "the same tree, moved to the second change");
    assert!(previews.dirs.lock().iter().all(|x| *x), "each saw its change's files");
    let after = a.code.checkout_counts();
    assert!(after.0 > before.0, "the second preview moved the tree by diff: {before:?} -> {after:?}");
    assert!(!a.dir.path().join("code").join("previews").exists(), "no per-change copies");
}

/// Deleting a repo removes its changes' previews (containers and data branches), which nothing
/// else would ever stop.
#[tokio::test]
async fn deleting_a_repo_removes_its_previews() {
    let a = app(None);
    let previews = Arc::new(FakePreviews::default());
    a.code.set_preview_backend(previews.clone());
    a.repo("acme", "gone").await;
    let p = "/v1/code/repos/acme/gone";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a.txt": { "content": "a" } } }))).await;
    let mut ids = Vec::new();
    for n in 0..2 {
        let ch = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { format!("c{n}.txt"): { "content": "c" } } }))).await;
        ids.push(ch["id"].as_str().unwrap().to_string());
    }
    a.ok("POST", &format!("/v1/code/changes/{}/preview", ids[0]), None).await;
    let (s, _) = a.req("DELETE", p, None, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(previews.deleted.lock().as_slice(), std::slice::from_ref(&ids[0]), "only the change with a preview");
}

#[tokio::test]
async fn event_shapes_and_previews() {
    let a = app(None);
    let previews = Arc::new(FakePreviews::default());
    a.code.set_preview_backend(previews.clone());
    a.repo("acme", "live").await;
    let p = "/v1/code/repos/acme/live";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a.txt": { "content": "a" } } }))).await;
    let mut ev = Events::open(&a, &format!("{p}/events")).await;

    let w = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "next", "labels": ["ui"], "priority": 1 }))).await;
    let got = ev.next("work").await;
    assert_eq!(got, w, "work events carry the WorkItem");
    for k in ["id", "repo_id", "repo", "number", "title", "body", "labels", "priority", "state", "source", "github_url", "claimed_by", "lease_until", "changes", "created_at", "updated_at"] {
        assert!(got.get(k).is_some(), "WorkItem.{k}");
    }

    let ch = a
        .ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "work_item": w["id"], "message": "c", "files": { "c.txt": { "content": "c" } } })))
        .await;
    let id = ch["id"].as_str().unwrap().to_string();
    let got = ev.next("change").await;
    assert_eq!(got["id"], id);
    for k in ["id", "repo_id", "repo", "number", "title", "body", "author", "state", "base", "head", "revision", "work_item", "github_pr", "github_url", "trusted", "checks", "preview", "landed_as", "conflict", "created_at", "updated_at"] {
        assert!(got.get(k).is_some(), "Change.{k}");
    }
    assert_eq!(ev.next("work").await["state"], "review", "the work item moved to review");

    a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "failure", "url": "https://ci/1", "summary": "1 failed" }))).await;
    let got = ev.next("check").await;
    assert_eq!(got["change"], id);
    assert_eq!(got["check"]["name"], "test");
    assert_eq!(got["check"]["status"], "failure");
    assert_eq!(got["check"]["source"], "external");
    assert_eq!(got["check"]["url"], "https://ci/1");
    assert_eq!(got["check"]["summary"], "1 failed");
    assert!(got["check"]["started_at"].is_i64() && got["check"]["finished_at"].is_i64());
    assert_eq!(ev.next("change").await["checks"][0]["status"], "failure");

    let c = a.ok("POST", &format!("/v1/code/changes/{id}/preview"), None).await;
    assert_eq!(c["preview"]["state"], "sleeping");
    assert_eq!(c["preview"]["url"], format!("http://{id}.preview.test"));
    assert_eq!(previews.dirs.lock().as_slice(), [true], "the backend gets the change's tree materialized");
    let got = ev.next("preview").await;
    assert_eq!(got["change"], id);
    assert_eq!(got["preview"]["state"], "sleeping");
    for k in ["state", "url", "postgres", "redis", "error", "created_at"] {
        assert!(got["preview"].get(k).is_some(), "Preview.{k}");
    }
    let c = a.ok("DELETE", &format!("/v1/code/changes/{id}/preview"), None).await;
    assert_eq!(c["preview"], Value::Null);
    assert_eq!(previews.deleted.lock().as_slice(), std::slice::from_ref(&id));
    assert_eq!(ev.next("preview").await, json!({ "change": id, "preview": null }));

    let landed = a.ok("POST", &format!("/v1/code/changes/{id}/land"), None).await;
    let got = ev.next("landed").await;
    assert_eq!(got["changeset"], landed["changeset"]);
    assert_eq!(got["change"], id, "the landing names the change it came from");
    assert_eq!(got["git_commit"], landed["git_commit"]);
    assert_eq!(got["ref"], "main");
    assert_eq!(got["message"], "c");
    assert_eq!(got["files"], 1);
    assert_eq!(got["count"], 1);
    assert!(got["at"].as_i64().unwrap() > 1_600_000_000, "seconds");
    assert!(got["author"].is_string());
    assert_eq!(ev.next("work").await["state"], "done");

    // Another repo's events don't leak into this feed.
    a.repo("acme", "other").await;
    a.ok("POST", "/v1/code/repos/acme/other/work", Some(json!({ "title": "elsewhere" }))).await;
    a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "here" }))).await;
    assert_eq!(ev.next("work").await["title"], "here");

    // A private repo's feed is invisible without its token.
    let b = app(Some(ADMIN));
    two_owners(&b).await;
    let r = b.send(Request::get("/v1/code/repos/acme/priv/events").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn change_authors() {
    let a = app(Some(ADMIN));
    let (acme, _) = two_owners(&a).await;
    let p = "/v1/code/repos/acme/pub";
    let files = json!({ "x.txt": { "content": "x" } });
    let by = |author: Option<Value>| {
        let mut b = json!({ "title": "t", "message": "m", "files": files });
        if let Some(au) = author {
            b["author"] = au;
        }
        b
    };
    let (_, c) = a.req("POST", &format!("{p}/changes"), Some(by(None)), Some(ADMIN)).await;
    assert_eq!(c["author"], "roll", "an admin write without an author is roll's, not \"admin\"");
    let (_, c) = a.req("POST", &format!("{p}/changes"), Some(by(None)), Some(&acme)).await;
    assert_eq!(c["author"], "acme", "an owner token writes as the owner");
    let (_, c) = a.req("POST", &format!("{p}/changes"), Some(by(Some(json!("Grace")))), Some(ADMIN)).await;
    assert_eq!(c["author"], "Grace");
    let (_, c) = a.req("POST", &format!("{p}/changes"), Some(by(Some(json!({ "name": "Ada", "email": "ada@x.dev" })))), Some(&acme)).await;
    assert_eq!(c["author"], "Ada");
    let cs = c["head"].as_str().unwrap();
    let info = a.req("GET", &format!("{p}/changesets/{cs}"), None, Some(&acme)).await.1;
    assert_eq!(info["author"]["email"], "ada@x.dev");
    // A change from an existing changeset is by that changeset's author.
    let (_, c2) = a.req("POST", &format!("{p}/changes"), Some(json!({ "title": "again", "changeset": cs })), Some(ADMIN)).await;
    assert_eq!(c2["author"], "Ada");
}

#[tokio::test]
async fn lookups_by_number_and_id() {
    let a = app(None);
    a.repo("acme", "x").await;
    a.repo("acme", "y").await;
    let p = "/v1/code/repos/acme/x";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a": { "content": "a" } } }))).await;
    let w = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "w" }))).await;
    let c = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "b": { "content": "b" } } }))).await;
    assert_eq!((w["number"].as_u64(), c["number"].as_u64()), (Some(1), Some(2)), "one counter per repo");
    let (wid, cid) = (w["id"].as_str().unwrap(), c["id"].as_str().unwrap());
    assert_eq!(a.ok("GET", &format!("{p}/changes/2"), None).await["id"], cid);
    assert_eq!(a.ok("GET", &format!("{p}/changes/%232"), None).await["id"], cid);
    assert_eq!(a.ok("GET", &format!("{p}/changes/{cid}"), None).await["id"], cid);
    assert_eq!(a.ok("GET", &format!("{p}/work/1"), None).await["id"], wid);
    assert_eq!(a.ok("GET", &format!("{p}/work/{wid}"), None).await["id"], wid);
    // Numbers belong to their repo: another repo's ids and numbers don't resolve here.
    assert_eq!(a.req("GET", &format!("/v1/code/repos/acme/y/changes/{cid}"), None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.req("GET", &format!("/v1/code/repos/acme/y/work/{wid}"), None, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.req("GET", "/v1/code/repos/acme/y/changes/2", None, None).await.0, StatusCode::NOT_FOUND);
    // A change number isn't a work item number.
    assert_eq!(a.req("GET", &format!("{p}/work/2"), None, None).await.0, StatusCode::NOT_FOUND);

    // Release hands the item back; release by another agent is refused.
    let claimed = a.ok("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "a1" }))).await;
    assert_eq!(claimed["id"], wid);
    assert!(claimed["lease_until"].as_i64().unwrap() > 0);
    let (s, _) = a.req("POST", &format!("/v1/code/work/{wid}/release"), Some(json!({ "agent": "a2" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = a.req("POST", &format!("/v1/code/work/{wid}/complete"), Some(json!({ "agent": "a2" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let r = a.ok("POST", &format!("/v1/code/work/{wid}/release"), Some(json!({ "agent": "a1" }))).await;
    assert_eq!(r["state"], "open");
    assert_eq!(r["claimed_by"], Value::Null);
    assert_eq!(r["lease_until"], Value::Null);
    // Labels: trimmed, de-duplicated case-insensitively.
    let l = a.ok("PATCH", &format!("/v1/code/work/{wid}"), Some(json!({ "labels": [" ui ", "UI", "api", ""], "priority": 0, "body": "details" }))).await;
    assert_eq!(l["labels"], json!(["ui", "api"]));
    assert_eq!(l["priority"], 0);
    assert_eq!(l["body"], "details");
    let list = a.ok("GET", &format!("{p}/work?state=open"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let list = a.ok("GET", &format!("{p}/changes?state=open"), None).await;
    assert_eq!(list[0]["repo"], "acme/x");
}

#[tokio::test]
async fn state_survives_a_restart() {
    let (cid, wid, head);
    let dir = {
        let a = app(None);
        a.repo("acme", "x").await;
        let p = "/v1/code/repos/acme/x";
        head = a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "a": { "content": "a" } } }))).await["changeset"].clone();
        wid = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "w" }))).await["id"].clone();
        let c = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "c", "message": "c", "files": { "b": { "content": "b" } } }))).await;
        cid = c["id"].clone();
        a.ok("POST", &format!("/v1/code/changes/{}/checks", cid.as_str().unwrap()), Some(json!({ "name": "t", "status": "success" }))).await;
        a.ok("PATCH", p, Some(json!({ "name": "renamed", "description": "kept" }))).await;
        let App { code, router, dir } = a;
        drop((code, router));
        dir
    };
    let code = open_in(dir.path(), |_| {});
    let a = App { router: code_control::api::router(code.clone()), code, dir };
    let r = a.ok("GET", "/v1/code/repos/acme/renamed", None).await;
    assert_eq!(r["head"], head);
    assert_eq!(r["description"], "kept");
    let r = a.send(Request::get("/v1/code/repos/acme/x/tree/main").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY, "redirects persist");
    assert_eq!(r.headers()["location"], "/v1/code/repos/acme/renamed");
    let c = a.ok("GET", &format!("/v1/code/changes/{}", cid.as_str().unwrap()), None).await;
    assert_eq!(c["checks"][0]["status"], "success");
    assert_eq!(a.ok("GET", &format!("/v1/code/work/{}", wid.as_str().unwrap()), None).await["title"], "w");
    let raw = a.raw("GET", "/v1/code/repos/acme/renamed/blob/main/a", None, None).await;
    assert_eq!(raw.1, b"a");
    // The counter continues.
    let w = a.ok("POST", "/v1/code/repos/acme/renamed/work", Some(json!({ "title": "w2" }))).await;
    assert_eq!(w["number"], 3);
}
