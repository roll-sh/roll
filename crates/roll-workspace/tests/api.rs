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
    ws: Arc<roll_workspace::Workspace>,
    _dir: tempfile::TempDir,
}

/// People as roll accounts has them: `usr_<handle>` with `<handle>@example.com`.
struct People;

impl roll_workspace::People for People {
    fn person(&self, user: &str) -> Option<roll_workspace::Person> {
        let handle = user.strip_prefix("usr_")?;
        (handle != "gone").then(|| roll_workspace::Person { handle: handle.into(), email: Some(format!("{handle}@example.com")), admin: handle == "root" })
    }
}

impl Api {
    fn new() -> Api {
        let dir = tempfile::tempdir().unwrap();
        let target = code_sql::Target::Sqlite(dir.path().join("workspace.db"));
        let db = code_sql::Db::open_or_test(&target, "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(db).unwrap());
        ws.set_people(Arc::new(People));
        ws.set_web_url("https://roll.example");
        Api { router: roll_workspace::router(ws.clone()), ws, _dir: dir }
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
            // What rolld's gate adds: the person's memberships as their session's grant, now.
            let scope = roll_auth::roles::session_scope(&self.ws.memberships_of(u).unwrap());
            req.extensions_mut().insert(roll_auth::Grant { id: "ses_test".into(), name: "session".into(), kind: roll_auth::GrantKind::Session, scope: Arc::new(scope), user: Some(u.into()), ask_otherwise: false });
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

/// docs/members.md, "Lifecycle": invite, accept by the right address only, change a role, the
/// last owner, leave and remove; nobody grants more than they hold; every change is an event.
#[tokio::test]
async fn members_are_invited_changed_and_removed() {
    let api = Api::new();
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = events.clone();
    api.ws.on_event(Box::new(move |e| {
        seen.lock().unwrap().push(format!("{} {}", e.action, e.subject.as_deref().unwrap_or("-")));
        Ok(())
    }));
    let (s, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "pay" }))).await;
    assert_eq!((s, p["role"].as_str()), (StatusCode::CREATED, Some("owner")), "its maker is its owner");
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");

