//! `/v1/workspace` in process, on SQLite in a tempdir (docs/workspace.md).

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

struct Api {
    router: Router,
    _dir: tempfile::TempDir,
}

impl Api {
    fn new() -> Api {
        let dir = tempfile::tempdir().unwrap();
        let target = code_sql::Target::Sqlite(dir.path().join("workspace.db"));
        let db = code_sql::Db::open_or_test(&target, "workspace").unwrap();
        let ws = roll_workspace::Workspace::new(db).unwrap();
        Api { router: roll_workspace::router(Arc::new(ws)), _dir: dir }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_as(None, method, path, body).await
    }

    /// As a signed-in user (`usr_…`), as rolld's gate would mark the request; `None`: the
    /// operator.
    async fn call_as(&self, user: Option<&str>, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        let mut req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        if let Some(u) = user {
            let signed_in = roll_auth::SignedIn { user: u.into(), handle: u.into(), admin: false, code_owner: None, session: "ses_test".into() };
            req.extensions_mut().insert(signed_in);
        }
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    async fn create(&self, name: &str) -> Value {
        let (s, p) = self.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": name }))).await;
        assert_eq!(s, StatusCode::CREATED, "{p}");
        p
    }
}

fn names(list: &Value) -> Vec<&str> {
    list.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn projects_are_created_listed_renamed_and_deleted() {
    let api = Api::new();
    let a = api.create("billing").await;
    let id = a["id"].as_str().unwrap();
    assert!(id.len() == 16 && id.starts_with("prj_") && id[4..].bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "{id}");
    assert_eq!(a["resources"], json!([]));
    assert!(a["created_at"].as_i64().unwrap() > 1_700_000_000_000, "unix ms");
    api.create("search").await;

    let (s, list) = api.call(Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((s, names(&list)), (StatusCode::OK, vec!["billing", "search"]));

    let (s, p) = api.call(Method::PATCH, &format!("/v1/workspace/projects/{id}"), Some(json!({ "name": "payments" }))).await;
    assert_eq!((s, p["name"].as_str(), p["id"].as_str()), (StatusCode::OK, Some("payments"), Some(id)));
    let (s, p) = api.call(Method::GET, &format!("/v1/workspace/projects/{id}"), None).await;
    assert_eq!((s, p["name"].as_str()), (StatusCode::OK, Some("payments")));

    // Its look: a description and an icon, changed alone; `null` brings the default back.
    assert_eq!((a["description"].as_str(), &a["icon"]), (Some(""), &Value::Null));
    let icon = json!({ "kind": "emoji", "emoji": "🧾", "color": "teal" });
    let (s, p) = api.call(Method::PATCH, &format!("/v1/workspace/projects/{id}"), Some(json!({ "description": "Invoices", "icon": icon }))).await;
    assert_eq!((s, p["name"].as_str(), p["description"].as_str(), &p["icon"]), (StatusCode::OK, Some("payments"), Some("Invoices"), &icon));
    let (s, p) = api.call(Method::PATCH, &format!("/v1/workspace/projects/{id}"), Some(json!({ "icon": null }))).await;
    assert_eq!((s, p["description"].as_str(), &p["icon"]), (StatusCode::OK, Some("Invoices"), &Value::Null));
    for bad in [json!({ "kind": "image", "url": "data:image/svg+xml;base64,PHN2Zz4=" }), json!({ "kind": "monogram", "color": "orange" })] {
        let (s, _) = api.call(Method::PATCH, &format!("/v1/workspace/projects/{id}"), Some(json!({ "icon": bad }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }

    let (s, _) = api.call(Method::DELETE, &format!("/v1/workspace/projects/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, e) = api.call(Method::GET, &format!("/v1/workspace/projects/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(e["error"].is_string());
    let (_, list) = api.call(Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!(names(&list), vec!["search"]);
}

#[tokio::test]
async fn resources_are_added_moved_and_removed() {
    let api = Api::new();
    let a = api.create("web").await["id"].as_str().unwrap().to_string();
    let b = api.create("data").await["id"].as_str().unwrap().to_string();
    // A repo's ref is `owner/name`, one percent-encoded segment.
    let repo = |p: &str| format!("/v1/workspace/projects/{p}/resources/repo/acme%2Fsite");

    let (s, r) = api.call(Method::PUT, &repo(&a), Some(json!({ "x": 10, "y": -2.5 }))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!((r["kind"].as_str(), r["ref"].as_str(), r["x"].as_f64(), r["y"].as_f64()), (Some("repo"), Some("acme/site"), Some(10.0), Some(-2.5)));
    let added = r["added_at"].as_i64().unwrap();
    api.call(Method::PUT, &format!("/v1/workspace/projects/{a}/resources/bucket/assets"), Some(json!({ "x": 0, "y": 0 }))).await;

    // Moving on the canvas keeps when it was added.
    let (_, r) = api.call(Method::PUT, &repo(&a), Some(json!({ "x": 40, "y": 8 }))).await;
    assert_eq!((r["x"].as_f64(), r["added_at"].as_i64()), (Some(40.0), Some(added)));

    // Into another project, in one write: it leaves the first.
    let (s, _) = api.call(Method::PUT, &repo(&b), Some(json!({ "x": 1, "y": 2 }))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, pa) = api.call(Method::GET, &format!("/v1/workspace/projects/{a}"), None).await;
    let (_, pb) = api.call(Method::GET, &format!("/v1/workspace/projects/{b}"), None).await;
    assert_eq!(pa["resources"].as_array().unwrap().iter().map(|r| r["ref"].as_str().unwrap()).collect::<Vec<_>>(), vec!["assets"]);
    assert_eq!(pb["resources"][0]["ref"], "acme/site");
    assert_eq!((pb["resources"][0]["x"].as_f64(), pb["resources"][0]["y"].as_f64()), (Some(1.0), Some(2.0)));

    // Removing it from a project it isn't in is a 404; from its own, a 204.
    let (s, _) = api.call(Method::DELETE, &repo(&a), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = api.call(Method::DELETE, &repo(&b), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, pb) = api.call(Method::GET, &format!("/v1/workspace/projects/{b}"), None).await;
    assert_eq!(pb["resources"], json!([]));

    // A project that doesn't exist takes nothing.
    let (s, _) = api.call(Method::PUT, &repo("prj_missing"), Some(json!({ "x": 0, "y": 0 }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_names_conflict_and_bad_input_is_rejected() {
    let api = Api::new();
    let id = api.create("api").await["id"].as_str().unwrap().to_string();
    let other = api.create("worker").await["id"].as_str().unwrap().to_string();

    let (s, e) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "api" }))).await;
    assert_eq!((s, e), (StatusCode::CONFLICT, json!({ "error": "a project named api exists" })));
    let (s, _) = api.call(Method::PATCH, &format!("/v1/workspace/projects/{other}"), Some(json!({ "name": "api" }))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    for name in ["", "-api", "api-", "API", "a_b", &"a".repeat(49)] {
        let (s, e) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": name }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name:?}");
        assert!(e["error"].is_string());
    }

    let (s, e) = api.call(Method::PUT, &format!("/v1/workspace/projects/{id}/resources/vm/x"), Some(json!({ "x": 0, "y": 0 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"].as_str().unwrap().contains("vm"), "{e}");
    let (s, _) = api.call(Method::PUT, &format!("/v1/workspace/projects/{id}/resources/redis/{}", "r".repeat(201)), Some(json!({ "x": 0, "y": 0 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, e) = api.call(Method::PUT, &format!("/v1/workspace/projects/{id}/resources/redis/db1"), Some(json!({ "x": "left", "y": 0 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(e["error"].is_string());
}

#[tokio::test]
async fn users_see_only_their_own_projects() {
    let api = Api::new();
    let ops = api.create("web").await["id"].as_str().unwrap().to_string();
    let (s, mine) = api.call_as(Some("usr_a"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "web" }))).await;
    assert_eq!((s, mine["owner"].as_str()), (StatusCode::CREATED, Some("usr_a")), "names are per owner");
    let mine = mine["id"].as_str().unwrap().to_string();
    // The same repo on two canvases: placing it on one doesn't move it off the other.
    let repo = |p: &str| format!("/v1/workspace/projects/{p}/resources/repo/a%2Fsite");
    api.call(Method::PUT, &repo(&ops), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, _) = api.call_as(Some("usr_a"), Method::PUT, &repo(&mine), Some(json!({ "x": 5, "y": 5 }))).await;
    assert_eq!(s, StatusCode::OK);

    let (_, list) = api.call_as(Some("usr_a"), Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!(list.as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect::<Vec<_>>(), vec![mine.as_str()]);
    for (m, path) in [(Method::GET, format!("/v1/workspace/projects/{ops}")), (Method::DELETE, format!("/v1/workspace/projects/{ops}")), (Method::PUT, repo(&ops))] {
        let (s, _) = api.call_as(Some("usr_b"), m.clone(), &path, Some(json!({ "x": 1, "y": 1 }))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {path}: another owner's project");
    }
    let (_, all) = api.call(Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2, "the operator sees every project");
    let (_, p) = api.call(Method::GET, &format!("/v1/workspace/projects/{ops}"), None).await;
    assert_eq!(p["resources"][0]["ref"], "a/site");
}

#[test]
fn projects_from_before_owners_become_the_operators() {
    // Both shapes production has had: the first tables, and those with a description and icon.
    for look in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let target = code_sql::Target::Sqlite(dir.path().join("workspace.db"));
        let db = code_sql::Db::open_or_test(&target, "workspace").unwrap();
        let columns = if look { ", description TEXT NOT NULL DEFAULT '', icon TEXT" } else { "" };
        db.ensure_schema(&format!(
            "CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL{columns}) WITHOUT ROWID;
             CREATE TABLE resources (kind TEXT NOT NULL, ref TEXT NOT NULL, project_id TEXT NOT NULL, x REAL NOT NULL, y REAL NOT NULL, added_at INTEGER NOT NULL, PRIMARY KEY (kind, ref)) WITHOUT ROWID;
             CREATE INDEX resources_project ON resources (project_id);"
        ))
        .unwrap();
        db.execute("INSERT INTO projects (id, name, created_at) VALUES ('prj_old', 'billing', 1)", ()).unwrap();
        if look {
            db.execute(r#"UPDATE projects SET description = 'Invoices', icon = '{"kind":"emoji","emoji":"🧾"}'"#, ()).unwrap();
        }
        db.execute("INSERT INTO resources (kind, ref, project_id, x, y, added_at) VALUES ('bucket', 'assets', 'prj_old', 1, 2, 3)", ()).unwrap();
        let ws = roll_workspace::Workspace::new(db.clone()).unwrap();
        let list = ws.list(roll_workspace::Scope::All).unwrap();
        assert_eq!((list.len(), list[0].owner.as_str(), list[0].resources.len()), (1, "", 1));
        let kept = (list[0].description.as_str(), list[0].icon.clone());
        assert_eq!(kept, if look { ("Invoices", Some(json!({ "kind": "emoji", "emoji": "🧾" }))) } else { ("", None) });
        drop(ws);
        // Opening again finds the new tables and leaves them be.
        assert_eq!(roll_workspace::Workspace::new(db).unwrap().list(roll_workspace::Scope::All).unwrap().len(), 1);
    }
}
