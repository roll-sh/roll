//! `/v1/secrets` in process, on SQLite in a tempdir (docs/secrets.md), next to the workspace
//! router it reads projects from. One test per flow a person relies on.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use roll_secrets::links::{Field, Issued, Link, LinkProvider, Links, Mode};
use roll_secrets::syncs::{Push, Resources, SyncField, SyncTarget, Syncs};
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
    fn suggested(&self) -> &[(&'static str, Mode)] {
        &[("url", Mode::Live), ("access_key_id", Mode::Managed), ("secret_access_key", Mode::Managed)]
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

/// A push a fake platform got: its values and credentials (`credential:<key>`), exposed;
/// changed and removed names.
type Pushed = (BTreeMap<String, String>, Vec<String>, Vec<String>);

/// A fake outside platform: the pushes it got.
#[derive(Default)]
struct FakeSync {
    pushes: Mutex<Vec<Pushed>>,
}

impl SyncTarget for FakeSync {
    fn kind(&self) -> &str {
        "fake"
    }
    fn label(&self) -> &str {
        "Fake"
    }
    fn fields(&self) -> &[SyncField] {
        &[]
    }
    fn signed(&self) -> bool {
        true
    }
    fn credentials(&self, target: &Value) -> Vec<String> {
        target["token_key"].as_str().map(String::from).into_iter().collect()
    }
    fn check(&self, _: &str, _: &Resources, target: &Value) -> roll_secrets::Result<()> {
        target.get("name").map(|_| ()).ok_or_else(|| roll_secrets::Error::BadRequest("target needs a name".into()))
    }
    fn push(&self, _: &Value, p: &Push<'_>) -> roll_secrets::Result<()> {
        let credentials = p.credentials.iter().map(|(k, v)| (format!("credential:{k}"), v.expose().to_string()));
        let values = p.values.iter().map(|(k, v)| (k.clone(), v.expose().to_string())).chain(credentials).collect();
        self.pushes.lock().unwrap().push((values, p.changed.to_vec(), p.removed.to_vec()));
        Ok(())
    }
}

/// Audit deliveries, kept instead of sent; and an OIDC issuer's keys, served.
#[derive(Default)]
struct Sent(Mutex<Vec<Value>>, Mutex<Option<Value>>, std::sync::atomic::AtomicBool);

impl roll_secrets::Deliver for Sent {
    fn check(&self, url: &str) -> roll_secrets::Result<()> {
        if url.starts_with("https://") { Ok(()) } else { Err(roll_secrets::Error::BadRequest("url is a public https:// URL".into())) }
    }
    fn post(&self, _: &str, _: &str, _: &Secret, body: &Value) -> roll_secrets::Result<()> {
        self.0.lock().unwrap().push(body.clone());
        Ok(())
    }
    fn get(&self, url: &str, _: usize) -> roll_secrets::Result<Vec<u8>> {
        let doc = match url.strip_prefix(ISSUER) {
            Some("/.well-known/openid-configuration") => json!({ "issuer": ISSUER, "jwks_uri": format!("{ISSUER}/keys") }),
            Some("/keys") => self.1.lock().unwrap().clone().unwrap_or(json!({ "keys": [] })),
            _ => return Err(roll_secrets::Error::Link(format!("{url}: 404"))),
        };
        Ok(doc.to_string().into_bytes())
    }
    /// A rotator outside roll: each issue is a new vendor key.
    fn call(&self, _: &str, _: &str, _: &Secret, body: &Value, _: usize) -> roll_secrets::Result<zeroize::Zeroizing<Vec<u8>>> {
        if self.2.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(roll_secrets::Error::Link("rotator.example: connection refused".into()));
        }
        let mut sent = self.0.lock().unwrap();
        sent.push(body.clone());
        let n = sent.iter().filter(|b| b["action"] == "issue").count();
        let answer = if body["action"] == "issue" { json!({ "value": format!("vendor-key-{n}"), "handle": n }) } else { json!({}) };
        Ok(answer.to_string().into_bytes().into())
    }
}

const ISSUER: &str = "https://token.actions.githubusercontent.com";

/// An ES256 key of the fake issuer, its JWKS, and tokens it signs.
struct Issuer(ring::signature::EcdsaKeyPair);

impl Issuer {
    fn new() -> Issuer {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        Issuer(EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap())
    }
    fn jwks(&self) -> Value {
        use ring::signature::KeyPair;
        let p = self.0.public_key().as_ref();
        json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "x": b64(&p[1..33]), "y": b64(&p[33..65]) }] })
    }
    fn sign(&self, claims: Value) -> String {
        let signed = format!("{}.{}", b64(json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let sig = self.0.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).unwrap();
        format!("{signed}.{}", b64(sig.as_ref()))
    }
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// People's accounts: `ada` until disabled, and `root`, an admin.
#[derive(Default)]
struct People {
    disabled: std::sync::Mutex<bool>,
}

impl roll_secrets::People for People {
    fn person(&self, user: &str) -> Result<Option<roll_secrets::Person>, String> {
        Ok(match user {
            "usr_ada" if !*self.disabled.lock().unwrap() => Some(roll_secrets::Person { admin: false, code_owner: Some("ada".into()) }),
            "usr_root" => Some(roll_secrets::Person { admin: true, code_owner: None }),
            _ => None,
        })
    }
}

/// As rolld answers: an admin's canvas stands for anything, a person's for their own repos.
impl roll_workspace::Owners for People {
    fn trusts(&self, user: &str, kind: roll_workspace::Kind, r#ref: &str) -> bool {
        match roll_secrets::People::person(self, user) {
            Ok(Some(p)) => p.admin || (kind == roll_workspace::Kind::Repo && r#ref.starts_with(&format!("{}/", p.code_owner.unwrap_or_default()))),
            _ => false,
        }
    }
}

struct Api {
    sent: Arc<Sent>,
    people: Arc<People>,
    router: Router,
    secrets: Arc<Secrets>,
    db: code_sql::Db,
    fake: Arc<Fake>,
    sync: Arc<FakeSync>,
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
        let sync = Arc::new(FakeSync::default());
        let mut syncs = Syncs::default();
        syncs.register(sync.clone());
        let sent = Arc::new(Sent::default());
        let people = Arc::new(People::default());
        ws.set_owners(people.clone());
        let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, syncs, open: true, trust_forwarded_for: false, web_url: None, deliver: Some(sent.clone()), grants: None, people: Some(people.clone()) };
        let secrets = Arc::new(Secrets::new(db.clone(), ws.clone(), opts).unwrap());
        let router = roll_workspace::router(ws).merge(roll_secrets::router(secrets.clone()));
        let mut api = Api { sent, people, router, secrets, db, fake, sync, project: String::new(), _dir: dir };
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

    /// As `usr_ada`, signed in and not an admin (the gate put her session on the request).
    async fn as_ada(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.signed_in("ada", false, method, path, body).await
    }

    async fn signed_in(&self, handle: &str, admin: bool, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path).header("content-type", "application/json").body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        req.extensions_mut().insert(roll_auth::SignedIn { user: format!("usr_{handle}"), handle: handle.into(), admin, code_owner: None, session: format!("ses_{handle}") });
        if admin {
            req.extensions_mut().insert(roll_auth::Operator);
        }
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
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

    // What production had then, without rolling back.
    let then = api.ok(Method::GET, &format!("/environments/production/values?seq={seq}"), None).await;
    assert_eq!((then["values"].clone(), then["seq"].as_i64()), (json!({ "A": "10", "B": "2" }), Some(seq)));
    let r = api.ok(Method::POST, "/environments/production/rollback", Some(json!({ "to": seq }))).await;
    assert_eq!(r["change"]["kind"], "rollback");
    assert_eq!(api.values("production", "/").await["values"], json!({ "A": "10", "B": "2" }));
    // A local copy against it, by fingerprint: names only, never a value.
    let d = api.ok(Method::POST, "/environments/production/diff", Some(json!({ "dotenv": "A=10\nB=stale\nNEW=x\n" }))).await;
    assert_eq!((d["same"].clone(), d["changed"].clone(), d["added"].clone(), d["missing"].clone()), (json!(["A"]), json!(["B"]), json!(["NEW"]), json!([])));
    assert!(!d.to_string().contains("stale"), "{d}");
        // Deleted and restored, a key keeps one history.
    let versions = api.ok(Method::GET, "/environments/production/secrets/A/versions", None).await;
    assert_eq!(versions[0]["version"], 4);
    let changes = api.ok(Method::GET, "/environments/production/changes", None).await;
    assert_eq!(changes.as_array().unwrap().len(), 4);
    // After a leak: older values are purged for good; history keeps the rows, a rollback says so.
    let p = api.ok(Method::POST, "/environments/production/secrets/A/purge", None).await;
    assert_eq!(p["purged"], 2);
    let (s, _) = api.call(Method::GET, "/environments/production/secrets/A/versions/1/value", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let r = api.ok(Method::POST, "/environments/production/rollback", Some(json!({ "to": 1 }))).await;
    assert_eq!(r["warnings"], json!(["A's value at #1 was purged: it stays as it is now"]));
    // A retention purges only what's older than it, never a value in use.
    api.ok(Method::PATCH, "", Some(json!({ "history_days": 1 }))).await;
    let sec = api.secrets.clone();
    tokio::task::spawn_blocking(move || sec.sweep().unwrap()).await.unwrap();
    assert_eq!(api.values("production", "/").await["values"]["A"], "10");
    assert_eq!(api.ok(Method::GET, "", None).await["history_days"], 1);
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
    // What a reference reaches moves the digest (roll run --watch, syncs), in another environment too.
    let before = api.ok(Method::GET, "/environments/development/digest", None).await["digest"].clone();
    api.set("production", "DB_HOST", "prod2.db").await;
    assert_ne!(api.ok(Method::GET, "/environments/development/digest", None).await["digest"], before);
    api.set("production", "DB_HOST", "prod.db").await;
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

    // Ensuring one: created, then unchanged, then brought up to date in one change.
    let want = |db: &str, extra: bool| {
        let mut ops = vec![json!({ "op": "set", "key": "DATABASE_URL", "value": db })];
        if extra {
            ops.push(json!({ "op": "set", "key": "EXTRA", "value": "x" }));
        }
        json!({ "base": "staging", "labels": { "preview": "pv-1" }, "ttl_minutes": 60, "secrets": ops, "replace": true })
    };
    let (s, first) = api.call(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1", true))).await;
    assert_eq!((s, first["created"].as_bool()), (StatusCode::CREATED, Some(true)));
    let again = api.ok(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1", true))).await;
    assert_eq!((again["created"].as_bool(), again["change"].clone()), (Some(false), Value::Null), "ensuring what's there is no change");
    let moved = api.ok(Method::PUT, "/environments/pv-1", Some(want("postgres://pv-1b", false))).await;
    assert_eq!(moved["change"]["keys"], 2, "one change: the new value, and EXTRA gone");
    assert_eq!(api.values("pv-1", "/").await["values"], json!({ "API": "https://staging-2", "DATABASE_URL": "postgres://pv-1b" }));
    let (s, e) = api.call(Method::PUT, "/environments/pv-1", Some(json!({ "base": "production" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("mismatch")));
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

    // Typed keys: a value that doesn't fit is refused by key (never echoed); one that only fits
    // once resolved is checked then, so roll run won't start with it.
    let (s, _) = api.call(Method::PUT, "/required", Some(json!([{ "key": "PORT", "type": "port" }, { "key": "REGION", "pattern": "[a-z]{2}-[a-z]+-[0-9" }]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a pattern that doesn't compile");
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "PORT", "type": "port" }, { "key": "REGION", "pattern": "[a-z]{2}-[a-z]+-[0-9]" }]))).await;
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/PORT", Some(json!({ "value": "eighty-eighty" }))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("PORT must be a port (1–65535)")));
    let (s, _) = api.call(Method::POST, "/environments/staging/import", Some(json!({ "dotenv": "REGION=Europe\n" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.set("staging", "PORT", "8080").await;
    api.set("staging", "REGION", "eu-west-1").await;
    api.set("development", "WRONG", "not a port").await;
    api.set("development", "PORT", "${WRONG}").await;
    let problems = api.values("development", "/").await["problems"].clone();
    assert!(problems.as_array().unwrap().iter().any(|p| p["key"] == "PORT" && p["problem"] == "PORT must be a port (1–65535)"), "{problems}");
    // A type added after the values: the grid says which don't fit, to whoever may reveal them.
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "SAME_OR_NOT", "type": "int" }]))).await;
    let grid = api.ok(Method::GET, "/grid", None).await;
    let row = grid["rows"].as_array().unwrap().iter().find(|r| r["key"] == "SAME_OR_NOT").unwrap().clone();
    assert_eq!(row["cells"]["production"]["misfit"], "SAME_OR_NOT must be an integer");
}

#[tokio::test]
async fn protected_environments_take_change_requests_that_another_principal_approves() {
    let api = Api::new().await;
    api.set("production", "KEY", "old").await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/KEY", Some(json!({ "value": "new" }))).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("approval_required")));

    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1, "notify_url": "https://hooks.example/T0/B0/secret-path" }))).await;
    let r = api.ok(Method::POST, "/environments/production/requests", Some(json!({ "message": "rotate", "ops": [{ "op": "set", "key": "KEY", "value": "new" }] }))).await;
    let id = r["id"].as_str().unwrap();
    assert_eq!((r["state"].as_str(), r["items"][0]["what"].clone()), (Some("open"), json!(["changed"])));
    assert!(!r.to_string().contains("\"new\""), "a request never shows its values");
    let (s, _) = api.call(Method::POST, &format!("/environments/production/requests/{id}/apply"), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // Its approvers hear about it, by key name only, from the sweeper; the environment shows the
    // URL's host, never the URL.
    let sec = api.secrets.clone();
    tokio::task::spawn_blocking(move || sec.sweep().unwrap()).await.unwrap();
    let notice = api.sent.0.lock().unwrap().iter().find(|b| b["request"]["id"] == id).cloned().unwrap();
    assert_eq!(notice["request"]["keys"], json!(["KEY"]));
    assert!(notice["text"].as_str().unwrap().contains("opened change request") && !notice.to_string().contains("\"new\""));
    let env = api.ok(Method::GET, "/environments/production", None).await;
    assert_eq!(env["notify"], "hooks.example");
    // An approver who may reveal sees what changes, the value now beside the one proposed.
    let v = api.ok(Method::GET, &format!("/environments/production/requests/{id}/values"), None).await;
    assert_eq!((v[0]["key"].as_str(), v[0]["now"].as_str(), v[0]["proposed"].as_str()), (Some("KEY"), Some("old"), Some("new")));
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
    // A token pinned to addresses works only from them (checked on every request).
    let pinned = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ci", "actions": ["read"], "allowed_ips": ["203.0.113.0/24"] }))).await;
    assert_eq!(pinned["allowed_ips"], json!(["203.0.113.0/24"]));
    let (s, _) = api.req(Method::GET, "/environments", None, pinned["token"].as_str()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "from an address outside its list");
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
    // On the canvas and linked nowhere: offered for every environment, under its usual keys.
    let offered = api.ok(Method::GET, "/suggestions", None).await;
    assert_eq!((offered[0]["ref"].as_str(), offered[0]["environments"].clone()), (Some("assets"), json!(["development", "staging", "production"])));
    assert_eq!(offered[0]["links"][1], json!({ "key": "AWS_ACCESS_KEY_ID", "field": "access_key_id", "mode": "managed" }));
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
    assert_eq!(api.ok(Method::GET, "/suggestions", None).await[0]["environments"], json!(["development", "staging"]), "linked in production now");
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
    // A long-running service renews its lease instead of restarting.
    let lease = a["leases"][0].as_str().unwrap();
    let before = api.ok(Method::GET, "/leases", None).await.as_array().unwrap().iter().find(|l| l["id"] == lease).unwrap()["expires_at"].as_i64().unwrap();
    let renewed = api.ok(Method::POST, &format!("/leases/{lease}/renew"), Some(json!({ "ttl_minutes": 120 }))).await;
    assert!(renewed["expires_at"].as_i64().unwrap() > before + 60 * 60_000);
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

    // Other managers' JSON exports import too.
    let j = api.ok(Method::POST, "/environments/staging/import", Some(json!({ "json": [{ "key": "FROM_DOPPLER", "value": "d" }, { "key": "PORT", "value": 8080 }] }))).await;
    assert_eq!(j["items"].as_array().unwrap().len(), 2);
        // `replace` makes the folder match the file, deletions included, in one change.
    let r = api.ok(Method::POST, "/environments/production/import", Some(json!({ "dotenv": "STRIPE_KEY=sk_live_123\n", "replace": true }))).await;
    assert_eq!((r["items"][0]["key"].as_str(), r["items"][0]["what"][0].as_str()), (Some("PORT"), Some("deleted")));
}

