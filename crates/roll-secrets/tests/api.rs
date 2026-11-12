//! `/v1/secrets` in process, on SQLite in a tempdir (docs/secrets.md), next to the workspace
//! router it reads projects from. One test per flow a person relies on.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use roll_secrets::links::{Field, Issued, Link, LinkProvider, Links, Mode};
use roll_secrets::{Keyring, Options, Secrets, Value as Secret};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

/// A fake product: a database whose password rotates, and access keys it issues and revokes.
#[derive(Default)]
struct Fake {
    password: Mutex<u32>,
    issued: Mutex<u32>,
    live_keys: Mutex<Vec<String>>,
}

impl LinkProvider for Fake {
    fn kind(&self) -> &str {
        "bucket"
    }
    fn fields(&self) -> &[Field] {
        const F: &[Field] = &[
            Field { field: "url", label: "URL", modes: &[Mode::Live], secret: true, suggest: "BUCKET_URL", credential: None },
            Field { field: "access_key_id", label: "Access key id", modes: &[Mode::Managed, Mode::Dynamic], secret: false, suggest: "AWS_ACCESS_KEY_ID", credential: Some("key") },
            Field { field: "secret_access_key", label: "Secret", modes: &[Mode::Managed, Mode::Dynamic], secret: true, suggest: "AWS_SECRET_ACCESS_KEY", credential: Some("key") },
        ];
        F
    }
    fn read(&self, r#ref: &str, _: &str, _: &Link) -> roll_secrets::Result<Secret> {
        Ok(Secret::new(format!("s3://{ref}?pw={}", self.password.lock().unwrap())))
    }
    fn issue(&self, _: &str, _: &str, _: &Link, _: &str, _: Option<u64>) -> roll_secrets::Result<Issued> {
        let mut n = self.issued.lock().unwrap();
        *n += 1;
        let id = format!("AK{n}");
        self.live_keys.lock().unwrap().push(id.clone());
        let parts = BTreeMap::from([("access_key_id".to_string(), Secret::new(id.clone())), ("secret_access_key".to_string(), Secret::new(format!("sk-{n}")))]);
        Ok(Issued { parts, handle: json!({ "id": id }) })
    }
    fn revoke(&self, _: &str, handle: &Value) -> roll_secrets::Result<()> {
        self.live_keys.lock().unwrap().retain(|k| Some(k.as_str()) != handle["id"].as_str());
        Ok(())
    }
    fn rotate_source(&self, _: &str) -> roll_secrets::Result<bool> {
        *self.password.lock().unwrap() += 1;
        Ok(true)
    }
}

struct Api {
    router: Router,
    secrets: Arc<Secrets>,
    fake: Arc<Fake>,
    project: String,
    _dir: tempfile::TempDir,
}

impl Api {
    async fn new() -> Api {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let fake = Arc::new(Fake::default());
        let mut links = Links::default();
        links.register(fake.clone());
        let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, open: true, trust_forwarded_for: false, web_url: None };
        let secrets = Arc::new(Secrets::new(db, ws.clone(), opts).unwrap());
        let router = roll_workspace::router(ws).merge(roll_secrets::router(secrets.clone()));
        let mut api = Api { router, secrets, fake, project: String::new(), _dir: dir };
        let (_, p) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await;
        api.project = p["id"].as_str().unwrap().to_string();
        api
    }

    async fn req(&self, method: Method, path: &str, body: Option<Value>, token: Option<&str>) -> (StatusCode, Value) {
        let path = if path.starts_with("/v1/") { path.to_string() } else { format!("/v1/secrets/projects/{}{path}", self.project) };
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.req(method, path, body, None).await
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method.clone(), path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn set(&self, env: &str, key: &str, value: &str) -> Value {
        self.ok(Method::PUT, &format!("/environments/{env}/secrets/{key}"), Some(json!({ "value": value }))).await
    }

    async fn values(&self, env: &str, path: &str) -> Value {
        self.ok(Method::GET, &format!("/environments/{env}/values?path={path}"), None).await
    }
}

