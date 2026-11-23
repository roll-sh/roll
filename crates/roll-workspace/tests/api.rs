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

/// docs/members-v1-1.md, "Groups": a group's role reaches its people on their next request, roles add
/// up, a group never holds owner, only the operator or a project admin who is in the group gives
/// it a role, a SCIM group's members are its provider's, and taking someone out (or the role away)
/// cuts them off at once, with one `group.lost` per person for what follows.
#[tokio::test]
async fn groups_hold_roles_that_add_up_and_go_at_once() {
    let api = Api::new();
    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ev = events.clone();
    api.ws.on_event(Box::new(move |e| {
        ev.lock().unwrap().push(format!("{} {}", e.action, e.subject.as_deref().unwrap_or("-")));
        Ok(())
    }));
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");
    // Groups are the operator's: a person can't make one (D12).
    assert_eq!(api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/groups", Some(json!({ "name": "eng" }))).await.0, StatusCode::FORBIDDEN);
    let (s, g) = api.call(Method::POST, "/v1/workspace/groups", Some(json!({ "name": "eng" }))).await;
    assert_eq!((s, g["source"].as_str()), (StatusCode::CREATED, Some("roll")), "{g}");
    let eng = g["id"].as_str().unwrap().to_string();
    for who in ["usr_bob", "usr_cat"] {
        assert_eq!(api.call(Method::PUT, &format!("/v1/workspace/groups/{eng}/members/{who}"), None).await.0, StatusCode::NO_CONTENT);
    }
    // Ada (an owner, not in eng) can't give it a role; once she's in it, she can. Never owner.
    let role = at(&format!("/groups/{eng}"));
    let (s, e) = api.call_as(Some("usr_ada"), Method::PUT, &role, Some(json!({ "role": "developer" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{e}");
    assert_eq!(api.call(Method::PUT, &format!("/v1/workspace/groups/{eng}/members/usr_ada"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.call_as(Some("usr_ada"), Method::PUT, &role, Some(json!({ "role": "owner" }))).await.0, StatusCode::BAD_REQUEST, "a group never holds owner");
    let (s, r) = api.call_as(Some("usr_ada"), Method::PUT, &role, Some(json!({ "role": "developer" }))).await;
    assert_eq!((s, r["role"].as_str(), r["members"].as_i64()), (StatusCode::OK, Some("developer"), Some(3)), "{r}");
    // Bob reaches the project now, through the group alone; ada is still its only owner.
    let (s, p) = api.call_as(Some("usr_bob"), Method::GET, &at(""), None).await;
    assert_eq!((s, p["role"].as_str()), (StatusCode::OK, Some("developer")), "{p}");
    let (_, list) = api.call_as(Some("usr_bob"), Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!(names(&list), vec!["shop"]);
    assert_eq!(api.ws.owners_of(&id).unwrap(), vec!["usr_ada".to_string()]);
    // Roles add up: cat, a viewer in her own right, is a developer through the group; an admin
    // group role on top makes bob an admin.
    let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "cat@example.com", "role": "viewer" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.call_as(Some("usr_cat"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await.0, StatusCode::OK);
    assert_eq!(api.ws.role_in(&id, "usr_cat").unwrap(), Some(roll_auth::roles::Role::Developer));
    assert_eq!(api.ws.direct_role(&id, "usr_cat").unwrap(), Some(roll_auth::roles::Role::Viewer));
    // A SCIM group: its provider keeps its members, roll gives it roles.
    let (ops, _) = api.ws.scim_create("sso_acme", "ops", Some("okta-1"), &["usr_bob".into()]).unwrap();
    assert_eq!(api.call(Method::PUT, &format!("/v1/workspace/groups/{}/members/usr_cat", ops.id), None).await.1["code"], "managed");
    assert_eq!(api.call(Method::PUT, &at(&format!("/groups/{}", ops.id)), Some(json!({ "role": "admin" }))).await.0, StatusCode::OK);
    assert_eq!(api.ws.role_in(&id, "usr_bob").unwrap(), Some(roll_auth::roles::Role::Admin));
    // An admin only through groups can't make that his own by inviting himself, nor leave as if
    // he were a member (the groups decide).
    let (s, e) = api.call_as(Some("usr_bob"), Method::POST, &at("/invites"), Some(json!({ "email": "bob@example.com", "role": "admin" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{e}");
    let (s, e) = api.call_as(Some("usr_bob"), Method::DELETE, &at("/members/me"), None).await;
    assert!(s == StatusCode::CONFLICT && e["error"].as_str().unwrap().contains("eng"), "{s} {e}");
    // A scoped token is never D11's operator, whatever its rules.
    let everything = roll_auth::roles::session_scope(&[roll_auth::roles::Membership { project: id.clone(), role: roll_auth::roles::Role::Owner, environments: None }]);
    let token = roll_workspace::Actor { id: "token:tok_ops".into(), grant: Some(everything), ..Default::default() };
    assert!(matches!(api.ws.set_group_role(&token, &id, &ops.id, "viewer", None), Err(roll_workspace::Error::Forbidden(_))));
    // Cat sees the group she is in, not one that only has a role on a project she doesn't manage.
    let (_, mine) = api.call_as(Some("usr_cat"), Method::GET, "/v1/workspace/groups", None).await;
    assert_eq!(mine.as_array().unwrap().iter().map(|g| g["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["eng"]);
    assert_eq!(api.call_as(Some("usr_cat"), Method::GET, &format!("/v1/workspace/groups/{}", ops.id), None).await.0, StatusCode::NOT_FOUND);

    // The provider takes bob out of ops: he's a developer again at once, and one `group.lost`
    // follows for him. Out of eng too, he doesn't reach the project at all.
    events.lock().unwrap().clear();
    let n = api.ws.scim_patch("sso_acme", &ops.id, &[roll_workspace::GroupOp::RemoveMembers(vec!["usr_bob".into()])]).unwrap();
    assert_eq!(n.removed, 1);
    api.ws.drain_events(10);
    assert_eq!(api.ws.role_in(&id, "usr_bob").unwrap(), Some(roll_auth::roles::Role::Developer));
    assert!(events.lock().unwrap().contains(&"group.lost usr_bob".to_string()), "{:?}", events.lock().unwrap());
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/groups/{eng}/members/usr_bob"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.call_as(Some("usr_bob"), Method::GET, &at(""), None).await.0, StatusCode::NOT_FOUND);
    // Eng's role taken away: cat is back to her own viewer, and each of its people gets a
    // `group.lost` once the group's event fans out.
    events.lock().unwrap().clear();
    assert_eq!(api.call_as(Some("usr_ada"), Method::DELETE, &role, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.ws.role_in(&id, "usr_cat").unwrap(), Some(roll_auth::roles::Role::Viewer));
    api.ws.drain_events(10);
    // Cat lost the developer role; ada, an owner in her own right, lost nothing and gets none.
    let log = events.lock().unwrap().clone();
    assert!(log.contains(&"group.lost usr_cat".to_string()) && !log.contains(&"group.lost usr_ada".to_string()) && log.contains(&"group.role_remove -".to_string()), "{log:?}");
}

/// docs/members-v1-1.md, "When a group loses a role": a big group's fan-out never holds up a
/// member's own removal (what ends their shares and leases): members' events are claimed first,
/// and a request runs one round of the outbox, leaving the rest to rolld's tick.
#[tokio::test]
async fn a_big_groups_fan_out_never_holds_up_a_removal() {
    let api = Api::new();
    let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let o = order.clone();
    api.ws.on_event(Box::new(move |e| {
        o.lock().unwrap().push(e.action.clone());
        Ok(())
    }));
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "big" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let people: Vec<String> = (0..300).map(|i| format!("usr_p{i:03}")).collect();
    let (g, _) = api.ws.scim_create("sso_acme", "everyone", None, &people).unwrap();
    api.ws.set_group_role(&roll_workspace::Actor::operator(), &id, &g.id, "viewer", None).unwrap();
    let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &format!("/v1/workspace/projects/{id}/invites"), Some(json!({ "email": "bob@example.com", "role": "viewer" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    assert_eq!(api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await.0, StatusCode::OK);
    order.lock().unwrap().clear();
    // The provider deletes the group (300 people to fan out), then ada removes bob.
    api.ws.scim_delete("sso_acme", &g.id).unwrap();
    assert_eq!(api.call_as(Some("usr_ada"), Method::DELETE, &format!("/v1/workspace/projects/{id}/members/usr_bob"), None).await.0, StatusCode::NO_CONTENT);
    api.ws.drain_events(100);
    let log = order.lock().unwrap().clone();
    let removal = log.iter().position(|a| a == "member.remove").expect("the removal ran");
    let lost = log.iter().filter(|a| *a == "group.lost").count();
    assert_eq!(lost, 300, "one per person, not per person and project");
    assert!(removal < 60, "the removal waited behind {removal} events of the group's: {:?}", &log[..removal.min(10)]);
}

#[tokio::test]
async fn a_product_may_refuse_a_delete_and_a_deleted_project_comes_back_with_its_canvas_and_members() {
    let api = Api::new();
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");
    let (s, _) = api.call(Method::PUT, &at("/resources/bucket/assets"), Some(json!({ "x": 1, "y": 2 }))).await;
    assert_eq!(s, StatusCode::OK);
    // Bob a member, carol still invited.
    async fn invite(api: &Api, path: &str, email: &str, role: &str) -> String {
        let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, path, Some(json!({ "email": email, "role": role }))).await;
        inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string()
    }
    let bob = invite(&api, &at("/invites"), "bob@example.com", "developer").await;
    assert_eq!(api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{bob}/accept"), None).await.0, StatusCode::OK);
    let carol = invite(&api, &at("/invites"), "carol@example.com", "viewer").await;
    // Two groups with roles: dan through one, eve through one deleted while the project is.
    let mut groups = vec![];
    for (name, who) in [("ops", "usr_dan"), ("temp", "usr_eve")] {
        let (_, g) = api.call(Method::POST, "/v1/workspace/groups", Some(json!({ "name": name }))).await;
        let g = g["id"].as_str().unwrap().to_string();
        api.call(Method::PUT, &format!("/v1/workspace/groups/{g}/members/{who}"), None).await;
        assert_eq!(api.call(Method::PUT, &at(&format!("/groups/{g}")), Some(json!({ "role": "viewer" }))).await.0, StatusCode::OK);
        groups.push(g);
    }

    // A product's no, answered as it gave it, until the request carries what it asks for.
    api.ws.before_delete(Box::new(|d| match d.request.uri.query() {
        Some("confirm=shop") => Ok(()),
        _ => Err(roll_workspace::Refusal { status: 409, code: "holds_things".into(), message: format!("{} holds things", d.project.name) }),
    }));
    let told = Arc::new(std::sync::Mutex::new(vec![]));
    let t = told.clone();
    api.ws.on_delete(Box::new(move |d| t.lock().unwrap().push(d.project.id.clone())));
    let (s, e) = api.call_as(Some("usr_ada"), Method::DELETE, &at(""), None).await;
    assert_eq!((s, e["code"].as_str(), e["error"].as_str()), (StatusCode::CONFLICT, Some("holds_things"), Some("shop holds things")));
    assert!(told.lock().unwrap().is_empty());
    assert_eq!(api.call(Method::GET, &at(""), None).await.0, StatusCode::OK);
    assert_eq!(api.call_as(Some("usr_ada"), Method::DELETE, &at("?confirm=shop"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(*told.lock().unwrap(), vec![id.clone()]);
    assert_eq!(api.call(Method::GET, &at(""), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.ws.deleted(&id).unwrap().unwrap().resources.len(), 1);
    // Its members reach nothing while it's deleted (through groups neither), and its invites ended.
    assert!(api.ws.memberships_of("usr_bob").unwrap().is_empty());
    assert!(api.ws.memberships_of("usr_dan").unwrap().is_empty());
    assert_eq!(api.call(Method::DELETE, &format!("/v1/workspace/groups/{}", groups[1]), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.call_as(Some("usr_carol"), Method::POST, &format!("/v1/workspace/invites/{carol}/accept"), None).await.0, StatusCode::NOT_FOUND);

    // Its name taken meanwhile: not restored over another project.
    let (_, other) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await;
    assert!(matches!(api.ws.restore(&id), Err(roll_workspace::Error::Conflict(_))));
    let (s, _) = api.call_as(Some("usr_ada"), Method::PATCH, &format!("/v1/workspace/projects/{}", other["id"].as_str().unwrap()), Some(json!({ "name": "shop-2" }))).await;
    assert_eq!(s, StatusCode::OK);
    let back = api.ws.restore(&id).unwrap();
    assert_eq!((back.name.as_str(), back.resources.len(), back.resources[0].x), ("shop", 1, 1.0));
    assert!(api.ws.deleted(&id).unwrap().is_none());
    // Back with its members and groups' roles, as they were; the deleted group's role isn't.
    let (s, p) = api.call_as(Some("usr_bob"), Method::GET, &at(""), None).await;
    assert_eq!((s, p["role"].as_str()), (StatusCode::OK, Some("developer")));
    assert_eq!(api.call_as(Some("usr_dan"), Method::GET, &at(""), None).await.1["role"], "viewer");
    let (_, roles) = api.call(Method::GET, &at("/groups"), None).await;
    assert_eq!(roles.as_array().unwrap().iter().map(|r| r["group"].as_str().unwrap()).collect::<Vec<_>>(), vec![groups[0].as_str()]);
    // Once a product purged it, it's gone for good.
    assert_eq!(api.call(Method::DELETE, &at("?confirm=shop"), None).await.0, StatusCode::NO_CONTENT);
    api.ws.forget_deleted(&id).unwrap();
    assert!(matches!(api.ws.restore(&id), Err(roll_workspace::Error::NotFound(_))));
}

#[tokio::test]
async fn a_role_change_is_the_next_event_and_after_returns_only_later_ones() {
    let api = Api::new();
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "pay" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");
    let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "bob@example.com", "role": "developer" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;

    // The maker joining, the invite sent and accepted, bob joining: the head is at 4.
    let (s, head) = api.call_as(Some("usr_ada"), Method::GET, &at("/events?latest=1"), None).await;
    assert_eq!((s, head), (StatusCode::OK, json!({ "events": [], "next": 4 })));
    for bad in ["/events?latest=1&after=1", "/events?kinds=member.nope"] {
        assert_eq!(api.call_as(Some("usr_ada"), Method::GET, &at(bad), None).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }

    let (s, _) = api.call_as(Some("usr_ada"), Method::PATCH, &at("/members/usr_bob"), Some(json!({ "role": "viewer" }))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, page) = api.call_as(Some("usr_ada"), Method::GET, &at("/events?after=4"), None).await;
    let e = &page["events"][0];
    assert_eq!((page["events"].as_array().unwrap().len(), &page["next"], &e["seq"], &e["kind"]), (1, &json!(5), &json!(5), &json!("member.role_changed")), "{page}");
    assert_eq!(e["data"], json!({ "user": "usr_bob", "handle": "bob", "from": { "role": "developer", "environments": null }, "to": { "role": "viewer", "environments": null }, "actor": "usr_ada" }));

    // A family, paged; a page cut short by its filter moves the cursor to the head.
    let (_, page) = api.call_as(Some("usr_ada"), Method::GET, &at("/events?kinds=member.*&limit=2"), None).await;
    let seqs = |p: &Value| p["events"].as_array().unwrap().iter().map(|e| e["seq"].as_i64().unwrap()).collect::<Vec<_>>();
    assert_eq!((seqs(&page), &page["next"]), (vec![1, 4], &json!(4)));
    let (_, page) = api.call_as(Some("usr_ada"), Method::GET, &at("/events?kinds=invite.*&after=3"), None).await;
    assert_eq!((seqs(&page), &page["next"]), (vec![], &json!(5)));
}

#[tokio::test]
async fn a_projects_events_are_for_whoever_sees_its_members() {
    let api = Api::new();
    let (_, p) = api.call_as(Some("usr_ada"), Method::POST, "/v1/workspace/projects", Some(json!({ "name": "pay" }))).await;
    let id = p["id"].as_str().unwrap().to_string();
    let at = |rest: &str| format!("/v1/workspace/projects/{id}{rest}");
    let (_, inv) = api.call_as(Some("usr_ada"), Method::POST, &at("/invites"), Some(json!({ "email": "bob@example.com", "role": "viewer" }))).await;
    let token = inv["url"].as_str().unwrap().split("token=").nth(1).unwrap().to_string();
    api.call_as(Some("usr_bob"), Method::POST, &format!("/v1/workspace/invites/{token}/accept"), None).await;

    // Someone outside the project gets the members route's 404.
    for path in ["/members", "/events"] {
        assert_eq!(api.call_as(Some("usr_eve"), Method::GET, &at(path), None).await.0, StatusCode::NOT_FOUND, "{path}");
    }
    // A viewer reads it, without the invite's address; its admin sees it, as in the members list.
    let sent = |page: &Value| page["events"].as_array().unwrap().iter().find(|e| e["kind"] == "invite.sent").unwrap()["data"].clone();
    let (s, page) = api.call_as(Some("usr_bob"), Method::GET, &at("/events"), None).await;
    assert_eq!((s, sent(&page).get("email")), (StatusCode::OK, None), "{page}");
    assert!(!page.to_string().contains("rlv_"), "never a token: {page}");
    let (_, page) = api.call_as(Some("usr_ada"), Method::GET, &at("/events"), None).await;
    assert_eq!(sent(&page)["email"], "bob@example.com");
}

/// Two nodes changing roles in one project while a follower on each reads after its cursor: rows
/// commit in `seq` order, so no follower ever passes one that commits later.
#[test]
fn two_nodes_changing_roles_never_leave_a_gap_a_follower_has_passed() {
    use roll_workspace::{Actor, EventsQuery, Scope, Workspace};
    let dir = tempfile::tempdir().unwrap();
    let target = code_sql::Target::Sqlite(dir.path().join("workspace.db"));
    let node = || {
        let ws = Arc::new(Workspace::new(code_sql::Db::open_or_test(&target, "workspace").unwrap()).unwrap());
        ws.set_people(Arc::new(People));
        ws
    };
    let (a, b) = (node(), node());
    let project = a.create("", "race", "", None).unwrap().id;
    for user in ["bob", "carol"] {
        let (_, url) = a.invite(&Actor::operator(), &project, &format!("{user}@example.com"), "developer", None).unwrap();
        a.accept(&format!("usr_{user}"), url.split("token=").nth(1).unwrap(), None).unwrap();
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = [(a.clone(), "usr_bob"), (b.clone(), "usr_carol"), (a.clone(), "usr_carol"), (b.clone(), "usr_bob")]
        .into_iter()
        .enumerate()
        .map(|(n, (ws, user))| {
            let project = project.clone();
            std::thread::spawn(move || {
                for i in 0..15 {
                    let role = if (i + n) % 2 == 0 { "viewer" } else { "admin" };
                    ws.set_role(&Actor::operator(), &project, user, role, None).unwrap();
                }
            })
        })
        .collect();
    let follow = |ws: Arc<Workspace>| {
        let (done, project) = (done.clone(), project.clone());
        std::thread::spawn(move || {
            let (mut seen, mut after) = (Vec::new(), 0);
            loop {
                let last = done.load(std::sync::atomic::Ordering::SeqCst);
                let page = ws.events(Scope::All, &project, EventsQuery { after: Some(after), limit: Some(3), ..Default::default() }, true).unwrap();
                seen.extend(page.events.iter().map(|e| e.seq));
                after = page.next;
                if last && page.events.is_empty() {
                    return seen;
                }
            }
        })
    };
    let followers = [follow(a.clone()), follow(b.clone())];
    for w in writers {
        w.join().unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let all: Vec<i64> = a.events(Scope::All, &project, EventsQuery { limit: Some(500), ..Default::default() }, true).unwrap().events.iter().map(|e| e.seq).collect();
    assert_eq!(all, (1..=all.len() as i64).collect::<Vec<_>>(), "no gaps");
    assert!(all.len() > 6, "two invites' three events each, then the role changes that changed a role: {}", all.len());
    for f in followers {
        assert_eq!(f.join().unwrap(), all);
    }
}