#[tokio::test]
async fn nothing_reaches_an_environment_its_caller_cannot_read() {
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("development", "REF", "${production.DB_PASSWORD}").await;
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(json!([{ "environment": "production", "path": "/" }]))).await;
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-1", "base": "production" }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;

    // Checks, sandboxes and previews resolve as untrusted consumers, which may not touch protected
    // environments: not through a reference, an import or an ephemeral environment's base either.
    let body = || Some(json!({ "consumer": "check:acme/web#test", "untrusted": true }));
    let dev = api.ok(Method::POST, "/environments/development/resolve", body()).await;
    let staging = api.ok(Method::POST, "/environments/staging/resolve", body()).await;
    let preview = api.ok(Method::POST, "/environments/pr-1/resolve", body()).await;
    assert_eq!(dev["values"]["REF"], "${production.DB_PASSWORD}");
    assert_eq!(dev["problems"][0]["problem"], "production.DB_PASSWORD: no access to production");
    assert!(staging["values"].get("DB_PASSWORD").is_none() && preview["values"].get("DB_PASSWORD").is_none());
    let (s, _) = api.call(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "check:x", "untrusted": true }))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // A preview's environment is untrusted itself: whoever reads it (an app's deploy as the
    // operator here) gets nothing from a protected base, unless it was made trusted.
    assert!(api.values("pr-1", "/").await["values"].get("DB_PASSWORD").is_none());
    api.ok(Method::POST, "/environments", Some(json!({ "slug": "pr-2", "base": "production", "trusted": true }))).await;
    assert_eq!(api.values("pr-2", "/").await["values"]["DB_PASSWORD"], "prod-pw");

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