    // Invite bob as a developer: the link is in the answer once, never in the list.
    let (s, inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "Bob@Example.com", "role": "developer" }))).await;
    assert_eq!((s, inv["email"].as_str(), inv["role"].as_str()), (StatusCode::CREATED, Some("bob@example.com"), Some("developer")), "{inv}");
    let url = inv["url"].as_str().unwrap();
    assert!(url.starts_with("https://roll.example/dashboard/invites?token=rlv_") && roll_auth::credentials::checksum_ok(url.split("token=").nth(1).unwrap()), "{url}");
    let token = url.split("token=").nth(1).unwrap().to_string();
    let (_, list) = api.call_as(Some("usr_ada"), Method::GET, &at("/invites"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains(&token) && list[0].get("url").is_none());

    // Until bob accepts, he's nobody here; carol can't take his invite.
    let (s, _) = api.call_as(Some("usr_bob"), Method::GET, &at(""), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = api.call_as(Some("usr_carol"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "an invite is for its address only");
    let (_, mine) = api.call_as(Some("usr_bob"), Method::GET, "/v1/workspace/invites", None).await;
    assert_eq!((mine.as_array().unwrap().len(), mine[0]["project_name"].as_str()), (1, Some("pay")));
    let (s, m) = api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;
    assert_eq!((s, m["role"].as_str(), m["user"].as_str()), (StatusCode::OK, Some("developer"), Some("usr_bob")), "{m}");
    let (s, _) = api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "used once");
    let (_, list) = api.call_as(Some("usr_bob"), Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((list.as_array().unwrap().len(), list[0]["role"].as_str()), (1, Some("developer")));

    // Members' emails are for the project's admins.
    let (_, ms) = api.call_as(Some("usr_bob"), Method::GET, &at("/members"), None).await;
    assert_eq!(ms.as_array().unwrap().len(), 2);
    assert!(ms.as_array().unwrap().iter().all(|m| m.get("email").is_none()), "{ms}");
    let (_, ms) = api.call_as(Some("usr_ada"), Method::GET, &at("/members"), None).await;
    assert_eq!(ms[1]["email"].as_str(), Some("bob@example.com"));

    // A developer manages nobody.
    let (s, _) = api.call_as(Some("usr_bob"), Method::POST, &at("/invites"), Some(json!({ "email": "dan@example.com", "role": "viewer" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Carol joins as an admin: she manages up to admin, never an owner.
    let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "carol@example.com", "role": "admin" }))).await;
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    api.call_as(Some("usr_carol"), Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await;
    let (s, m) = api.call_as(Some("usr_carol"), Method::PATCH, &at("/members/usr_bob"), Some(json!({ "role": "developer", "environments": ["production"] }))).await;
    assert_eq!((s, &m["environments"]), (StatusCode::OK, &json!(["production"])), "{m}");
    for (body, who) in [(json!({ "role": "owner" }), "usr_bob"), (json!({ "role": "viewer" }), "usr_ada")] {
        let (s, e) = api.call_as(Some("usr_carol"), Method::PATCH, &at(&format!("/members/{who}")), Some(body)).await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "an admin can't make or change an owner: {e}");
    }
    let (s, _) = api.call_as(Some("usr_carol"), Method::POST, &at("/invites"), Some(json!({ "email": "dan@example.com", "role": "owner" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = api.call_as(Some("usr_carol"), Method::DELETE, &at("/members/usr_ada"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // The last owner can't leave or be demoted until there's another.
    let (s, e) = api.call_as(Some("usr_ada"), Method::DELETE, &at("/members/me"), None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("last_owner")));
    let (s, e) = api.call_as(Some("usr_ada"), Method::PATCH, &at("/members/usr_ada"), Some(json!({ "role": "admin" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("last_owner")));
    let (s, _) = api.call_as(Some("usr_ada"), Method::PATCH, &at("/members/usr_carol"), Some(json!({ "role": "owner" }))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = api.call_as(Some("usr_ada"), Method::DELETE, &at("/members/me"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api.call_as(Some("usr_ada"), Method::GET, &at(""), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "gone at once");
    let (s, _) = api.call_as(Some("usr_carol"), Method::DELETE, &at("/members/usr_bob"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, ms) = api.call_as(Some("usr_carol"), Method::GET, &at("/members"), None).await;
    assert_eq!(ms.as_array().unwrap().iter().map(|m| m["user"].as_str().unwrap()).collect::<Vec<_>>(), vec!["usr_carol"]);

    assert_eq!(
        *events.lock().unwrap(),
        [
            "member.invite -", "member.accept usr_bob", "member.invite -", "member.accept usr_carol", "member.role usr_bob",
            "member.role usr_carol", "member.leave usr_ada", "member.remove usr_bob",
        ]
    );

    // The operator's projects take members too (D8), and their owner is the operator.
    let ops = api.create("shared").await["id"].as_str().unwrap().to_string();
    let (s, inv) = api.call(Method::POST, &format!("/v1/workspace/projects/{ops}/invites"), Some(json!({ "email": "bob@example.com", "role": "viewer" }))).await;
    assert_eq!(s, StatusCode::CREATED);
    let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    let (s, _) = api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{t}/decline"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

/// `workspace-members`: every project a person made before members makes them its owner.
#[test]
fn makers_become_owners() {
    let dir = tempfile::tempdir().unwrap();
    let target = code_sql::Target::Sqlite(dir.path().join("workspace.db"));
    let db = code_sql::Db::open_or_test(&target, "workspace").unwrap();
    db.ensure_schema(
        "CREATE TABLE projects (id TEXT PRIMARY KEY, owner TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '', icon TEXT, created_at INTEGER NOT NULL, UNIQUE (owner, name)) WITHOUT ROWID;
         CREATE TABLE resources (owner TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL, ref TEXT NOT NULL, project_id TEXT NOT NULL, x REAL NOT NULL, y REAL NOT NULL, added_at INTEGER NOT NULL, PRIMARY KEY (owner, kind, ref)) WITHOUT ROWID;",
    )
    .unwrap();
    db.execute("INSERT INTO projects (id, owner, name, created_at) VALUES ('prj_mine', 'usr_ada', 'web', 1), ('prj_ops', '', 'web', 2)", ()).unwrap();
    let ws = roll_workspace::Workspace::new(db).unwrap();
    let m = ws.memberships_of("usr_ada").unwrap();
    assert_eq!((m.len(), m[0].project.as_str(), m[0].role), (1, "prj_mine", roll_auth::roles::Role::Owner));
    assert_eq!(ws.owners_of("prj_ops").unwrap(), Vec::<String>::new(), "the operator's projects get no members");
}

/// The security review's finding 5: a home is kept only for an id a product never gives out
/// again, and goes when the product deletes the resource, so nothing made later under the same
/// ref inherits a project's trust.
#[tokio::test]
async fn homes_are_kept_only_for_ids_never_given_out_again() {
    let api = Api::new();
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "pay" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let (s, _) = api.call(Method::PUT, &format!("/v1/workspace/projects/{id}/homes/redis/rdb_1"), None).await;
    assert_eq!(s, StatusCode::OK);
    let project = api.ws.get(roll_workspace::Scope::All, &id).unwrap();
    assert!(api.ws.trusts(&project, roll_workspace::Kind::Redis, "rdb_1"));
    // Deleted by its product: the home goes, so a new `rdb_1` (were one ever made) is a stranger.
    api.ws.forget_resource(roll_workspace::Kind::Redis, "rdb_1").unwrap();
    assert_eq!(api.ws.home_of(roll_workspace::Kind::Redis, "rdb_1").unwrap(), None);
    assert!(!api.ws.trusts(&project, roll_workspace::Kind::Redis, "rdb_1"));
    // A bucket goes by a name someone else can make again: no home, by anyone.
    let (s, e) = api.call(Method::PUT, &format!("/v1/workspace/projects/{id}/homes/bucket/acme-prod"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(!api.ws.trusts(&project, roll_workspace::Kind::Bucket, "acme-prod"));
}

/// roll-access as far as an invite's inviter goes: the operator's token `tok_ci`, every rule, until
/// it's revoked.
#[derive(Default)]
struct Revocable(std::sync::Mutex<bool>);

impl roll_auth::Grants for Revocable {
    fn grant(&self, id: &str) -> Option<roll_auth::Grant> {
        let all: roll_auth::scope::Scope = serde_json::from_value(json!([{ "product": "*", "resources": "*", "actions": { "read": "allow", "write": "allow", "admin": "allow", "delete": "allow", "reveal": "allow", "deploy": "allow" } }])).unwrap();
        (id == "tok_ci" && !*self.0.lock().unwrap()).then(|| roll_auth::Grant { id: id.into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(all), user: None, ask_otherwise: false })
    }
}

/// What an admin account is to `Owners`: anything (`trusts`), but nothing of its own (`owns`).
struct AdminEverything;

impl roll_workspace::Owners for AdminEverything {
    fn trusts(&self, user: &str, _: roll_workspace::Kind, _: &str) -> bool {
        user == "usr_root"
    }
    fn owns(&self, _: &str, _: roll_workspace::Kind, _: &str) -> bool {
        false
    }
}

/// The re-review (docs/members.md, "Lifecycle"): an invite never gives more than its inviter can
/// when it's accepted, and goes when their role no longer gives it; a re-invite never quietly
/// takes a higher invite away; an admin account made an owner of someone's project adds nothing of
/// its own; a deleted account leaves every project and its invites go, each audited.
#[tokio::test]
async fn invites_follow_their_inviter_and_admin_owners_add_nothing() {
    let api = Api::new();
    api.ws.set_owners(std::sync::Arc::new(AdminEverything));
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = events.clone();
    api.ws.on_event(Box::new(move |e| {
        seen.lock().unwrap().push(format!("{} {}", e.action, e.subject.as_deref().unwrap_or("-")));
        Ok(())
    }));
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "pay" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");
    let join = |who: &'static str, role: &'static str| {
        let (api, at) = (&api, at(""));
        async move {
            let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &format!("{at}/invites"), Some(json!({ "email": format!("{who}@example.com"), "role": role }))).await;
            let t = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
            assert_eq!(api.call_as(Some(&format!("usr_{who}")), Method::POST, &format!("/v1/workspace/invites/{t}/accept"), None).await.0, StatusCode::OK);
        }
    };
    join("cat", "admin").await;

    // Cat invites two people; then she's demoted: what her role no longer gives goes, at once.
    let (s, inv) = api.call_as(Some("usr_cat"), Method::POST, &at("/invites"), Some(json!({ "email": "dan@example.com", "role": "admin" }))).await;
    assert_eq!(s, StatusCode::CREATED);
    let dan = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    // Re-inviting an address replaces its pending invite only for someone who may give that
    // invite's role: an admin can't quietly swap an owner's invite for a viewer's.
    let (_, owner_inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "eve@example.com", "role": "owner" }))).await;
    let (s, e) = api.call_as(Some("usr_cat"), Method::POST, &at("/invites"), Some(json!({ "email": "eve@example.com", "role": "viewer" }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "a re-invite can't take an owner's invite away: {e}");
    assert!(owner_inv["id"].is_string());
    assert_eq!(api.call_as(Some("usr_ada"), Method::PATCH, &at("/members/usr_cat"), Some(json!({ "role": "viewer" }))).await.0, StatusCode::OK);
    let (_, pending) = api.call_as(Some("usr_ada"), Method::GET, &at("/invites"), None).await;
    let emails: Vec<&str> = pending.as_array().unwrap().iter().map(|i| i["email"].as_str().unwrap()).collect();
    assert_eq!(emails, ["eve@example.com"], "cat's invite went with her role");
    assert_eq!(api.call_as(Some("usr_dan"), Method::POST, &format!("/v1/workspace/invites/{dan}/accept"), None).await.0, StatusCode::NOT_FOUND);

    // An invite whose inviter can no longer give it isn't accepted: checked again at accept time
    // (here, the operator's token that made it was revoked meanwhile).
    let ci = Arc::new(Revocable::default());
    api.ws.set_grants(ci.clone());
    let mut req = axum::http::Request::builder().method(Method::POST).uri(at("/invites")).header("content-type", "application/json").body(axum::body::Body::from(json!({ "email": "fay@example.com", "role": "admin" }).to_string())).unwrap();
    req.extensions_mut().insert(roll_auth::Grants::grant(&*ci, "tok_ci").unwrap());
    let resp = api.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let inv: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let fay = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    *ci.0.lock().unwrap() = true;
    let (s, e) = api.call_as(Some("usr_fay"), Method::POST, &format!("/v1/workspace/invites/{fay}/accept"), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");

    // An admin account made an owner of ada's project adds nothing of its own to what it trusts.
    join("root", "owner").await;
    let project = api.ws.get(roll_workspace::Scope::All, &id).unwrap();
    assert!(!api.ws.trusts(&project, roll_workspace::Kind::Bucket, "anyone-elses"));

    // Root deletes their account: they leave, and their pending invites go, each in the audit.
    let (_, inv) = api.call_as(Some("usr_root"), Method::POST, &at("/invites"), Some(json!({ "email": "gus@example.com", "role": "viewer" }))).await;
    assert!(inv["id"].is_string(), "{inv}");
    assert_eq!(api.ws.forget_person("usr_root", None).unwrap(), 1);
    assert_eq!(api.ws.role_in(&id, "usr_root").unwrap(), None);
    let log = events.lock().unwrap().clone();
    assert!(log.contains(&"member.remove usr_root".to_string()) && log.contains(&"member.invite_revoke usr_root".to_string()), "{log:?}");
    assert!(log.contains(&"member.invite_revoke usr_cat".to_string()), "cat's demotion revoked hers: {log:?}");
}
