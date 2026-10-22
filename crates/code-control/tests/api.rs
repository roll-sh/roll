//! The /v1/code API: owners and names, repos, landing, changes, work items, READMEs, events.

mod common;

use axum::http::StatusCode;
use common::app;
use futures::StreamExt;
use serde_json::json;

#[tokio::test]
async fn names_and_owners() {
    let a = app(None);
    let v = a.ok("GET", "/v1/code/names/Acme", None).await;
    assert_eq!(v["available"], true);
    let o = a.ok("POST", "/v1/code/owners", Some(json!({ "name": "Acme", "kind": "org", "display_name": "Acme Inc" }))).await;
    assert_eq!(o["name"], "acme");
    assert_eq!(o["kind"], "org");
    assert_eq!(o["display_name"], "Acme Inc");

    let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "ACME" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "dashboard" })), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "a--b" })), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    for (name, reason) in [("acme", "taken"), ("ACME", "taken"), ("settings", "reserved"), ("-x", "invalid"), ("x_y", "invalid")] {
        let v = a.ok("GET", &format!("/v1/code/names/{name}"), None).await;
        assert_eq!(v["available"], false, "{name}");
        assert_eq!(v["reason"], reason, "{name}");
        assert!(v["message"].is_string());
    }

    let v = a.ok("PATCH", "/v1/code/owners/acme", Some(json!({ "bio": "We build things", "url": "https://acme.dev" }))).await;
    assert_eq!(v["bio"], "We build things");
    let v = a.ok("GET", "/v1/code/owners/ACME", None).await;
    assert_eq!(v["repos"], json!([]));
    assert_eq!(v["repos_count"], 0);
}