#[tokio::test]
async fn a_sync_pushes_each_change_once_removes_deleted_keys_and_is_audited() {
    let api = Api::new().await;
    api.set("staging", "DB_URL", "postgres://db").await;
    api.set("staging", "STRIPE_KEY", "sk_live_123").await;
    api.set("staging", "PLATFORM_TOKEN", "tok_platform_1").await;
    // Before it exists: what it would push (names only), its credential kept out.
    let p = api.ok(Method::POST, "/syncs/preview", Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" }, "keys": ["DB_URL", "STRIPE_KEY", "TYPO"] }))).await;
    assert_eq!(p, json!({ "names": ["DB_URL", "STRIPE_KEY"], "credentials": ["PLATFORM_TOKEN"], "missing_credentials": [], "unknown_keys": ["TYPO"] }));
    let s = api.ok(Method::POST, "/syncs", Some(json!({ "environment": "staging", "kind": "fake", "target": { "name": "x", "token_key": "PLATFORM_TOKEN" } }))).await;
    let id = s["id"].as_str().unwrap().to_string();
    assert!(s["signing_secret"].as_str().unwrap().starts_with("whsec_"), "shown once: {s}");
    let pushes = |n: usize| api.sync.pushes.lock().unwrap()[n].clone();

    api.secrets.sweep().unwrap();
    let (values, changed, removed) = pushes(0);
    assert_eq!(values["STRIPE_KEY"], "sk_live_123");
    assert_eq!(values["credential:PLATFORM_TOKEN"], "tok_platform_1", "the platform's token, resolved at push time");
    assert!(!values.contains_key("PLATFORM_TOKEN"), "and never pushed itself");
    assert_eq!((changed, removed.len()), (vec!["DB_URL".to_string(), "STRIPE_KEY".into()], 0));
    api.secrets.sweep().unwrap();
    assert_eq!(api.sync.pushes.lock().unwrap().len(), 1, "an unchanged environment isn't pushed again");

    api.set("staging", "DB_URL", "postgres://db2").await;
    api.ok(Method::DELETE, "/environments/staging/secrets/STRIPE_KEY", None).await;
    api.secrets.sweep().unwrap();
    let (values, changed, removed) = pushes(1);
    assert_eq!((values.len(), changed, removed), (2, vec!["DB_URL".to_string()], vec!["STRIPE_KEY".to_string()]));

    let listed = api.ok(Method::GET, "/syncs", None).await;
    assert_eq!((listed["syncs"][0]["status"].as_str(), listed["syncs"][0]["synced"].as_u64()), (Some("ok"), Some(1)));
    assert!(listed["syncs"][0].get("signing_secret").is_none());
    let audit = api.ok(Method::GET, &format!("/audit?actor=sync:{id}"), None).await;
    assert_eq!(audit[0]["action"], "sync.push");
    assert_eq!(audit[0]["keys"], json!(["DB_URL", "STRIPE_KEY"]));
    assert_eq!(audit.as_array().unwrap().len(), 2);
    assert!(!audit.to_string().contains("sk_live_123") && !audit.to_string().contains("postgres://"));

    // A change that only comes through a reference is pushed too.
    api.set("production", "HOST", "db.one").await;
    api.set("staging", "REF", "${production.HOST}/x").await;
    api.secrets.sweep().unwrap();
    api.set("production", "HOST", "db.two").await;
    api.secrets.sweep().unwrap();
    let (values, changed, _) = pushes(3);
    assert_eq!((values["REF"].as_str(), changed), ("db.two/x", vec!["REF".to_string()]));
}

#[tokio::test]
async fn the_audit_log_exports_and_streams_to_a_sink_without_values() {
    let api = Api::new().await;
    api.set("production", "BEFORE", "x-secret-before").await;
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "http://10.0.0.1/hook" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let sink = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "https://siem.example/roll" }))).await;
    assert!(sink["secret"].as_str().unwrap().starts_with("whsec_"));
    // From now on: rows after the sink was made, once each, in order.
    api.set("production", "STRIPE_KEY", "sk_live_never_sent").await;
    api.values("production", "/").await;
    // Rows go out once they've settled (a late commit can't be skipped).
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert!(api.sent.0.lock().unwrap().is_empty());
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || {
        s.sweep().unwrap();
        s.sweep().unwrap();
    })
    .await
    .unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one batch, not delivered twice");
    let actions: Vec<&str> = sent[0]["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["sink.create", "secrets.write", "secrets.export"]);
    assert!(!serde_json::to_string(&sent).unwrap().contains("sk_live_never_sent"));
    // Or a roll bucket: of the project's own canvas only.
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "bucket:audit-archive" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let p = api.project.clone();
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/bucket/audit-archive"), Some(json!({ "x": 0, "y": 0 }))).await;
    let bucket = api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "bucket:audit-archive" }))).await;
    assert_eq!(bucket["url"], "bucket:audit-archive");
    // A topic of a stream on the canvas, the same way.
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/audit" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/stream/st_x"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, _) = api.call(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/bad topic" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    api.ok(Method::POST, "/audit/sinks", Some(json!({ "url": "stream:st_x/audit" }))).await;
    // Taken off the canvas later, it gets nothing more: checked at every delivery.
    api.ok(Method::DELETE, &format!("/v1/workspace/projects/{p}/resources/bucket/audit-archive"), None).await;
    api.set("production", "AFTER", "x").await;
    api.db.write(|c| c.execute("UPDATE audit SET at = at - 60000", ())).unwrap();
    let before = api.sent.0.lock().unwrap().len();
    let sw = api.secrets.clone();
    tokio::task::spawn_blocking(move || sw.sweep().unwrap()).await.unwrap();
    let sinks = api.ok(Method::GET, "/audit/sinks", None).await;
    let b = sinks.as_array().unwrap().iter().find(|x| x["id"] == bucket["id"]).unwrap();
    assert_eq!((b["status"].as_str(), b["error"].as_str().is_some_and(|e| e.contains("isn't on this project's canvas"))), (Some("error"), true), "{b}");
    assert_eq!(api.sent.0.lock().unwrap().iter().skip(before).filter(|x| x["sink"] == bucket["id"]).count(), 0);

    let (_, csv) = api.call(Method::GET, "/audit?format=csv", None).await;
    let csv = csv.as_str().unwrap();
    assert!(csv.starts_with("id,at,actor,action,") && csv.contains("\"secrets.export\"") && !csv.contains("sk_live"));
}