fn slugs(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|e| e["slug"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn environments_start_as_three_and_can_be_added_renamed_reordered_and_deleted() {
    let api = Api::new().await;
    let envs = api.ok(Method::GET, "/environments", None).await;
    assert_eq!(slugs(&envs), ["development", "staging", "production"]);
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "qa" }))).await;
    let (s, e) = api.call(Method::POST, "/environments", Some(json!({ "slug": "qa" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    // A reference to staging follows the rename.
    api.set("staging", "HOST", "db.internal").await;
    api.set("development", "UPSTREAM", "https://${staging.HOST}/api").await;
    let e = api.ok(Method::PATCH, "/environments/staging", Some(json!({ "slug": "preprod", "name": "Pre-production" }))).await;
    assert_eq!((e["slug"].as_str(), e["name"].as_str()), (Some("preprod"), Some("Pre-production")));
    assert_eq!(api.values("development", "/").await["values"]["UPSTREAM"], "https://db.internal/api");
    let s = api.ok(Method::GET, "/environments/development/secrets/UPSTREAM", None).await;
    assert_eq!(s["references"], json!(["preprod.HOST"]));

    let envs = api.ok(Method::PUT, "/environments/order", Some(json!({ "order": ["qa", "development", "preprod", "production"] }))).await;
    assert_eq!(slugs(&envs), ["qa", "development", "preprod", "production"]);
    let (s, _) = api.call(Method::DELETE, "/environments/qa", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    for slug in ["development", "preprod"] {
        api.call(Method::DELETE, &format!("/environments/{slug}"), None).await;
    }
    let (s, e) = api.call(Method::DELETE, "/environments/production", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("last_environment")));
}

#[tokio::test]
async fn every_write_is_a_version_and_an_environment_rolls_back_to_any_change() {
    let api = Api::new().await;
    api.set("production", "A", "1").await;
    let first = api.ok(Method::POST, "/environments/production/changes", Some(json!({ "message": "launch", "ops": [{ "op": "set", "key": "B", "value": "2", "comment": "the b" }, { "op": "set", "key": "A", "value": "10" }] }))).await;
    let seq = first["change"]["seq"].as_i64().unwrap();
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "delete", "key": "A" }, { "op": "set", "key": "C", "value": "3" }] }))).await;
    assert_eq!(api.values("production", "/").await["values"], json!({ "B": "2", "C": "3" }));

    let versions = api.ok(Method::GET, "/environments/production/secrets/A/versions", None).await;
    let ops: Vec<(i64, &str)> = versions.as_array().unwrap().iter().map(|v| (v["version"].as_i64().unwrap(), v["op"].as_str().unwrap())).collect();
    assert_eq!(ops, [(3, "delete"), (2, "set"), (1, "set")]);
    let old = api.ok(Method::GET, "/environments/production/secrets/A/versions/1/value", None).await;
    assert_eq!(old["value"], "1");
    let detail = api.ok(Method::GET, &format!("/environments/production/changes/{seq}"), None).await;
    assert_eq!(detail["message"], "launch");
    assert!(!detail.to_string().contains("\"10\""), "a change shows fingerprints, never values: {detail}");

    let r = api.ok(Method::POST, "/environments/production/rollback", Some(json!({ "to": seq }))).await;
    assert_eq!(r["change"]["kind"], "rollback");
    assert_eq!(api.values("production", "/").await["values"], json!({ "A": "10", "B": "2" }));
    // Deleted and restored, a key keeps one history.
    let versions = api.ok(Method::GET, "/environments/production/secrets/A/versions", None).await;
    assert_eq!(versions[0]["version"], 4);
    let changes = api.ok(Method::GET, "/environments/production/changes", None).await;
    assert_eq!(changes.as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn references_imports_and_personal_values_resolve_and_cycles_are_reported() {
    let api = Api::new().await;
    api.set("production", "DB_HOST", "prod.db").await;
    api.ok(Method::PUT, "/environments/development/secrets/PASS?path=/db", Some(json!({ "value": "pw" }))).await;
    api.ok(
        Method::POST,
        "/environments/development/changes",
        Some(json!({ "ops": [
            { "op": "set", "key": "URL", "value": "postgres://u:${development.db.PASS}@${production.DB_HOST}/x" },
            { "op": "set", "key": "LITERAL", "value": "$${HOME}" },
            { "op": "set", "key": "A", "value": "${B}" },
            { "op": "set", "key": "B", "value": "${A}" },
        ] })),
    )
    .await;
    let v = api.values("development", "/").await;
    assert_eq!(v["values"]["URL"], "postgres://u:pw@prod.db/x");
    assert_eq!(v["values"]["LITERAL"], "${HOME}");
    let problems: Vec<&str> = v["problems"].as_array().unwrap().iter().map(|p| p["problem"].as_str().unwrap()).collect();
    assert!(problems.iter().any(|p| p.starts_with("cycle: A → B → A")), "{problems:?}");
    let w = api.ok(Method::PUT, "/environments/development/secrets/X", Some(json!({ "value": "${staging.NOPE}" }))).await;
    assert_eq!(w["warnings"], json!(["X references staging.NOPE, which doesn't exist"]));

    // An import fills in what the folder doesn't set; the folder's own key wins.
    api.set("staging", "SHARED", "from-staging").await;
    api.set("staging", "URL", "staging-url").await;
    api.ok(Method::PUT, "/environments/development/imports?path=/", Some(json!([{ "environment": "staging", "path": "/" }]))).await;
    let v = api.values("development", "/").await;
    assert_eq!((v["values"]["SHARED"].as_str(), v["values"]["URL"].as_str()), (Some("from-staging"), Some("postgres://u:pw@prod.db/x")));
    let s = api.ok(Method::GET, "/environments/development/secrets/SHARED", None).await;
    assert_eq!(s["source"], json!({ "kind": "import", "environment": "staging", "path": "/" }));

    // A personal value replaces the shared one for its owner only.
    api.ok(Method::PUT, "/environments/staging/secrets/SHARED", Some(json!({ "value": "mine", "type": "personal" }))).await;
    assert_eq!(api.values("staging", "/").await["values"]["SHARED"], "mine");
    let token = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ci", "actions": ["read", "reveal"] }))).await;
    let (_, v) = api.req(Method::GET, "/environments/staging/values", None, token["token"].as_str()).await;
    assert_eq!(v["values"]["SHARED"], "from-staging");
}

#[tokio::test]
async fn ephemeral_environments_inherit_their_base_and_go_by_label_or_expiry() {
    let api = Api::new().await;
    api.set("staging", "API", "https://staging").await;
    api.set("staging", "DATABASE_URL", "postgres://staging").await;
    let e = api
        .ok(
            Method::POST,
            "/environments",
            Some(json!({ "slug": "pr-117", "base": "staging", "labels": { "pr": "117" }, "ttl_minutes": 60, "secrets": [{ "op": "set", "key": "DATABASE_URL", "value": "postgres://pr-117" }] })),
        )
        .await;
    assert_eq!((e["kind"].as_str(), e["base"].as_str()), (Some("ephemeral"), Some("staging")));
    assert_eq!(api.values("pr-117", "/").await["values"], json!({ "API": "https://staging", "DATABASE_URL": "postgres://pr-117" }));
    // The base changes; the preview follows.
    api.set("staging", "API", "https://staging-2").await;
    assert_eq!(api.values("pr-117", "/").await["values"]["API"], "https://staging-2");
    let grid = api.ok(Method::GET, "/grid", None).await;
    assert_eq!((grid["environments"].as_array().unwrap().len(), grid["previews"].as_i64()), (3, Some(1)));

    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-118", "base": "staging", "labels": { "pr": "118" } }))).await;
    let gone = api.ok(Method::DELETE, "/environments?label=pr=117", None).await;
    assert_eq!(gone["deleted"], json!(["pr-117"]));
    api.ok(Method::PATCH, "/environments/pr-118", Some(json!({ "expires_at": 1 }))).await;
    assert_eq!(api.secrets.sweep().unwrap().expired, 1);
    assert_eq!(slugs(&api.ok(Method::GET, "/environments", None).await), ["development", "staging", "production"]);
}

#[tokio::test]
async fn the_grid_flags_differences_and_promote_copies_them() {
    let api = Api::new().await;
    for (env, v) in [("development", "a"), ("staging", "a"), ("production", "b")] {
        api.set(env, "SAME_OR_NOT", v).await;
    }
    api.set("development", "NEW", "n").await;
    api.set("development", "OTHER", "o").await;
    // A required key nobody set yet is a row of its own, and reading values reports it.
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "STRIPE_KEY", "comment": "payments" }]))).await;
    let grid = api.ok(Method::GET, "/grid", None).await;
    let status: BTreeMap<&str, &str> = grid["rows"].as_array().unwrap().iter().map(|r| (r["key"].as_str().unwrap(), r["status"].as_str().unwrap())).collect();
    assert_eq!(status, BTreeMap::from([("NEW", "missing"), ("OTHER", "missing"), ("SAME_OR_NOT", "different"), ("STRIPE_KEY", "missing")]));
    assert_eq!(api.values("staging", "/").await["problems"], json!([{ "key": "STRIPE_KEY", "problem": "required, but staging doesn't set it" }]));
    assert!(!grid.to_string().contains("\"a\""), "no values in the grid");

    let cmp = api.ok(Method::GET, "/compare?from=development&to=staging", None).await;
    let rows: Vec<(&str, &str)> = cmp["rows"].as_array().unwrap().iter().map(|r| (r["key"].as_str().unwrap(), r["status"].as_str().unwrap())).collect();
    assert_eq!(rows, [("NEW", "only_from"), ("OTHER", "only_from"), ("SAME_OR_NOT", "same")]);
    let p = api.ok(Method::POST, "/environments/staging/promote?from=development", Some(json!({ "keys": ["NEW"] }))).await;
    assert_eq!(p["change"]["kind"], "promote");
    assert_eq!(api.values("staging", "/").await["values"], json!({ "NEW": "n", "SAME_OR_NOT": "a" }));
}