#[tokio::test]
async fn repos_rename_and_visibility() {
    let a = app(Some("admin-secret"));
    let admin = Some("admin-secret");
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme" })), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), admin).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, t) = a.req("POST", "/v1/code/owners/acme/tokens", Some(json!({ "name": "ci" })), admin).await;
    let token = t["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("rlc_"));
    let tok = Some(token.as_str());

    let (s, r) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "Web", "description": "The site" })), tok).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["full_name"], "acme/Web");
    assert_eq!(r["visibility"], "public");
    assert_eq!(r["default_branch"], "main");
    assert_eq!(r["head"], serde_json::Value::Null);
    assert_eq!(r["clone_url"], "http://127.0.0.1:7070/git/acme/Web.git");
    assert_eq!(r["web_url"], "https://roll.sh/acme/Web");
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "web" })), tok).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "x.git" })), tok).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "anon" })), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "bad" })), Some("rlc_nope")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    for (name, available, reason) in [("web", false, "taken"), ("WEB", false, "taken"), ("x.git", false, "invalid"), ("api", true, "")] {
        let v = a.ok("GET", &format!("/v1/code/names/acme/{name}"), None).await;
        assert_eq!(v["available"], available, "{name}");
        if !reason.is_empty() {
            assert_eq!(v["reason"], reason, "{name}");
        }
    }
    let (s, _) = a.req("GET", "/v1/code/names/nobody/x", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Any case reads it; anonymous readers see public repos.
    let (s, v) = a.req("GET", "/v1/code/repos/ACME/web", None, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "Web");

    // Rename keeps a redirect.
    let (s, v) = a.req("PATCH", "/v1/code/repos/acme/web", Some(json!({ "name": "site" })), tok).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["full_name"], "acme/site");
    let (s, v) = a.req("GET", "/v1/code/repos/acme/Web", None, None).await;
    assert_eq!(s, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(v["redirect"], "acme/site");
    // A new repo can take the old name back; the redirect goes away.
    let (s, _) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": "web" })), tok).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/web", None, None).await;
    assert_eq!(s, StatusCode::OK);

    // Private: invisible without the owner's token.
    let (s, _) = a.req("PATCH", "/v1/code/repos/acme/site", Some(json!({ "visibility": "private" })), tok).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/site", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/site", None, tok).await;
    assert_eq!(s, StatusCode::OK);
    let (_, list) = a.req("GET", "/v1/code/repos?owner=acme", None, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (_, list) = a.req("GET", "/v1/code/repos?owner=acme", None, admin).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    let (_, o) = a.req("GET", "/v1/code/owners/acme", None, None).await;
    assert_eq!(o["repos_count"], 1);
    // Renaming a private repo doesn't tell outsiders its new name.
    let (s, _) = a.req("PATCH", "/v1/code/repos/acme/site", Some(json!({ "name": "secret-plans" })), tok).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = a.req("GET", "/v1/code/repos/acme/site", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(!v.to_string().contains("secret-plans"), "{v}");
    let (s, v) = a.req("GET", "/v1/code/repos/acme/site/tree/main", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = a.req("GET", "/v1/code/repos/acme/site", None, tok).await;
    assert_eq!((s, v["redirect"].clone()), (StatusCode::MOVED_PERMANENTLY, json!("acme/secret-plans")));

    // Writes need a token.
    let (s, _) = a.req("POST", "/v1/code/repos/acme/web/land", Some(json!({ "message": "x", "files": { "a": { "content": "a" } } })), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = a.req("DELETE", "/v1/code/repos/acme/web", None, tok).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = a.req("GET", "/v1/code/repos/acme/web", None, tok).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn land_merge_and_conflict() {
    let a = app(None);
    a.repo("acme", "api").await;
    let p = "/v1/code/repos/acme/api";
    let text = (0..20).map(|i| format!("line {i}\n")).collect::<String>();

    let v = a
        .ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": {
            "README.md": { "content": "# api\n" },
            "src/main.rs": { "content": "fn main() {}\n" },
            "notes.txt": { "content": text },
            "run.sh": { "content": "#!/bin/sh\n", "mode": "100755" },
        } })))
        .await;
    assert_eq!(v["status"], "landed");
    assert_eq!(v["ref"], "main");
    let base = v["changeset"].as_str().unwrap().to_string();
    assert!(base.starts_with("cs_") && base.len() == 67, "{base}");
    assert_eq!(v["git_commit"].as_str().unwrap().len(), 40);

    // Two agents on the same base: different files, then the same file on different lines.
    let v = a.ok("POST", &format!("{p}/land"), Some(json!({ "base": base, "message": "a", "files": { "a.txt": { "content": "a\n" } } }))).await;
    assert_eq!(v["status"], "landed");
    let edited = text.replace("line 1\n", "line one\n");
    let v = a.ok("POST", &format!("{p}/land"), Some(json!({ "base": base, "message": "b", "files": { "notes.txt": { "content": edited } } }))).await;
    assert_eq!(v["status"], "landed");
    let edited = text.replace("line 18\n", "line eighteen\n");
    let v = a.ok("POST", &format!("{p}/land"), Some(json!({ "base": base, "message": "c", "files": { "notes.txt": { "content": edited } } }))).await;
    assert_eq!(v["status"], "landed", "{v}");
    assert_eq!(v["merged_paths"], json!(["notes.txt"]));

    // Same line from the old base: a conflict, with the paths and the head to rebase onto.
    let edited = text.replace("line 1\n", "line uno\n");
    let (s, v) = a.req("POST", &format!("{p}/land"), Some(json!({ "base": base, "message": "d", "files": { "notes.txt": { "content": edited } } })), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["status"], "conflict");
    assert_eq!(v["paths"], json!(["notes.txt"]));
    assert_eq!(v["ref"], "main");
    assert!(v["head"].as_str().unwrap().starts_with("cs_"));
    assert!(v["error"].as_str().unwrap().contains("notes.txt"));

    // Reads.
    let raw = a.raw("GET", &format!("{p}/blob/main/notes.txt"), None, None).await;
    let s = String::from_utf8(raw.1).unwrap();
    assert!(s.contains("line one\n") && s.contains("line eighteen\n"));
    let tree = a.ok("GET", &format!("{p}/tree/main"), None).await;
    let names: Vec<&str> = tree["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["src", "README.md", "a.txt", "notes.txt", "run.sh"]);
    let notes = tree["entries"].as_array().unwrap().iter().find(|e| e["name"] == "notes.txt").unwrap();
    assert_eq!(notes["last_change"]["message"], "c");
    assert_eq!(notes["kind"], "file");
    let run = tree["entries"].as_array().unwrap().iter().find(|e| e["name"] == "run.sh").unwrap();
    assert_eq!(run["mode"], "100755");
    let src = a.ok("GET", &format!("{p}/tree/main/src"), None).await;
    assert_eq!(src["entries"][0]["path"], "src/main.rs");
    let (s, _) = a.req("GET", &format!("{p}/tree/main/nope"), None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let log = a.ok("GET", &format!("{p}/log?limit=2"), None).await;
    assert_eq!(log["items"].as_array().unwrap().len(), 2);
    assert_eq!(log["items"][0]["message"], "c");
    let cursor = log["cursor"].as_str().unwrap();
    let more = a.ok("GET", &format!("{p}/log?limit=10&cursor={cursor}"), None).await;
    assert_eq!(more["items"].as_array().unwrap().len(), 2);
    assert_eq!(more["cursor"], serde_json::Value::Null);
    let hist = a.ok("GET", &format!("{p}/log?path=notes.txt"), None).await;
    assert_eq!(hist["items"].as_array().unwrap().len(), 3);

    let head = a.ok("GET", p, None).await;
    let head_id = head["head"].as_str().unwrap().to_string();
    let cs = a.ok("GET", &format!("{p}/changesets/{}", &head_id[..15]), None).await;
    assert_eq!(cs["id"], head_id);
    assert_eq!(cs["files"][0]["path"], "notes.txt");
    assert_eq!(cs["files"][0]["change"], "modified");
    assert_eq!(cs["files"][0]["additions"], 1);
    assert_eq!(cs["files"][0]["deletions"], 1);
    assert!(cs["files"][0]["patch"].as_str().unwrap().contains("+line eighteen"));
    let cmp = a.ok("GET", &format!("{p}/compare/{base}...main"), None).await;
    assert_eq!(cmp["changesets"].as_array().unwrap().len(), 3);
    assert_eq!(cmp["files"].as_array().unwrap().len(), 2);

    let refs = a.ok("GET", &format!("{p}/refs"), None).await;
    assert_eq!(refs[0]["name"], "main");
    assert_eq!(refs[0]["changeset"], head_id);
    let paths = a.ok("GET", &format!("{p}/paths/main"), None).await;
    assert_eq!(paths["paths"], json!(["README.md", "a.txt", "notes.txt", "run.sh", "src/main.rs"]));

    // Stats and languages.
    assert_eq!(head["stats"]["merges_today"], 4);
    assert!(head["stats"]["changesets"].as_u64().unwrap() >= 4);
    assert_eq!(head["stats"]["merges_daily"].as_array().unwrap().len(), 30);
    assert!(head["languages"]["Rust"].as_u64().unwrap() > 0);
    assert_eq!(head["readme"], "README.md");
    assert_eq!(head["generation"], 3, "the root is generation 0");
    let info = a.ok("GET", "/v1/code", None).await;
    assert_eq!(info["repos"], 1);
    assert_eq!(info["stats"]["merges_today"], 4);

    // Writes on a branch.
    let v = a.ok("POST", &format!("{p}/land"), Some(json!({ "ref": "release", "base": base, "message": "r", "files": { "r.txt": { "content": "r" } } }))).await;
    assert_eq!(v["ref"], "release");
    let raw = a.raw("GET", &format!("{p}/blob/release/r.txt"), None, None).await;
    assert_eq!(raw.1, b"r");
}

#[tokio::test]
async fn changes_and_work() {
    let a = app(None);
    a.repo("acme", "api").await;
    let p = "/v1/code/repos/acme/api";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "init", "files": { "x.txt": { "content": "x\n" } } }))).await;

    let w1 = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "Fix login", "labels": ["auth", "bug"], "priority": 1 }))).await;
    let w0 = a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "Outage", "priority": 0 }))).await;
    assert_eq!(w1["number"], 1);
    assert_eq!(w0["number"], 2);
    assert_eq!(w1["state"], "open");
    assert_eq!(w1["source"], "native");
    let (s, _) = a.req("POST", &format!("{p}/work"), Some(json!({ "title": "x", "priority": 7 })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Highest priority first; labels filter.
    let c = a.ok("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "agent-1" }))).await;
    assert_eq!(c["id"], w0["id"]);
    assert_eq!(c["state"], "claimed");
    assert_eq!(c["claimed_by"], "agent-1");
    let (s, _) = a.req("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "agent-2", "labels": ["nope"] })), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let c2 = a.ok("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "agent-2", "labels": ["auth"], "lease_s": 1 }))).await;
    assert_eq!(c2["id"], w1["id"]);
    let (s, _) = a.req("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "agent-3" })), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // A lease that runs out goes back to open.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    let w = a.ok("GET", &format!("/v1/code/work/{}", w1["id"].as_str().unwrap()), None).await;
    assert_eq!(w["state"], "open");
    assert_eq!(w["claimed_by"], serde_json::Value::Null);
    let c3 = a.ok("POST", &format!("{p}/work/claim"), Some(json!({ "agent": "agent-3" }))).await;
    assert_eq!(c3["id"], w1["id"]);
    let wid = c3["id"].as_str().unwrap().to_string();
    let (s, _) = a.req("POST", &format!("/v1/code/work/{wid}/heartbeat"), Some(json!({ "agent": "agent-9" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let hb = a.ok("POST", &format!("/v1/code/work/{wid}/heartbeat"), Some(json!({ "agent": "agent-3", "lease_s": 3600 }))).await;
    assert!(hb["lease_until"].as_i64().unwrap() > c3["lease_until"].as_i64().unwrap_or(0));
    // A heartbeat without lease_s renews by the lease's length, not the default.
    let hb2 = a.ok("POST", &format!("/v1/code/work/{wid}/heartbeat"), Some(json!({ "agent": "agent-3" }))).await;
    assert!(hb2["lease_until"].as_i64().unwrap() >= hb["lease_until"].as_i64().unwrap());

    // A change for the work item: the item moves to review, landing finishes it.
    let ch = a
        .ok("POST", &format!("{p}/changes"), Some(json!({
            "title": "Fix login redirect", "body": "Closes the loop", "work_item": wid,
            "message": "fix login", "files": { "login.rs": { "content": "fn login() {}\n" } }
        })))
        .await;
    assert_eq!(ch["number"], 3);
    assert_eq!(ch["state"], "open");
    assert_eq!(ch["repo"], "acme/api");
    assert_eq!(ch["revision"], 1);
    assert_eq!(ch["work_item"], wid);
    let w = a.ok("GET", &format!("{p}/work/1"), None).await;
    assert_eq!(w["state"], "review");
    assert_eq!(w["changes"], json!([ch["id"]]));
    let id = ch["id"].as_str().unwrap().to_string();

    // Required checks gate queueing (`required_checks` is the queue's old name and maps to it).
    let r = a.ok("PATCH", p, Some(json!({ "land_policy": "required_checks", "required_checks": ["test"] }))).await;
    assert_eq!(r["land_policy"], "queue");
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{id}/land"), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["blockers"][0]["kind"], "checks", "{v}");
    assert!(v["blockers"][0]["message"].as_str().unwrap().contains("test (not run)"), "{v}");
    let v = a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "running" }))).await;
    assert_eq!(v["checks"][0]["status"], "running");
    let v = a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "success", "summary": "42 passed" }))).await;
    assert_eq!(v["checks"][0]["status"], "success");
    assert!(v["checks"][0]["finished_at"].is_i64());
    assert!(v["checks"][0]["started_at"].is_i64());

    // A revision resets checks.
    let first_head = v["head"].clone();
    let v = a.ok("POST", &format!("/v1/code/changes/{id}/revise"), Some(json!({ "message": "fix login v2", "files": { "login.rs": { "content": "fn login() { /* v2 */ }\n" } } }))).await;
    assert_eq!(v["revision"], 2);
    assert_eq!(v["checks"], json!([]));
    // CI finishing on the first revision can't pass the second.
    let (s, body) = a.req("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "success", "head": first_head })), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    let (s, body) = a.req("POST", &format!("/v1/code/changes/{id}/land"), None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "the stale success didn't count: {body}");
    let v2 = v["head"].as_str().unwrap();
    let short = &v2[..v2.len().min(15)];
    a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "running", "head": short }))).await;
    a.ok("POST", &format!("/v1/code/changes/{id}/checks"), Some(json!({ "name": "test", "status": "success" }))).await;

    // Landing queues it (202); the queue (no runner here: nothing to test) lands it.
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{id}/land"), None, None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["status"], "queued");
    assert_eq!(v["position"], 1);
    assert_eq!(v["change"]["state"], "queued");
    assert_eq!(v["change"]["queue"]["state"], "waiting");
    a.code.start_queues().unwrap();
    let c = a.wait_change(&id, "landed").await;
    assert!(c["landed_as"].as_str().unwrap().starts_with("cs_"));
    assert_eq!(c["queue"]["state"], "landed");
    assert_eq!(c["queue"]["position"], serde_json::Value::Null);
    let w = a.ok("GET", &format!("/v1/code/work/{wid}"), None).await;
    assert_eq!(w["state"], "done");
    let (s, _) = a.req("POST", &format!("/v1/code/changes/{id}/land"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let by_number = a.ok("GET", &format!("{p}/changes/3"), None).await;
    assert_eq!(by_number["id"], id);
    let (s, _) = a.req("GET", "/v1/code/changes/chg_missing", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // A conflicting change records the conflict.
    a.ok("PATCH", p, Some(json!({ "land_policy": "post_merge" }))).await;
    let x = a.ok("GET", &format!("{p}/log?limit=100"), None).await;
    let first = x["items"].as_array().unwrap().last().unwrap()["id"].as_str().unwrap().to_string();
    let ch = a.ok("POST", &format!("{p}/changes"), Some(json!({ "title": "old", "base": first, "message": "old", "files": { "login.rs": { "content": "other\n" } } }))).await;
    let cid = ch["id"].as_str().unwrap();
    let (s, v) = a.req("POST", &format!("/v1/code/changes/{cid}/land"), None, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let c = a.ok("GET", &format!("/v1/code/changes/{cid}"), None).await;
    assert_eq!(c["state"], "conflict");
    assert_eq!(c["conflict"]["paths"], json!(["login.rs"]));
    let c = a.ok("POST", &format!("/v1/code/changes/{cid}/close"), None).await;
    assert_eq!(c["state"], "closed");

    let open = a.ok("GET", &format!("{p}/changes?state=landed"), None).await;
    assert_eq!(open.as_array().unwrap().len(), 1);
    let list = a.ok("GET", &format!("{p}/work?label=auth"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let w = a.ok("PATCH", &format!("/v1/code/work/{}", w0["id"].as_str().unwrap()), Some(json!({ "state": "open", "labels": ["ops"] }))).await;
    assert_eq!(w["state"], "open");
    assert_eq!(w["claimed_by"], serde_json::Value::Null);
    assert_eq!(w["labels"], json!(["ops"]));
    let w = a.ok("POST", &format!("/v1/code/work/{}/complete", w0["id"].as_str().unwrap()), Some(json!({}))).await;
    assert_eq!(w["state"], "done");
}

#[tokio::test]
async fn readme_is_rendered_and_sanitized() {
    let a = app(None);
    a.repo("acme", "docs").await;
    let p = "/v1/code/repos/acme/docs";
    let md = "# Docs\n\n<script>alert('x')</script>\n\n<img src=\"x\" onerror=\"alert(1)\">\n\nSee [guide](guide/start.md) and ![shot](img/s.png).\n";
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "docs", "files": { "README.md": { "content": md }, "lib.ts": { "content": "export {}\n" } } }))).await;
    let v = a.ok("GET", &format!("{p}/readme/main"), None).await;
    assert_eq!(v["path"], "README.md");
    let html = v["html"].as_str().unwrap();
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("onerror"), "{html}");
    assert!(html.contains("<h1"), "{html}");
    assert!(html.contains("href=\"https://roll.sh/acme/docs/blob/main/guide/start.md\""), "{html}");
    assert!(html.contains("src=\"https://api.roll.sh/v1/code/repos/acme/docs/blob/main/img/s.png\""), "{html}");
    let (s, _) = a.req("GET", &format!("{p}/readme/main?path=lib.ts"), None, None).await;
    assert_eq!(s, StatusCode::OK);

    // Raw files are never served as HTML.
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "page", "files": { "x.html": { "content": "<script>1</script>" } } }))).await;
    let resp = a.router.clone();
    use tower::ServiceExt;
    let r = resp
        .oneshot(axum::http::Request::get(format!("{p}/blob/main/x.html")).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
    assert!(r.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
    assert!(r.headers()["x-roll-changeset"].to_str().unwrap().starts_with("cs_"));
}

#[tokio::test]
async fn events_stream_landings_and_changes() {
    let a = app(None);
    a.repo("acme", "live").await;
    let p = "/v1/code/repos/acme/live";
    use tower::ServiceExt;
    let resp = a
        .router
        .clone()
        .oneshot(axum::http::Request::get(format!("{p}/events")).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body().into_data_stream();
    a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "hello", "files": { "a": { "content": "a" } } }))).await;
    a.ok("POST", &format!("{p}/work"), Some(json!({ "title": "next" }))).await;
    let mut text = String::new();
    while !(text.contains("event: landed") && text.contains("event: work")) {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.next()).await.expect("event").unwrap().unwrap();
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let landed = text.split("event: landed\ndata: ").nth(1).unwrap().lines().next().unwrap();
    let v: serde_json::Value = serde_json::from_str(landed).unwrap();
    assert_eq!(v["message"], "hello");
    assert_eq!(v["ref"], "main");
    assert_eq!(v["files"], 1);
    assert!(v["changeset"].as_str().unwrap().starts_with("cs_"));
}