#[tokio::test]
async fn insights_show_when_each_key_was_last_read_and_changed() {
    let api = Api::new().await;
    api.set("production", "USED", "u").await;
    api.ok(Method::PUT, "/environments/production/secrets/IDLE?path=/old", Some(json!({ "value": "i" }))).await;
    api.values("production", "/").await;
    api.ok(Method::PATCH, "/environments/production", Some(json!({ "max_age_days": 90 }))).await;
    // A vendor's key that stops working in three days: flagged, and its date outlives a new value.
    let soon = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64 + 3 * 86_400_000;
    api.ok(Method::PUT, "/environments/production/secrets/CERT", Some(json!({ "value": "c1", "expires_at": soon }))).await;
    let kept = api.ok(Method::PUT, "/environments/production/secrets/CERT", Some(json!({ "value": "c2" }))).await;
    assert_eq!(kept["secret"]["expires_at"].as_i64(), Some(soon));
    let i = api.ok(Method::GET, "/environments/production/insights", None).await;
    assert_eq!(i["max_age_days"], 90);
    let cert = &i["keys"][0];
    assert_eq!((cert["key"].as_str(), cert["expiring"].as_bool()), (Some("CERT"), Some(true)));
    let used = &i["keys"][1];
    assert_eq!((used["key"].as_str(), used["reads_30d"].as_i64(), used["last_read_by"].as_str(), used["stale"].as_bool()), (Some("USED"), Some(1), Some("operator"), Some(false)));
    let idle = api.ok(Method::GET, "/environments/production/insights?path=/old", None).await;
    assert_eq!((idle["keys"][0]["key"].as_str(), idle["keys"][0]["last_read_at"].clone()), (Some("IDLE"), Value::Null));

    // An environment with a limit and somewhere to say so is reminded of its stale keys, once a day.
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    // Production's value pasted into staging, where more people and less vetted code read it.
    api.set("staging", "USED", "u").await;
    let i = api.ok(Method::GET, "/environments/production/insights", None).await;
    assert_eq!((i["keys"][1]["key"].as_str(), i["keys"][1]["exposed_in"].clone(), i["keys"][0]["exposed_in"].clone()), (Some("USED"), json!(["staging"]), json!([])));
    api.db.write(|c| c.execute("UPDATE secrets SET updated_at = updated_at - 8640000000 WHERE key = 'USED'", ())).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || {
        s.sweep().unwrap();
        s.sweep().unwrap();
    })
    .await
    .unwrap();
    let sent = api.sent.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one reminder a day");
    let findings: Vec<(&str, &str)> = sent[0]["findings"].as_array().unwrap().iter().map(|f| (f["issue"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    // Every finding there, worst first: USED's value is in staging too.
    assert_eq!(findings, [("exposed", "USED"), ("expiring", "CERT"), ("stale", "USED")]);
    assert!(sent[0]["text"].as_str().unwrap().starts_with("roll secrets: production has 1 exposed: USED (also in staging); 1 expiring: CERT (expires "), "{}", sent[0]["text"]);
    assert!(!sent[0].to_string().contains("\"u\"") && !sent[0].to_string().contains("\"c2\""), "names, never values");

    // A credential anyone could guess is flagged, to whoever may reveal it.
    api.set("staging", "DB_PASSWORD", "Password1").await;
    let weak = |v: &Value| v["keys"].as_array().unwrap().iter().find(|k| k["key"] == "DB_PASSWORD").map(|k| k["weak"].clone());
    let i = api.ok(Method::GET, "/environments/staging/insights", None).await;
    assert_eq!(weak(&i), Some(json!("a common password")));
    let token = api.ok(Method::POST, "/tokens", Some(json!({ "name": "lint", "actions": ["read"] }))).await;
    let (_, i) = api.req(Method::GET, "/environments/staging/insights", None, token["token"].as_str()).await;
    assert_eq!(weak(&i), Some(Value::Null));
}

#[tokio::test]
async fn health_lists_every_finding_in_every_environment_worst_first() {
    let api = Api::new().await;
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "DB_PASSWORD", "value": "Password1" }, { "op": "set", "key": "CERT", "value": "c", "expires_at": 1000 }] }))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;
    api.set("staging", "DB_PASSWORD", "Password1").await;
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "CERT", "owner": "@payments" }]))).await;
    let found = api.ok(Method::GET, "/health", None).await;
    let got: Vec<(&str, &str, &str)> = found.as_array().unwrap().iter().map(|f| (f["issue"].as_str().unwrap(), f["environment"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    assert_eq!(
        got,
        [
            ("expired", "production", "CERT"),
            ("missing", "development", "CERT"),
            ("missing", "staging", "CERT"),
            ("exposed", "production", "DB_PASSWORD"),
            ("weak", "staging", "DB_PASSWORD"),
            ("weak", "production", "DB_PASSWORD"),
        ]
    );
    assert_eq!(found[3]["detail"], "also in staging");
    // A required key's owner is who to ask.
    assert_eq!((found[0]["owner"].as_str(), found[1]["owner"].as_str(), found[3]["owner"].as_str()), (Some("@payments"), Some("@payments"), None));
    // It filters itself (roll-access lets environment-limited tokens in on that): a token for
    // staging hears only of staging.
    let t = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ci", "environments": ["staging"], "actions": ["read", "reveal"] }))).await;
    let (_, mine) = api.req(Method::GET, "/health", None, t["token"].as_str()).await;
    assert!(mine.as_array().unwrap().iter().all(|f| f["environment"] == "staging") && !mine.as_array().unwrap().is_empty(), "{mine}");
    // Keys no repository on the canvas mentions, when asked (roll code's search is a fake here).
    struct Code;
    impl roll_secrets::health::CodeRefs for Code {
        fn referenced(&self, repo: &str, keys: &[String]) -> Option<std::collections::BTreeSet<String>> {
            (repo == "acme/web").then(|| keys.iter().filter(|k| *k == "CERT").cloned().collect())
        }
    }
    api.secrets.set_code_refs(Arc::new(Code));
    let p = api.project.clone();
    api.ok(Method::PUT, &format!("/v1/workspace/projects/{p}/resources/repo/acme%2Fweb"), Some(json!({ "x": 0, "y": 0 }))).await;
    let with_code = api.ok(Method::GET, "/health?code=1", None).await;
    let unreferenced: Vec<(&str, &str)> = with_code.as_array().unwrap().iter().filter(|f| f["issue"] == "unreferenced").map(|f| (f["environment"].as_str().unwrap(), f["key"].as_str().unwrap())).collect();
    assert_eq!(unreferenced, [("staging", "DB_PASSWORD"), ("production", "DB_PASSWORD")]);
    // It says something about the repository's contents: never to a caller who can't read its code.
    let (s, _) = api.req(Method::GET, "/health?code=1", None, t["token"].as_str()).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Across projects: this one, with its findings.
    let all = api.ok(Method::GET, "/v1/secrets/findings", None).await;
    assert_eq!((all.as_array().map(Vec::len), all[0]["project"].as_str(), all[0]["findings"].as_array().map(Vec::len)), (Some(1), Some(api.project.as_str()), Some(6)));
}

#[tokio::test]
async fn a_scan_finds_values_only_its_caller_may_reveal() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_51Hx9Q2abc").await;
    let text = json!({ "candidates": ["hello world", "sk_live_51Hx9Q2abc"] });
    let found = api.ok(Method::POST, "/leaks", Some(text.clone())).await;
    assert_eq!(found, json!([{ "index": 1, "key": "STRIPE_KEY", "environment": "production", "path": "/" }]));
    // Without reveal, a scan would be an oracle for guessing values: it finds nothing.
    let token = api.ok(Method::POST, "/tokens", Some(json!({ "name": "lint", "actions": ["read"] }))).await;
    let (s, v) = api.req(Method::POST, "/leaks", Some(text), token["token"].as_str()).await;
    assert_eq!((s, v), (StatusCode::OK, json!([])));
    // Nor someone else's personal value, even to a caller who may reveal the folder: only its owner
    // ever sees it.
    api.ok(Method::PUT, "/environments/production/secrets/DB_PASSWORD", Some(json!({ "value": "mine-only-7f3k2", "type": "personal" }))).await;
    let mine = json!({ "candidates": ["mine-only-7f3k2"] });
    let reveal = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ci", "actions": ["read", "reveal"] }))).await;
    let (_, v) = api.req(Method::POST, "/leaks", Some(mine.clone()), reveal["token"].as_str()).await;
    assert_eq!(v, json!([]));
    assert_eq!(api.ok(Method::POST, "/leaks", Some(mine)).await[0]["key"], "DB_PASSWORD", "its owner finds it");
}

#[tokio::test]
async fn a_rotator_outside_roll_issues_and_rotates_a_keys_value() {
    let api = Api::new().await;
    let r = api.ok(Method::POST, "/rotators", Some(json!({ "name": "stripe", "url": "https://rotator.example/stripe?token=t" }))).await;
    assert!(r["secret"].as_str().unwrap().starts_with("whsec_") && r["host"] == "rotator.example", "{r}");
    let link = json!({ "kind": "rotator", "ref": r["id"], "field": "value", "mode": "managed" });
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE_KEY", Some(json!({ "link": link }))).await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-1");
    api.ok(Method::POST, "/environments/production/secrets/STRIPE_KEY/rotate", None).await;
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "vendor-key-2");
    // The key it issues keeps it.
    let (s, _) = api.call(Method::DELETE, &format!("/rotators/{}", r["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // The project goes while the rotator is down: what it issued is still revoked, by the
    // sweeper, which keeps the rotator (and the project's key) until then.
    api.sent.2.store(true, std::sync::atomic::Ordering::Relaxed);
    let (s, p) = (api.secrets.clone(), api.project.clone());
    tokio::task::spawn_blocking(move || s.forget_project(&p).unwrap()).await.unwrap();
    let count = |table: &str| {
        let (sql, p) = (format!("SELECT COUNT(*) FROM {table} WHERE project_id = ?"), api.project.clone());
        api.db.read(move |c| c.query_row(&sql, [p.as_str()], |r| r.get::<_, i64>(0))).unwrap()
    };
    assert_eq!((count("rotators"), count("projects"), count("secrets")), (1, 1, 0));
    api.sent.2.store(false, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    let revoked: Vec<Value> = api.sent.0.lock().unwrap().iter().filter(|b| b["action"] == "revoke").map(|b| b["handle"].clone()).collect();
    assert!(revoked.contains(&json!(2)), "the key in use when the project went is revoked: {revoked:?}");
    assert_eq!((count("rotators"), count("projects"), count("leases")), (0, 0, 0));
}

#[tokio::test]
async fn reads_unlike_the_reader_are_alerted() {
    let api = Api::new().await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "notify_url": "https://hooks.example/T0/B0/x" }))).await;
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    sweep().await;
    // Reads as the audit log records them: a CI token from its usual address, then another; a
    // person revealing values one by one.
    let read = |actor: &str, action: &str, ip: &str| {
        let (p, actor, action, ip) = (api.project.clone(), actor.to_string(), action.to_string(), ip.to_string());
        api.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO audit (project_id, at, actor, action, env, path, keys, summary, ip) VALUES (?, (SELECT COALESCE(MAX(at), 0) + 1 FROM audit), ?, ?, 'production', '/', '[\"API_KEY\"]', '', ?)",
                    code_sql::params![p, actor, action, ip],
                )
            })
            .unwrap();
    };
    read("token:tok_ci", "secrets.resolve", "10.0.0.1");
    read("token:tok_ci", "secrets.resolve", "10.0.0.1");
    for _ in 0..30 {
        read("alice", "secrets.reveal", "10.0.0.9");
    }
    read("token:tok_ci", "secrets.resolve", "203.0.113.7");
    sweep().await;
    sweep().await;
    let sent = api.sent.0.lock().unwrap().clone();
    // Notices queued in one sweep go out in no particular order.
    let mut alerts: Vec<Value> = sent.iter().map(|n| n["alert"].clone()).collect();
    alerts.sort_by_key(|a| a["kind"].as_str().map(String::from));
    assert_eq!(alerts, vec![json!({ "kind": "many_reveals", "actor": "alice", "count": 30 }), json!({ "kind": "new_address", "actor": "token:tok_ci", "ip": "203.0.113.7" })], "{sent:?}");
    // The audit log keeps them too: the environment's activity, and audit sinks.
    let logged = api.ok(Method::GET, "/audit?action=secrets.alert", None).await;
    assert_eq!(logged.as_array().map(Vec::len), Some(2), "{logged}");
}