#[tokio::test]
async fn protected_environments_take_change_requests_that_another_principal_approves() {
    let api = Api::new().await;
    api.set("production", "KEY", "old").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/KEY", Some(json!({ "value": "new" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")));

    let r = api.ok(Method::POST, "/environments/production/requests", Some(json!({ "message": "rotate", "ops": [{ "op": "set", "key": "KEY", "value": "new" }] }))).await;
    let id = r["id"].as_str().unwrap();
    assert_eq!((r["state"].as_str(), r["items"][0]["what"].clone()), (Some("open"), json!(["changed"])));
    assert!(!r.to_string().contains("\"new\""), "a request never shows its values");
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve" }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the author doesn't approve their own request");

    let reviewer = api.ok(Method::POST, "/tokens", Some(json!({ "name": "lead", "actions": ["read", "admin", "write"] }))).await;
    let (s, r) = api.req(Method::POST, &format!("/environments/production/requests/{id}/review"), Some(json!({ "verdict": "approve", "comment": "ok" })), reviewer["token"].as_str()).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")));
    let r = api.ok(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(r["state"], "applied");
    assert_eq!(api.values("production", "/").await["values"]["KEY"], "new");

    // An admin can bypass, and the bypass is in the audit log.
    let r = api.ok(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "KEY", "value": "newer" }] }))).await;
    api.ok(Method::POST, &format!("/environments/production/requests/{}/apply", r["id"].as_str().unwrap()), Some(json!({ "bypass": true }))).await;
    let audit = api.ok(Method::GET, "/audit?action=approval", None).await;
    assert_eq!(audit[0]["action"], "approval.bypass");

    // A promotion into it is a change request too, with the values the browser never had.
    api.set("staging", "KEY", "from-staging").await;
    let p = api.ok(Method::POST, "/environments/production/promote?from=staging", Some(json!({ "keys": ["KEY"] }))).await;
    assert_eq!((p["change"].clone(), p["request"]["state"].as_str()), (Value::Null, Some("open")));
    api.ok(Method::POST, &format!("/environments/production/requests/{}/apply", p["request"]["id"].as_str().unwrap()), Some(json!({ "bypass": true }))).await;
    assert_eq!(api.values("production", "/").await["values"]["KEY"], "from-staging");
}