#[tokio::test]
async fn a_roll_access_grant_reaches_only_what_its_rules_name() {
    use roll_auth::scope::{Action as A, Effect, Resources, Rule, Scope, Selector};
    let api = Api::new().await;
    api.set("production", "DB_PASSWORD", "prod-pw").await;
    api.set("development", "REF", "${production.DB_PASSWORD}").await;
    let rule = Rule {
        product: "secrets".into(),
        resources: Resources::Some(vec![Selector::Project { project: api.project.clone() }]),
        environments: Some(vec!["development".into()]),
        paths: None,
        actions: BTreeMap::from([(A::Read, Effect::Allow), (A::Reveal, Effect::Allow)]),
    };
    let grant = roll_auth::Grant { ask_otherwise: false, id: "tok_ci".into(), name: "ci".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(Scope(vec![rule])) };
    // As rolld's gate passes a grant's request on: the operator marker and the grant.
    let call = |path: String| {
        let mut req = Request::builder().uri(format!("/v1/secrets/projects/{}{path}", api.project)).body(Body::empty()).unwrap();
        req.extensions_mut().insert(roll_auth::Operator);
        req.extensions_mut().insert(grant.clone());
        let router = api.router.clone();
        async move {
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null))
        }
    };
    let (s, v) = call("/environments/development/values".into()).await;
    assert_eq!((s, v["values"]["REF"].as_str()), (StatusCode::OK, Some("${production.DB_PASSWORD}")), "the reference into production stays unresolved");
    assert_eq!(call("/environments/production/values".into()).await.0, StatusCode::FORBIDDEN);
    let audit = api.ok(Method::GET, "/audit?actor=token:tok_ci", None).await;
    assert_eq!(audit[0]["action"], "secrets.export");
}

#[tokio::test]
async fn a_signed_in_person_manages_only_their_own_projects_secrets() {
    let api = Api::new().await;
    api.set("development", "STRIPE_KEY", "the operator's").await;
    let (s, mine) = api.as_ada(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "ada-shop" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{mine}");
    let mine = mine["id"].as_str().unwrap().to_string();
    let at = |p: &str, rest: &str| format!("/v1/secrets/projects/{p}{rest}");
    let (s, _) = api.as_ada(Method::PUT, &at(&mine, "/environments/development/secrets/STRIPE_KEY"), Some(json!({ "value": "ada's" }))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = api.as_ada(Method::GET, &at(&mine, "/environments/development/values"), None).await;
    assert_eq!(v["values"]["STRIPE_KEY"], "ada's");
    // Anyone else's project doesn't exist for her, and searching finds only hers.
    assert_eq!(api.as_ada(Method::GET, &at(&api.project, "/environments"), None).await.0, StatusCode::NOT_FOUND);
    let (_, found) = api.as_ada(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None).await;
    assert_eq!(found.as_array().map(|a| a.iter().all(|f| f["project"] == mine.as_str()) && a.len() == 1), Some(true), "{found}");
    // Her canvas proves nothing about who owns what's on it: no sink or link to a product there.
    api.as_ada(Method::PUT, &format!("/v1/workspace/projects/{mine}/resources/bucket/someone-elses"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, e) = api.as_ada(Method::POST, &at(&mine, "/audit/sinks"), Some(json!({ "url": "bucket:someone-elses" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    // A roll-access token she made acts as her (the evaluator puts her session beside its grant):
    // a rule naming someone else's project still doesn't reach it.
    let everything = roll_auth::scope::Rule {
        product: "secrets".into(),
        resources: roll_auth::scope::Resources::All,
        environments: None,
        paths: None,
        actions: BTreeMap::from([(roll_auth::scope::Action::Read, roll_auth::scope::Effect::Allow)]),
    };
    let grant = roll_auth::Grant { ask_otherwise: false, id: "tok_ada".into(), name: "ada".into(), kind: roll_auth::GrantKind::Token, scope: Arc::new(roll_auth::scope::Scope(vec![everything])) };
    for (project, want) in [(&mine, StatusCode::OK), (&api.project, StatusCode::NOT_FOUND)] {
        let mut req = Request::builder().uri(at(project, "/environments")).body(Body::empty()).unwrap();
        req.extensions_mut().insert(grant.clone());
        req.extensions_mut().insert(roll_auth::SignedIn { user: "usr_ada".into(), handle: "ada".into(), admin: false, code_owner: None, session: "ses_ada".into() });
        assert_eq!(api.router.clone().oneshot(req).await.unwrap().status(), want, "{project}");
    }
    // An admin's project is a whole canvas, though made through a session.
    let (_, theirs) = api.signed_in("root", true, Method::POST, "/v1/workspace/projects", Some(json!({ "name": "root-shop" }))).await;
    let theirs = theirs["id"].as_str().unwrap().to_string();
    api.signed_in("root", true, Method::PUT, &format!("/v1/workspace/projects/{theirs}/resources/bucket/archive"), Some(json!({ "x": 0, "y": 0 }))).await;
    let (s, e) = api.signed_in("root", true, Method::POST, &at(&theirs, "/audit/sinks"), Some(json!({ "url": "bucket:archive" }))).await;
    assert_eq!(s, StatusCode::CREATED, "{e}");
    // A token she made, and a token that token made, work while her account can sign in.
    let (_, t) = api.as_ada(Method::POST, &at(&mine, "/tokens"), Some(json!({ "name": "ci", "actions": ["read", "admin"] }))).await;
    let token = t["token"].as_str().expect("a token").to_string();
    let (_, child) = api.req(Method::POST, &at(&mine, "/tokens"), Some(json!({ "name": "child", "actions": ["read"] })), Some(&token)).await;
    let child = child["token"].as_str().expect("a child token").to_string();
    for t in [&token, &child] {
        assert_eq!(api.req(Method::GET, &at(&mine, "/environments"), None, Some(t)).await.0, StatusCode::OK);
    }
    *api.people.disabled.lock().unwrap() = true;
    for t in [&token, &child] {
        assert_eq!(api.req(Method::GET, &at(&mine, "/environments"), None, Some(t)).await.0, StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn a_workload_logs_in_with_its_oidc_token_for_a_short_lived_scoped_token() {
    let api = Api::new().await;
    api.set("staging", "API_KEY", "staging-key").await;
    api.set("production", "API_KEY", "prod-key").await;
    let issuer = Issuer::new();
    *api.sent.1.lock().unwrap() = Some(issuer.jwks());
    let identity = |subject: &str| json!({ "name": "deploy", "issuer": ISSUER, "audience": "roll", "subject": subject, "claims": { "ref": "refs/heads/*" }, "environments": ["staging"], "actions": ["read", "reveal"], "ttl_minutes": 10 });
    // GitHub signs for every repo: an identity of its names one.
    let (s, _) = api.call(Method::POST, "/identities", Some(identity("repo:*"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let i = api.ok(Method::POST, "/identities", Some(identity("repo:acme/web:*"))).await;
    let id = i["id"].as_str().unwrap().to_string();

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let claims = |sub: &str, git_ref: &str, exp: i64| json!({ "iss": ISSUER, "aud": "roll", "sub": sub, "ref": git_ref, "exp": exp, "iat": now });
    let login = |token: String| api.call(Method::POST, "/v1/secrets/login", Some(json!({ "identity": id, "token": token })));
    let (s, l) = login(issuer.sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now + 300))).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let token = l["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("rlst_") && l["expires_at"].as_i64().unwrap() <= (now + 601) * 1000);
    let (s, v) = api.req(Method::GET, "/environments/staging/values", None, Some(&token)).await;
    assert_eq!((s, v["values"]["API_KEY"].as_str()), (StatusCode::OK, Some("staging-key")));
    let (s, _) = api.req(Method::GET, "/environments/production/values", None, Some(&token)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only the identity's environments");

    // Another repo, another claim, an old token and a forged one log in as nobody.
    for (bad, want) in [
        (issuer.sign(claims("repo:acme/website:ref:refs/heads/main", "refs/heads/main", now + 300)), StatusCode::FORBIDDEN),
        (issuer.sign(claims("repo:acme/web:pull_request", "refs/pull/1/merge", now + 300)), StatusCode::FORBIDDEN),
        (issuer.sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now - 600)), StatusCode::UNAUTHORIZED),
        (Issuer::new().sign(claims("repo:acme/web:ref:refs/heads/main", "refs/heads/main", now + 300)), StatusCode::UNAUTHORIZED),
    ] {
        assert_eq!(login(bad).await.0, want);
    }
    let audit = api.ok(Method::GET, &format!("/audit?actor=identity:{id}"), None).await;
    assert_eq!(audit.as_array().unwrap().iter().map(|a| a["action"].as_str().unwrap()).collect::<Vec<_>>(), ["secrets.export", "identity.login"]);
    let listed = api.ok(Method::GET, "/identities", None).await;
    assert_eq!(listed[0]["last_subject"], "repo:acme/web:ref:refs/heads/main");

    // A cluster's own issuer isn't reachable: an identity pins its keys, and roll fetches nothing.
    *api.sent.1.lock().unwrap() = None;
    let cluster = Issuer::new();
    let k8s = "https://kubernetes.default.svc.cluster.local";
    let pods = |jwks: Value| json!({ "name": "pods", "issuer": k8s, "audience": "roll", "subject": "system:serviceaccount:shop:web", "environments": ["staging"], "actions": ["read", "reveal"], "jwks": jwks.to_string() });
    assert_eq!(api.call(Method::POST, "/identities", Some(pods(json!({ "keys": [] })))).await.0, StatusCode::BAD_REQUEST, "no usable key");
    let p = api.ok(Method::POST, "/identities", Some(pods(cluster.jwks()))).await;
    assert_eq!(p["pinned_keys"], 1);
    let pod = |signer: &Issuer| json!({ "identity": p["id"], "token": signer.sign(json!({ "iss": k8s, "aud": "roll", "sub": "system:serviceaccount:shop:web", "exp": now + 300, "iat": now })) });
    let (s, l) = api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await;
    assert_eq!(s, StatusCode::OK, "{l}");
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&issuer))).await.0, StatusCode::UNAUTHORIZED, "only its own keys");

    // Deleting the identity ends what it gave out.
    api.ok(Method::DELETE, &format!("/identities/{id}"), None).await;
    let (s, _) = api.req(Method::GET, "/environments/staging/values", None, Some(&token)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_folder_shared_by_another_project_reads_as_its_own_until_the_share_ends() {
    let api = Api::new().await;
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "platform" }))).await;
    let platform = other["id"].as_str().unwrap().to_string();
    let theirs = |p: &str| format!("/v1/secrets/projects/{platform}{p}");
    api.ok(Method::PUT, &theirs("/environments/production/secrets/SENTRY_DSN?path=/shared"), Some(json!({ "value": "https://key@sentry.example/1" }))).await;
    api.ok(Method::PUT, &theirs("/environments/production/secrets/NEXT?path=/shared"), Some(json!({ "value": "${SENTRY_DSN}" }))).await;
    let import = json!([{ "project": platform, "environment": "production", "path": "/shared" }]);
    // Not shared yet: no import.
    let (s, _) = api.call(Method::PUT, "/environments/staging/imports?path=/", Some(import.clone())).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    api.ok(Method::PUT, &theirs("/environments/production/exports?path=/shared"), Some(json!([{ "project": api.project }]))).await;
    assert_eq!(api.ok(Method::GET, "/shared", None).await, import, "offered here, ready to import");
    api.ok(Method::PUT, "/environments/staging/imports?path=/", Some(import)).await;
    // Its stored values, opened with its own key; a reference would resolve against the wrong
    // project, so it doesn't cross.
    let t = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ci", "environments": ["staging"], "actions": ["read", "reveal"] }))).await;
    let (s, v) = api.req(Method::GET, "/environments/staging/values", None, t["token"].as_str()).await;
    assert_eq!((s, v["values"].clone()), (StatusCode::OK, json!({ "SENTRY_DSN": "https://key@sentry.example/1" })));
    // The same environment, folder and key in this project stays this project's own.
    api.ok(Method::PUT, "/environments/production/secrets/SENTRY_DSN?path=/shared", Some(json!({ "value": "ours" }))).await;
    api.set("staging", "REF", "${production.shared.SENTRY_DSN}").await;
    let v = api.values("staging", "/").await;
    assert_eq!((v["values"]["REF"].as_str(), v["values"]["SENTRY_DSN"].as_str()), (Some("ours"), Some("https://key@sentry.example/1")));
    // A shared value pasted into this project's code is a leak too, named by its project.
    let leaks = api.secrets.leaks(&api.project, &["https://key@sentry.example/1".to_string()]).unwrap();
    assert_eq!(leaks.iter().map(|l| l.environment.as_str()).collect::<Vec<_>>(), ["platform/production"]);
    // A protected environment never reaches an untrusted consumer, shared or not.
    api.ok(Method::PUT, &theirs("/environments/production/policy"), Some(json!({ "protected": true }))).await;
    let r = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "preview:pr-1", "untrusted": true }))).await;
    assert!(r["values"].get("SENTRY_DSN").is_none(), "{r}");
    // The share ends: so does the import, at once.
    api.ok(Method::PUT, &theirs("/environments/production/exports?path=/shared"), Some(json!([]))).await;
    assert_eq!(api.values("staging", "/").await["values"]["SENTRY_DSN"], json!(null));
    let audit = api.ok(Method::GET, &theirs("/audit"), None).await;
    assert!(audit.as_array().unwrap().iter().any(|a| a["action"] == "exports.set"));
}

/// docs/secrets.md's HTTP table is the contract (and roll-access's catalogue is checked against
/// it): every route the router serves is in it.
#[tokio::test]
async fn every_route_is_in_the_http_contract() {
    let api = Api::new().await;
    let text = format!("{:?}", roll_secrets::router(api.secrets.clone()));
    let mut routes = vec![];
    let mut rest = text.split("fallback_router:").next().unwrap_or_default();
    while let Some(i) = rest.find("RouteId(") {
        rest = &rest[i + "RouteId(".len()..];
        let Some(after) = rest.trim_start_matches(|c: char| c.is_ascii_digit()).strip_prefix("): \"") else { continue };
        let Some(end) = after.find('"') else { break };
        if after.starts_with("/v1/secrets") {
            routes.push(after[..end].to_string());
        }
    }
    // `{anything}` → `{}`; the project prefix and a query dropped, as the table writes them.
    let norm = |p: &str| -> String {
        let p = p.split('?').next().unwrap_or(p).trim_end_matches('/');
        let p = p.strip_prefix("/v1/secrets/projects/{project}").map(|r| if r.is_empty() { "/".to_string() } else { r.to_string() }).unwrap_or_else(|| p.to_string());
        let mut out = String::new();
        let mut depth = 0;
        for c in p.chars() {
            match c {
                '{' => {
                    depth += 1;
                    out.push_str("{}");
                }
                '}' => depth -= 1,
                c if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    };
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/secrets.md")).unwrap();
    let documented: std::collections::HashSet<String> = doc
        .lines()
        .filter(|l| l.starts_with("| `"))
        .filter_map(|l| l[3..].split('`').next())
        // A method alone (`GET`) is the project itself.
        .filter_map(|cell| match cell.split_once(' ') {
            Some((_, path)) => Some(norm(path)),
            None => ["GET", "PATCH", "PUT", "POST", "DELETE"].contains(&cell).then(|| "/".to_string()),
        })
        .collect();
    let missing: Vec<String> = routes.iter().map(|r| norm(r)).filter(|r| !documented.contains(r)).collect();
    assert!(routes.len() > 40, "the routes were read ({})", routes.len());
    assert!(missing.is_empty(), "routes missing from docs/secrets.md's HTTP table: {missing:?}");
}

#[tokio::test]
async fn a_project_backs_up_sealed_to_a_key_and_restores_into_another() {
    let api = Api::new().await;
    api.set("staging", "API", "https://staging").await;
    api.ok(Method::PUT, "/environments/production/secrets/STRIPE?path=/pay", Some(json!({ "value": "sk_live_backup", "comment": "payments" }))).await;
    api.ok(Method::PUT, "/environments/development/imports?path=/", Some(json!([{ "environment": "staging", "path": "/" }]))).await;
    api.ok(Method::PUT, "/required", Some(json!([{ "key": "API", "type": "url" }]))).await;
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;

    let secret = [7u8; 32];
    let b = api.ok(Method::POST, "/backup", Some(json!({ "recipient": roll_secrets::crypto::public_key(&secret) }))).await;
    assert!(!b.to_string().contains("sk_live_backup"), "sealed, never in the clear");
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(b["sealed"].clone()).unwrap();
    let plain = roll_secrets::crypto::open_sealed(&secret, b["context"].as_str().unwrap(), &sealed).unwrap();
    let archive: Value = serde_json::from_slice(&plain).unwrap();

    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "restored" }))).await;
    let to = |p: &str| format!("/v1/secrets/projects/{}{p}", other["id"].as_str().unwrap());
    let r = api.ok(Method::POST, &to("/restore"), Some(archive.clone())).await;
    assert_eq!((r["secrets"].as_u64(), r["warnings"].clone()), (Some(2), json!([])));
    assert_eq!(api.ok(Method::GET, &to("/environments/production/values?path=/pay"), None).await["values"]["STRIPE"], "sk_live_backup");
    assert_eq!(api.ok(Method::GET, &to("/environments/development/values"), None).await["values"]["API"], "https://staging", "imports restored");
    assert_eq!(api.ok(Method::GET, &to("/environments/production"), None).await["protected"], true, "protection last");
    assert_eq!(api.ok(Method::GET, &to("/required"), None).await[0]["type"], "url");
    let (s, _) = api.call(Method::POST, &to("/restore"), Some(archive)).await;
    assert_eq!(s, StatusCode::CONFLICT, "only into a project with no secrets");
}