#[tokio::test]
async fn scoped_tokens_reach_only_their_environments_paths_and_actions() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE?path=/api", Some(json!({ "value": "sk_live" }))).await;
    api.set("production", "ROOT", "r").await;
    let t = api.ok(Method::POST, "/tokens", Some(json!({ "name": "deploy", "environments": ["production"], "paths": ["/api"], "actions": ["read", "reveal"], "ttl_days": 30 }))).await;
    let token = t["token"].as_str().unwrap();
    assert!(token.starts_with("rlst_"));
    let (s, v) = api.req(Method::GET, "/environments/production/values?path=/api", None, Some(token)).await;
    assert_eq!((s, v["values"]["STRIPE"].as_str()), (StatusCode::OK, Some("sk_live")));
    for (method, path, body) in [
        (Method::GET, "/environments/production/values?path=/", None),
        (Method::GET, "/environments/staging/values?path=/api", None),
        (Method::PUT, "/environments/production/secrets/STRIPE?path=/api", Some(json!({ "value": "x" }))),
    ] {
        let (s, _) = api.req(method.clone(), path, body, Some(token)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (_, who) = api.req(Method::GET, "/v1/secrets/whoami", None, Some(token)).await;
    assert_eq!(who["project"].as_str(), Some(api.project.as_str()));
    // Revocation takes effect on the next request.
    api.ok(Method::DELETE, &format!("/tokens/{}", t["id"].as_str().unwrap()), None).await;
    let (s, _) = api.req(Method::GET, "/environments/production/values?path=/api", None, Some(token)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn links_read_live_values_issue_managed_credentials_rotate_and_lease_dynamic_ones() {
    let api = Api::new().await;
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/BUCKET_URL", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "url", "mode": "live" } }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("bucket assets isn't in this project")));
    let place = format!("/v1/workspace/projects/{}/resources/bucket/assets", api.project);
    api.ok(Method::PUT, &place, Some(json!({ "x": 0, "y": 0 }))).await;
    let link = |field: &str, mode: &str| json!({ "kind": "bucket", "ref": "assets", "field": field, "mode": mode });
    api.ok(
        Method::POST,
        "/environments/production/changes",
        Some(json!({ "ops": [
            { "op": "set", "key": "BUCKET_URL", "link": link("url", "live") },
            { "op": "set", "key": "AWS_ACCESS_KEY_ID", "link": link("access_key_id", "managed") },
            { "op": "set", "key": "AWS_SECRET_ACCESS_KEY", "link": link("secret_access_key", "managed") },
        ] })),
    )
    .await;
    // One credential for the pair; the live URL follows the resource.
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!((v["AWS_ACCESS_KEY_ID"].as_str(), v["AWS_SECRET_ACCESS_KEY"].as_str(), v["BUCKET_URL"].as_str()), (Some("AK1"), Some("sk-1"), Some("s3://assets?pw=0")));
    api.ok(Method::POST, "/environments/production/secrets/BUCKET_URL/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["BUCKET_URL"], "s3://assets?pw=1");

    // Rotation issues a new pair as one change; the old key works until its grace ends.
    let r = api.ok(Method::POST, "/environments/production/secrets/AWS_SECRET_ACCESS_KEY/rotate", None).await;
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    let v = api.values("production", "/").await["values"].clone();
    assert_eq!((v["AWS_ACCESS_KEY_ID"].as_str(), v["AWS_SECRET_ACCESS_KEY"].as_str()), (Some("AK2"), Some("sk-2")));
    assert_eq!(*api.fake.live_keys.lock().unwrap(), ["AK1", "AK2"]);
    let leases = api.ok(Method::GET, "/leases", None).await;
    assert_eq!(leases[0]["kind"], "retired");
    api.ok(Method::DELETE, &format!("/leases/{}", leases[0]["id"].as_str().unwrap()), None).await;
    assert_eq!(*api.fake.live_keys.lock().unwrap(), ["AK2"]);

    // Dynamic: every resolve issues a fresh credential under a lease.
    api.ok(Method::PUT, "/environments/staging/secrets/TEMP_KEY", Some(json!({ "link": { "kind": "bucket", "ref": "assets", "field": "access_key_id", "mode": "dynamic", "options": { "ttl_minutes": 1 } } }))).await;
    let a = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "app:web" }))).await;
    let b = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_ne!(a["values"]["TEMP_KEY"], b["values"]["TEMP_KEY"]);
    assert_eq!(a["leases"].as_array().unwrap().len(), 1);
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 3);

    // An environment that goes takes its managed credentials with it.
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "tmp" }))).await;
    api.ok(Method::PUT, "/environments/tmp/secrets/KEY_ID", Some(json!({ "link": link("access_key_id", "managed") }))).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 4);
    api.ok(Method::DELETE, "/environments/tmp", None).await;
    assert_eq!(api.fake.live_keys.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn values_are_sealed_for_services_and_never_appear_in_the_audit_log() {
    let api = Api::new().await;
    let imported = api.ok(Method::POST, "/environments/production/import", Some(json!({ "dotenv": "# stripe\nSTRIPE_KEY=sk_live_123\nexport PORT=8080\n" }))).await;
    assert_eq!(imported["items"].as_array().unwrap().len(), 2);
    let s = api.ok(Method::GET, "/environments/production/secrets/STRIPE_KEY", None).await;
    assert_eq!(s["comment"], "stripe");
    let (_, text) = api.call(Method::GET, "/environments/production/values?format=dotenv", None).await;
    assert_eq!(text, "PORT=8080\nSTRIPE_KEY=sk_live_123\n");

    let sk = [9u8; 32];
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web", "seal_to": roll_secrets::crypto::public_key(&sk) }))).await;
    assert!(!r.to_string().contains("sk_live_123"));
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(r["sealed"].clone()).unwrap();
    let plain = roll_secrets::crypto::open_sealed(&sk, r["context"].as_str().unwrap(), &sealed).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&plain).unwrap()["STRIPE_KEY"], "sk_live_123");

    let audit = api.ok(Method::GET, "/audit", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect();
    assert_eq!(&actions[..3], ["secrets.resolve", "secrets.export", "secrets.write"]);
    assert_eq!(audit[0]["summary"], "resolved 2 values for app:web");
    assert!(!audit.to_string().contains("sk_live_123"));
}