#[tokio::test]
async fn a_key_is_found_in_every_project_the_caller_may_read() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_1").await;
    api.set("staging", "STRIPE_WEBHOOK", "whsec_1").await;
    api.set("staging", "STRIPEXKEY", "not a prefix match").await;
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "billing" }))).await;
    let theirs = format!("/v1/secrets/projects/{}/environments/development/secrets/STRIPE_KEY", other["id"].as_str().unwrap());
    api.ok(Method::PUT, &theirs, Some(json!({ "value": "sk_test_2" }))).await;

    let found = api.ok(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None).await;
    let places: Vec<(&str, &str)> = found.as_array().unwrap().iter().map(|p| (p["project_name"].as_str().unwrap(), p["environment"].as_str().unwrap())).collect();
    assert_eq!(places.len(), 2, "{found}");
    assert!(places.contains(&("shop", "production")) && places.contains(&("billing", "development")));
    assert!(!found.to_string().contains("sk_"), "never a value");
    // `_` is a character, not LIKE's wildcard.
    let prefix = api.ok(Method::GET, "/v1/secrets/search?key=STRIPE_*", None).await;
    let keys: Vec<&str> = prefix.as_array().unwrap().iter().map(|p| p["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"STRIPE_WEBHOOK") && !keys.contains(&"STRIPEXKEY"), "{keys:?}");
    // A token for one project finds only there.
    let t = api.ok(Method::POST, "/tokens", Some(json!({ "name": "ro", "actions": ["read"] }))).await;
    let (_, mine) = api.req(Method::GET, "/v1/secrets/search?key=STRIPE_KEY", None, t["token"].as_str()).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
}