#[tokio::test]
async fn nothing_reaches_an_environment_its_caller_cannot_read() {
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("development", "REF", "${production.DB_PASSWORD}").await;
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-1", "base": "production" }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;

    // Checks and sandboxes resolve as a consumer that may not touch protected environments: not
    // through a reference, an import or an ephemeral environment's base either.
    let check = roll_secrets::Caller::consumer_except("check:acme/web#test", vec!["production".into()]);
    let s = api.secrets.clone();
    let p = api.project.clone();
    let (dev, staging, preview) = tokio::task::spawn_blocking(move || {
        (s.values(&p, "development", "/", true, Some("check"), &check).unwrap(), s.values(&p, "staging", "/", true, Some("check"), &check).unwrap(), s.values(&p, "pr-1", "/", true, Some("check"), &check).unwrap())
    })
    .await
    .unwrap();
    assert_eq!(dev.values["REF"].expose(), "${production.DB_PASSWORD}");
    assert_eq!(dev.problems[0].problem, "production.DB_PASSWORD: no access to production");
    assert!(!staging.values.contains_key("DB_PASSWORD") && !preview.values.contains_key("DB_PASSWORD"));

    // A token for development can't make development an overlay of production, nor link a
    // resource (that's an admin's), nor see the fingerprint of what it can't reveal.
    let t = api.ok(Method::POST, "/tokens", Some(json!({ "name": "dev", "environments": ["development"], "actions": ["read", "reveal", "write", "delete"] }))).await;
    let token = t["token"].as_str();
    let (s, _) = api.req(Method::DELETE, "/environments/development", None, token).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = api.req(Method::POST, "/environments", Some(json!({ "slug": "development", "base": "production" })), token).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let link = json!({ "link": { "kind": "bucket", "ref": "assets", "field": "url", "mode": "live" } });
    let (s, _) = api.req(Method::PUT, "/environments/development/secrets/URL", Some(link), token).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let reader = api.ok(Method::POST, "/tokens", Some(json!({ "name": "read", "actions": ["read"] }))).await;
    let (_, list) = api.req(Method::GET, "/environments/staging/secrets", None, reader["token"].as_str()).await;
    assert_eq!(list[0]["fp"], "");
}

#[tokio::test]
async fn a_stored_value_is_found_by_fingerprint_and_short_values_never_are() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_51Hx9").await;
    api.set("development", "PORT", "8080").await;
    let candidates = ["sk_live_51Hx9", "8080", "something-else"].map(String::from);
    let leaks = api.secrets.leaks(&api.project, &candidates).unwrap();
    assert_eq!(leaks, [roll_secrets::Leak { key: "STRIPE_KEY".into(), environment: "production".into(), path: "/".into() }]);
}

#[tokio::test]
async fn a_shared_value_opens_as_many_times_as_allowed_and_the_server_never_holds_it() {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    let api = Api::new().await;
    // The client encrypts; the key would be in the link's #fragment.
    let (key, nonce) = ([3u8; 32], [5u8; 12]);
    let cipher = aes_gcm::Aes256Gcm::new_from_slice(&key).unwrap();
    let ct = cipher.encrypt(&nonce.into(), Payload { msg: b"hunter2-staging-db", aad: b"roll-share-v1" }).unwrap();
    let blob = B64.encode([&nonce[..], &ct].concat());
    let share = api.ok(Method::POST, "/shares", Some(json!({ "ciphertext": blob, "note": "the staging db", "views": 2, "ttl_minutes": 60 }))).await;
    let id = share["id"].as_str().unwrap();

    // Looking at the link uses nothing up; each open does, and the last one drops it.
    let (_, peek) = api.call(Method::GET, &format!("/v1/secrets/shares/{id}"), None).await;
    assert_eq!((peek["note"].as_str(), peek["views_left"].as_i64()), (Some("the staging db"), Some(2)));
    for _ in 0..2 {
        let (s, opened) = api.call(Method::POST, &format!("/v1/secrets/shares/{id}/open"), None).await;
        assert_eq!(s, StatusCode::OK);
        let raw = B64.decode(opened["ciphertext"].as_str().unwrap()).unwrap();
        let plain = cipher.decrypt(raw[..12].into(), Payload { msg: &raw[12..], aad: b"roll-share-v1" }).unwrap();
        assert_eq!(plain, b"hunter2-staging-db");
    }
    let (s, _) = api.call(Method::POST, &format!("/v1/secrets/shares/{id}/open"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let list = api.ok(Method::GET, "/shares", None).await;
    assert_eq!((list[0]["open"].as_bool(), list[0]["views_left"].as_i64()), (Some(false), Some(0)));
    let audit = api.ok(Method::GET, "/audit?action=share", None).await;
    assert_eq!(audit.as_array().unwrap().len(), 3, "created and opened twice");
}
