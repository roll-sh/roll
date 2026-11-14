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
const ISSUER_URL: &str = "https://roll.example/v1/secrets/oidc";

/// GCP and Azure: they trust roll's token only for the pinned subject, and keep what they saw.
#[derive(Default)]
struct FakeClouds {
    pinned: Mutex<Option<String>>,
    seen: Mutex<Vec<String>>,
    /// Answer every exchange with an outage.
    down: std::sync::atomic::AtomicBool,
}

impl FakeClouds {
    fn trust(&self, jwt: &Secret, audience: &str) -> Result<u32, String> {
        use base64::Engine;
        let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(jwt.expose().split('.').nth(1).unwrap()).unwrap()).unwrap();
        if self.down.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("503 Service Unavailable".into());
        }
        let mut seen = self.seen.lock().unwrap();
        seen.push(jwt.expose().to_string());
        if claims["aud"] != audience || self.pinned.lock().unwrap().as_deref().is_some_and(|p| claims["sub"] != p) {
            return Err("invalid_grant: the subject isn't trusted".into());
        }
        Ok(seen.len() as u32)
    }
}

impl roll_secrets::clouds::CloudTokens for FakeClouds {
    fn gcp(&self, audience: &str, _: &str, jwt: &Secret, lifetime: u32) -> Result<(Secret, i64), String> {
        let n = self.trust(jwt, audience)?;
        Ok((Secret::new(format!("ya29.{n}")), i64::from(lifetime) * 1000))
    }
    fn azure(&self, _: &str, _: &str, _: &str, jwt: &Secret) -> Result<(Secret, i64), String> {
        let n = self.trust(jwt, roll_secrets::clouds::AZURE_AUDIENCE)?;
        Ok((Secret::new(format!("eyJ0eXAi.{n}")), 3_600_000))
    }
}

/// A Postgres server outside roll: each role's current password and, for a dynamic login, its
/// group and `VALID UNTIL`; the flag refuses every change.
#[derive(Default)]
struct Outside(std::sync::Mutex<BTreeMap<String, String>>, std::sync::atomic::AtomicBool, std::sync::Mutex<BTreeMap<String, (String, i64)>>);

impl roll_secrets::outside::OutsideDb for Outside {
    fn check(&self, url: &str, _: Option<&str>, roles: &[String], _: Option<&str>) -> Result<(), String> {
        if url.contains("@db.example") && roles.iter().all(|r| r.starts_with("app_")) { Ok(()) } else { Err("there's no role that can log in".into()) }
    }
    fn set_password(&self, _: &str, _: Option<&str>, role: &str, password: &str) -> Result<(), String> {
        if self.1.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("connection refused".into());
        }
        self.0.lock().unwrap().insert(role.into(), password.into());
        Ok(())
    }
    fn create_login(&self, _: &str, _: Option<&str>, name: &str, password: &str, group: &str, until: i64) -> Result<(), String> {
        self.0.lock().unwrap().insert(name.into(), password.into());
        self.2.lock().unwrap().insert(name.into(), (group.into(), until));
        Ok(())
    }
    fn extend_login(&self, _: &str, _: Option<&str>, name: &str, until: i64) -> Result<(), String> {
        self.2.lock().unwrap().get_mut(name).ok_or("no such role")?.1 = until;
        Ok(())
    }
    fn drop_login(&self, _: &str, _: Option<&str>, name: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(name);
        self.2.lock().unwrap().remove(name);
        Ok(())
    }
}

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

/// A customer's KMS: its key policy pins one project (or, unpinned, takes any), and it stops
/// decrypting once revoked.
#[derive(Default)]
struct FakeKms {
    revoked: Mutex<bool>,
    pinned: Mutex<Option<String>>,
}

impl roll_secrets::kms::ProjectKms for FakeKms {
    fn encrypt(&self, key: &str, project: &str, plain: &[u8]) -> Result<Vec<u8>, String> {
        if self.pinned.lock().unwrap().as_deref().is_some_and(|p| p != project) {
            return Err("AccessDeniedException".into());
        }
        Ok([format!("{key}|{project}|").as_bytes(), plain].concat())
    }
    fn decrypt(&self, key: &str, project: &str, ct: &[u8]) -> Result<zeroize::Zeroizing<Vec<u8>>, String> {
        if *self.revoked.lock().unwrap() {
            return Err("AccessDeniedException".into());
        }
        let prefix = format!("{key}|{project}|");
        ct.strip_prefix(prefix.as_bytes()).map(|p| zeroize::Zeroizing::new(p.to_vec())).ok_or_else(|| "InvalidCiphertextException".into())
}
}

/// STS for one role whose trust policy pins one ExternalId (or, unpinned, takes any).
#[derive(Default)]
struct FakeSts {
    pinned: Mutex<Option<String>>,
    sessions: Mutex<u32>,
    /// IAM user `legacy-ci`'s access key ids, and how many were ever made.
    keys: Mutex<(Vec<String>, u32)>,
}

impl roll_secrets::aws::Sts for FakeSts {
    fn assume(&self, role_arn: &str, external_id: &str, _session: &str, seconds: i32) -> Result<roll_secrets::aws::Session, String> {
        if role_arn != "arn:aws:iam::123456789012:role/roll" || self.pinned.lock().unwrap().as_deref().is_some_and(|p| p != external_id) {
            return Err("AccessDenied".into());
        }
        let mut n = self.sessions.lock().unwrap();
        *n += 1;
        Ok(roll_secrets::aws::Session {
            access_key_id: Secret::new(format!("ASIA{n}")),
            secret_access_key: Secret::new(format!("secret-{n}")),
            session_token: Secret::new(format!("token-{n}")),
            expires_at: i64::from(seconds) * 1000,
        })
    }
    fn iam_keys(&self, role_arn: &str, external_id: &str, user: &str) -> Result<Vec<String>, String> {
        self.assume(role_arn, external_id, "iam", 900)?;
        if user != "legacy-ci" {
            return Err("NoSuchEntity".into());
        }
        Ok(self.keys.lock().unwrap().0.clone())
    }
    fn iam_create_key(&self, role_arn: &str, external_id: &str, user: &str) -> Result<(Secret, Secret), String> {
        if self.iam_keys(role_arn, external_id, user)?.len() >= 2 {
            return Err("LimitExceeded".into());
        }
        let mut k = self.keys.lock().unwrap();
        k.1 += 1;
        let id = format!("AKIA{}", k.1);
        k.0.push(id.clone());
        Ok((Secret::new(id), Secret::new(format!("iam-secret-{}", k.1))))
    }
    fn iam_delete_key(&self, role_arn: &str, external_id: &str, user: &str, key_id: &str) -> Result<(), String> {
        self.iam_keys(role_arn, external_id, user)?;
        self.keys.lock().unwrap().0.retain(|k| k != key_id);
        Ok(())
    }
}

struct Api {
    sent: Arc<Sent>,
    kms: Arc<FakeKms>,
    ws: Arc<roll_workspace::Workspace>,
    sts: Arc<FakeSts>,
    people: Arc<People>,
    router: Router,
    secrets: Arc<Secrets>,
    db: code_sql::Db,
    fake: Arc<Fake>,
    sync: Arc<FakeSync>,
    clouds: Arc<FakeClouds>,
    outside: Arc<Outside>,
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
        let kms = Arc::new(FakeKms::default());
        let outside = Arc::new(Outside::default());
        let sts = Arc::new(FakeSts::default());
        ws.set_owners(people.clone());
        let clouds = Arc::new(FakeClouds::default());
        let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links, syncs, open: true, trust_forwarded_for: false, web_url: None, deliver: Some(sent.clone()), grants: None, people: Some(people.clone()), kms: Some(kms.clone()), sts: Some(sts.clone()), outside: Some(outside.clone()), clouds: Some(roll_secrets::clouds::Clouds { issuer: ISSUER_URL.into(), tokens: clouds.clone() }) };
        let secrets = Arc::new(Secrets::new(db.clone(), ws.clone(), opts).unwrap());
        let router = roll_workspace::router(ws.clone()).merge(roll_secrets::router(secrets.clone()));
        let mut api = Api { sent, kms, sts, ws: ws.clone(), people, router, secrets, db, fake, sync, outside, clouds, project: String::new(), _dir: dir };
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
    // Check-and-set: two writers read A at one version; the second's change is refused whole.
    let b = api.ok(Method::GET, "/environments/production/secrets/A", None).await["version"].as_i64().unwrap();
    api.ok(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "first", "if_version": b }))).await;
    let (st, e) = api.call(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "D", "value": "4" }, { "op": "set", "key": "A", "value": "second", "if_version": b }] }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale" && e["error"].as_str().unwrap().contains("A changed since version"), "{e}");
    assert_eq!((api.values("production", "/").await["values"]["A"].clone(), api.values("production", "/").await["values"]["D"].clone()), (json!("first"), Value::Null), "nothing of the refused change");
    // 0: only if it doesn't exist yet.
    assert_eq!(api.call(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "x", "if_version": 0 }))).await.0, StatusCode::CONFLICT);
    api.ok(Method::PUT, "/environments/production/secrets/NEWKEY", Some(json!({ "value": "x", "if_version": 0 }))).await;
    // Protected: a stale read is told so, not sent to a request; a request keeps the check.
    let now = api.ok(Method::GET, "/environments/production/secrets/A", None).await["version"].as_i64().unwrap();
    api.ok(Method::PUT, "/environments/production/policy", Some(json!({ "protected": true }))).await;
    let (st, e) = api.call(Method::PUT, "/environments/production/secrets/A", Some(json!({ "value": "y", "if_version": now - 1 }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale", "{e}");
    let (st, e) = api.call(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "A", "value": "y", "if_version": now - 1 }] }))).await;
    assert!(st == StatusCode::CONFLICT && e["code"] == "stale", "{e}");
    api.ok(Method::POST, "/environments/production/requests", Some(json!({ "ops": [{ "op": "set", "key": "A", "value": "y", "if_version": now }] }))).await;
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
    // A values answer carries the same digest, taken before its read: a watch compares from it.
    let (_, with) = api.call(Method::GET, "/environments/development/values?digest=1", None).await;
    assert_eq!((with["digest"].clone(), api.values("development", "/").await.get("digest").cloned()), (before.clone(), None), "asked for (a watch)");
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
    // A folder's own change policy, in an environment that isn't protected: its keys change only
    // through approved requests; the rest of the environment as before.
    api.ok(Method::PUT, "/environments/staging/folder-policies?path=/payments", Some(json!({ "approvals": 1 }))).await;
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/STRIPE?path=/payments/eu", Some(json!({ "value": "sk_1" }))).await;
    assert!(s == StatusCode::CONFLICT && e["code"] == "approval_required" && e["error"].as_str().unwrap().contains("/payments"), "{e}");
    api.ok(Method::PUT, "/environments/staging/secrets/OTHER", Some(json!({ "value": "free" }))).await;
    let r = api.ok(Method::POST, "/environments/staging/requests", Some(json!({ "path": "/payments/eu", "ops": [{ "op": "set", "key": "STRIPE", "value": "sk_1" }] }))).await;
    assert_eq!(r["gates"], json!([{ "path": "/payments", "approvals": 1, "approved": 0 }]));
    let id = r["id"].as_str().unwrap();
    let (s, r) = api.req(Method::POST, &format!("/environments/staging/requests/{id}/review"), Some(json!({ "verdict": "approve" })), reviewer["token"].as_str()).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("approved")));
    api.ok(Method::POST, &format!("/environments/staging/requests/{id}/apply"), None).await;
    assert_eq!(api.values("staging", "/payments/eu").await["values"]["STRIPE"], "sk_1");
    // Protected as well: an admin of /payments only approves for its folder's gate, never for the
    // environment's.
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": true, "approvals": 1 }))).await;
    let folder_admin = api.ok(Method::POST, "/tokens", Some(json!({ "name": "payments", "environments": ["staging"], "paths": ["/payments"], "actions": ["read", "write", "admin"] }))).await;
    let r = api.ok(Method::POST, "/environments/staging/requests", Some(json!({ "path": "/payments/eu", "ops": [{ "op": "set", "key": "STRIPE", "value": "sk_2" }] }))).await;
    let id = r["id"].as_str().unwrap();
    let (s, r) = api.req(Method::POST, &format!("/environments/staging/requests/{id}/review"), Some(json!({ "verdict": "approve" })), folder_admin["token"].as_str()).await;
    assert_eq!((s, r["state"].as_str()), (StatusCode::OK, Some("open")), "{r}");
    assert_eq!(r["gates"].as_array().unwrap().iter().map(|g| g["approved"].as_i64().unwrap()).collect::<Vec<_>>(), [0, 1], "the environment's gate, then /payments'");
    api.ok(Method::PUT, "/environments/staging/policy", Some(json!({ "protected": false }))).await;
    // Nor by deleting the folder, or one above it.
    for folder in ["/payments", "/payments/eu"] {
        let (s, e) = api.call(Method::DELETE, &format!("/environments/staging/folders?path={folder}"), None).await;
        assert!(s == StatusCode::CONFLICT && e["code"] == "approval_required", "{folder}: {e}");
    }
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
    // A reader that can't renew (External Secrets Operator) asks for none: the key is left out
    // (named in `dynamic`), no lease. With `strict`, a key that doesn't resolve fails the whole
    // read, so the reader keeps what it had instead of writing an empty value.
    let pulled = api.ok(Method::GET, "/environments/staging/values?dynamic=false&strict=1", None).await;
    assert!(pulled["values"].get("TEMP_KEY").is_none() && pulled["dynamic"] == json!(["TEMP_KEY"]), "{pulled}");
    assert_eq!((pulled["leases"].as_array().map(Vec::len), api.fake.live_keys.lock().unwrap().len()), (Some(0), 3));
    api.set("staging", "BROKEN", "${NOPE}").await;
    let (s, e) = api.call(Method::GET, "/environments/staging/values?dynamic=false&strict=1", None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("unresolved")), "{e}");
    // Dynamic keys asked for, and the read refused: what it issued is revoked, nothing outlives it.
    let (s, _) = api.call(Method::GET, "/environments/staging/values?strict=1", None).await;
    assert_eq!((s, api.fake.live_keys.lock().unwrap().len()), (StatusCode::CONFLICT, 3));
    api.ok(Method::DELETE, "/environments/staging/secrets/BROKEN", None).await;

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
    // A server's credential under a name a framework sends to every browser: the write warns and
    // health flags it; a publishable key under the same prefix is left alone.
    let w = api.ok(Method::PUT, "/environments/development/secrets/NEXT_PUBLIC_STRIPE_KEY", Some(json!({ "value": "sk_live_51Hx9Q2aBcD7eFgH8iJkLmN" }))).await;
    assert!(w["warnings"][0].as_str().unwrap().starts_with("NEXT_PUBLIC_STRIPE_KEY: NEXT_PUBLIC_ keys go into the browser bundle (Next.js)"), "{w}");
    api.set("development", "NEXT_PUBLIC_STRIPE_PUBLISHABLE", "pk_live_51Hx9Q2aBcD7eFgH8iJkLmN").await;
    let public: Vec<String> = api.ok(Method::GET, "/health", None).await.as_array().unwrap().iter().filter(|f| f["issue"] == "public").map(|f| f["key"].as_str().unwrap().to_string()).collect();
    assert_eq!(public, ["NEXT_PUBLIC_STRIPE_KEY"]);
    // What kind of credential it is comes from the value: not for a caller who can't reveal it.
    let reader = api.ok(Method::POST, "/tokens", Some(json!({ "name": "reader", "actions": ["read"] }))).await;
    let (_, seen) = api.req(Method::GET, "/health", None, reader["token"].as_str()).await;
    let detail = seen.as_array().unwrap().iter().find(|f| f["issue"] == "public").map(|f| f["detail"].clone());
    assert_eq!(detail, Some(json!("NEXT_PUBLIC_ keys go into the browser bundle (Next.js), and this one holds a server credential")));
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

    // A scheduled rotation and a revocation that fail show in health until they succeed.
    api.sent.2.store(true, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db
        .write(move |c| {
            // Due now (rotate_days 1, issued long ago) and the retired key's grace over.
            let state: String = c.query_row("SELECT link_state FROM secrets WHERE project_id = ? AND key = 'STRIPE_KEY'", [p.as_str()], |r| r.get(0))?;
            let mut state: Value = serde_json::from_str(&state).unwrap();
            state["issued_at"] = json!(0);
            c.execute("UPDATE secrets SET link_state = ?, link = replace(link, '\"mode\":\"managed\"', '\"mode\":\"managed\",\"options\":{\"rotate_days\":1}') WHERE project_id = ? AND key = 'STRIPE_KEY'", [state.to_string().as_str(), p.as_str()])?;
            c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])
        })
        .unwrap();
    let sweep = || {
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    sweep().await;
    let health = api.ok(Method::GET, "/health", None).await;
    let issues: Vec<&str> = health.as_array().unwrap().iter().filter_map(|f| f["issue"].as_str()).collect();
    assert!(issues.contains(&"rotation_failing") && issues.contains(&"revoke_failing"), "{health}");
    assert!(health.to_string().contains("connection refused"), "the reason: {health}");
    // The rotations list says it too, per credential.
    let rot = api.ok(Method::GET, "/rotations", None).await;
    assert_eq!((rot[0]["keys"].clone(), rot[0]["kind"].as_str(), rot[0]["rotate_days"].as_u64()), (json!(["STRIPE_KEY"]), Some("rotator"), Some(1)), "{rot}");
    assert!(rot[0]["last_error"].as_str().unwrap().contains("connection refused"), "{rot}");
    api.sent.2.store(false, std::sync::atomic::Ordering::Relaxed);
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND revoked_at IS NULL", [p.as_str()])).unwrap();
    sweep().await;
    let health = api.ok(Method::GET, "/health", None).await;
    assert!(!health.to_string().contains("failing"), "both cleared once they succeed: {health}");
    let rot = api.ok(Method::GET, "/rotations", None).await;
    assert!(rot[0]["last_error"].is_null() && rot[0]["next_due"].as_i64().unwrap() > rot[0]["issued_at"].as_i64().unwrap(), "{rot}");

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
async fn a_database_outside_roll_hands_out_logins_in_turn_and_per_lease() {
    let api = Api::new().await;
    // A password with a # unencoded: parsers disagree on where it ends, so it's refused, never
    // shown as the host.
    let (s, e) = api.call(Method::POST, "/outside-databases", Some(json!({ "name": "x", "url": "postgres://admin:abc#def@db.example/app", "roles": ["app_blue", "app_green"] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && !e.to_string().contains("abc"), "{e}");
    let new = json!({ "name": "billing", "url": "postgres://admin:hunter2@db.example:5432/app?sslmode=require", "roles": ["app_blue", "app_green"], "group": "app_rw" });
    let d = api.ok(Method::POST, "/outside-databases", Some(new)).await;
    assert_eq!((d["host"].as_str(), d.get("url")), (Some("db.example:5432"), None), "the admin URL stays sealed: {d}");
    let link = |field: &str| json!({ "kind": "outside_database", "ref": d["id"], "field": field, "mode": "managed" });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "DB_USER", "link": link("username") }, { "op": "set", "key": "DB_PASSWORD", "link": link("password") }] }))).await;
    let login = || async {
        let v = api.values("production", "/").await["values"].clone();
        (v["DB_USER"].as_str().unwrap().to_string(), v["DB_PASSWORD"].as_str().unwrap().to_string())
    };
    let password = |role: &str| api.outside.0.lock().unwrap().get(role).cloned();
    let blue = login().await;
    assert_eq!((blue.0.as_str(), password("app_blue")), ("app_blue", Some(blue.1.clone())));

    // Each rotation hands out the other role; the one in use keeps working through the grace.
    let rotate = || api.call(Method::POST, "/environments/production/secrets/DB_PASSWORD/rotate", None);
    assert!(rotate().await.0.is_success());
    let green = login().await;
    assert_eq!((green.0.as_str(), password("app_green"), password("app_blue")), ("app_green", Some(green.1.clone()), Some(blue.1.clone())));
    // Both roles are taken (green in use, blue in its grace): another rotation waits, resetting
    // nothing, even once the issue's own short hold on green is over.
    let id = d["id"].as_str().unwrap().to_string();
    let unhold = || {
        let id = id.clone();
        api.db
            .write(move |c| {
                let s: String = c.query_row("SELECT state FROM outside_dbs WHERE id = ?", [id.as_str()], |r| r.get(0))?;
                let mut v: Value = serde_json::from_str(&s).unwrap();
                v["held"] = json!({});
                c.execute("UPDATE outside_dbs SET state = ? WHERE id = ?", [v.to_string().as_str(), id.as_str()])
            })
            .unwrap()
    };
    unhold();
    assert_eq!(rotate().await.0, StatusCode::CONFLICT);
    assert_eq!((login().await, password("app_blue")), (green.clone(), Some(blue.1.clone())));
    // Blue's grace ends: its password goes, and it's free again.
    let expire = |cond: &'static str| {
        let p = api.project.clone();
        api.db.write(move |c| c.execute(&format!("UPDATE leases SET expires_at = 0 WHERE project_id = ? AND {cond}"), [p.as_str()])).unwrap();
        let s = api.secrets.clone();
        async move { tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap() }
    };
    expire("1 = 1").await;
    assert_ne!(password("app_blue"), Some(blue.1.clone()));
    // A rotation the server refused changes nothing, and the next one takes the free role.
    api.outside.1.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(!rotate().await.0.is_success());
    api.outside.1.store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!((login().await, password("app_green")), (green.clone(), Some(green.1.clone())));
    assert!(rotate().await.0.is_success());
    let blue2 = login().await;
    assert_eq!((blue2.0.as_str(), password("app_green")), ("app_blue", Some(green.1.clone())));
    // A retirement that comes late (blue's first) doesn't undo blue's reissue.
    let (p, late) = (api.project.clone(), json!({ "kind": "outside_database", "ref": d["id"], "field": "username", "mode": "managed" }).to_string());
    api.db
        .write(move |c| {
            c.execute(
                "INSERT INTO leases (id, project_id, env_id, key, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES ('lse_late', ?, '', '', 'retired', ?, '{\"role\":\"app_blue\",\"generation\":1}', 'test', 0, 1, NULL)",
                [p.as_str(), late.as_str()],
            )
        })
        .unwrap();
    expire("id = 'lse_late'").await;
    assert_eq!(password("app_blue"), Some(blue2.1.clone()));
    // Its two roles serve one environment.
    let (s, v) = api.call(Method::PUT, "/environments/staging/secrets/DB_USER", Some(json!({ "link": link("username") }))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // Dynamic: each resolve is a login of its own in the group, valid until its lease ends (the
    // server enforces it), renewed with the lease, dropped when it ends.
    let dynamic = |field: &str| json!({ "kind": "outside_database", "ref": d["id"], "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15 } });
    api.ok(Method::POST, "/environments/staging/changes", Some(json!({ "ops": [{ "op": "set", "key": "TMP_USER", "link": dynamic("username") }, { "op": "set", "key": "TMP_PASSWORD", "link": dynamic("password") }] }))).await;
    let a = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let b = api.ok(Method::POST, "/environments/staging/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let user = a["values"]["TMP_USER"].as_str().unwrap().to_string();
    assert_ne!(user, b["values"]["TMP_USER"].as_str().unwrap());
    assert_eq!(password(&user).as_deref(), a["values"]["TMP_PASSWORD"].as_str());
    let (group, until) = api.outside.2.lock().unwrap()[&user].clone();
    let lease = a["leases"][0].as_str().unwrap();
    let expires = api.ok(Method::GET, "/leases", None).await.as_array().unwrap().iter().find(|l| l["id"] == lease).unwrap()["expires_at"].as_i64().unwrap();
    assert!(group == "app_rw" && until > expires && until <= expires + 10 * 60_000, "{group} {until} {expires}");
    let renewed = api.ok(Method::POST, &format!("/leases/{lease}/renew"), Some(json!({ "ttl_minutes": 120 }))).await;
    assert!(api.outside.2.lock().unwrap()[&user].1 > renewed["expires_at"].as_i64().unwrap());
    api.ok(Method::DELETE, &format!("/leases/{lease}"), None).await;
    assert_eq!(password(&user), None);

    let (s, _) = api.call(Method::DELETE, &format!("/outside-databases/{}", d["id"].as_str().unwrap()), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "its keys keep it");
}

#[tokio::test]
async fn a_cloud_account_trusts_one_projects_token() {
    use base64::Engine;
    let api = Api::new().await;
    let doc = api.ok(Method::GET, "/v1/secrets/oidc/.well-known/openid-configuration", None).await;
    assert_eq!((doc["issuer"].as_str(), doc["jwks_uri"].as_str()), (Some(ISSUER_URL), Some(format!("{ISSUER_URL}/jwks").as_str())));
    let gcp = json!({ "name": "analytics", "cloud": "gcp", "audience": "//iam.googleapis.com/projects/1234/locations/global/workloadIdentityPools/roll/providers/roll", "service_account": "reports@shop-prod.iam.gserviceaccount.com" });
    // An outage proves nothing about the trust: the account isn't taken until the cloud answers.
    api.clouds.down.store(true, std::sync::atomic::Ordering::Relaxed);
    let (s, e) = api.call(Method::POST, "/cloud-accounts", Some(gcp.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("couldn't tell"), "{e}");
    api.clouds.down.store(false, std::sync::atomic::Ordering::Relaxed);
    // A pool that takes any of roll's projects (another customer's, say) is refused.
    let (s, e) = api.call(Method::POST, "/cloud-accounts", Some(gcp.clone())).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains(&format!("roll-secrets:{}", api.project)), "{e}");
    *api.clouds.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let g = api.ok(Method::POST, "/cloud-accounts", Some(gcp)).await;
    let a = api.ok(Method::POST, "/cloud-accounts", Some(json!({ "name": "infra", "cloud": "azure", "tenant": "contoso.onmicrosoft.com", "client_id": "11111111-2222-3333-4444-555555555555" }))).await;
    // Each resolve gets a token of its own, from a token roll signed for this project.
    for (key, kind, acct) in [("GOOGLE_OAUTH_ACCESS_TOKEN", "gcp", &g), ("AZURE_ACCESS_TOKEN", "azure", &a)] {
        api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(json!({ "link": { "kind": kind, "ref": acct["id"], "field": "access_token", "mode": "dynamic", "options": { "ttl_minutes": 15 } } }))).await;
    }
    let one = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:report" }))).await;
    let two = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:report" }))).await;
    assert!(one["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"].as_str().unwrap().starts_with("ya29.") && one["values"]["AZURE_ACCESS_TOKEN"].is_string());
    assert_ne!(one["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"], two["values"]["GOOGLE_OAUTH_ACCESS_TOKEN"]);
    // What the cloud got verifies against roll's published keys, and names this project.
    let jwt = api.clouds.seen.lock().unwrap().last().unwrap().clone();
    let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    let (header, claims): (Value, Value) = (serde_json::from_slice(&b64(parts[0])).unwrap(), serde_json::from_slice(&b64(parts[1])).unwrap());
    let jwks = api.ok(Method::GET, "/v1/secrets/oidc/jwks", None).await;
    let key = jwks["keys"].as_array().unwrap().iter().find(|k| k["kid"] == header["kid"]).expect("the signing key is published");
    let public = ring::signature::RsaPublicKeyComponents { n: b64(key["n"].as_str().unwrap()), e: b64(key["e"].as_str().unwrap()) };
    public.verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64(parts[2])).expect("roll's signature");
    assert_eq!((claims["iss"].as_str(), claims["sub"].clone(), header["alg"].as_str()), (Some(ISSUER_URL), json!(format!("roll-secrets:{}", api.project)), Some("RS256")));
    assert_eq!(claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(), 300);
    // An account a key links to stays, and a lease can't outlive its token.
    assert_eq!(api.call(Method::DELETE, &format!("/cloud-accounts/{}", g["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    let (s, _) = api.call(Method::PUT, "/environments/production/secrets/LONG_TOKEN", Some(json!({ "link": { "kind": "gcp", "ref": g["id"], "field": "access_token", "mode": "dynamic", "options": { "ttl_minutes": 120 } } }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The deployment key rotates: the issuer's key is resealed with the new one (health says when
    // the previous key can go), and keeps signing once the previous key is gone.
    let reopen = |current: &str, previous: Option<&str>| {
        let opts = Options { keyring: Ok(Keyring::new(current, previous).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, clouds: Some(roll_secrets::clouds::Clouds { issuer: ISSUER_URL.into(), tokens: api.clouds.clone() }), kms: None, sts: None, outside: None };
        Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap()
    };
    let (old, new) = ("k".repeat(32), "n".repeat(32));
    let rotated = reopen(&new, Some(&old));
    let p = api.project.clone();
    // Any request reseals its project's key; health reseals the issuer's.
    let s = tokio::task::spawn_blocking(move || {
        rotated.cloud_accounts(&p, &roll_secrets::Caller::operator()).unwrap();
        rotated.health().unwrap()
    })
    .await
    .unwrap();
    assert_eq!(s["resealed"], "all", "{s}");
    let after = Arc::new(reopen(&new, None));
    let (a, acct) = (after.clone(), g["id"].as_str().unwrap().to_string());
    let jwks = tokio::task::spawn_blocking(move || a.oidc_jwks().unwrap()).await.unwrap();
    assert_eq!(jwks["keys"].as_array().unwrap().len(), 1, "the same key, still published");
    let a = after.clone();
    tokio::task::spawn_blocking(move || a.links().get("gcp").unwrap().issue(&acct, "token", &serde_json::from_value(json!({ "kind": "gcp", "ref": "x", "field": "access_token", "mode": "dynamic" })).unwrap(), "after", Some(15)).map(|_| ()))
        .await
        .unwrap()
        .expect("signing with the resealed key");
}

#[tokio::test]
async fn a_projects_ca_signs_short_lived_certificates_for_its_names_only() {
    use x509_parser::prelude::*;
    let api = Api::new().await;
    api.secrets.set_public_api("https://roll.example".into());
    let ca = api.ok(Method::POST, "/cas", Some(json!({ "name": "internal", "domains": ["svc.internal"] }))).await;
    let ca_pem = ca["certificate"].as_str().unwrap().to_string();
    let link = |field: &str, mode: &str, names: Value| json!({ "kind": "pki", "ref": ca["id"], "field": field, "mode": mode, "options": { "names": names, "ttl_minutes": 60, "rotate_days": 7 } });
    // Each resolve gets a certificate of its own, signed by the CA, for the link's names.
    let names = json!(["api.svc.internal"]);
    let ops: Vec<Value> = [("certificate", "TLS_CERT"), ("private_key", "TLS_KEY"), ("ca", "TLS_CA")].iter().map(|(f, k)| json!({ "op": "set", "key": k, "link": link(f, "dynamic", names.clone()) })).collect();
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": ops }))).await;
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:api" }))).await;
    let (cert, key, chain) = (r["values"]["TLS_CERT"].as_str().unwrap(), r["values"]["TLS_KEY"].as_str().unwrap(), r["values"]["TLS_CA"].as_str().unwrap());
    assert!(key.starts_with("-----BEGIN PRIVATE KEY-----") && chain == ca_pem);
    let (_, ca_der) = parse_x509_pem(ca_pem.as_bytes()).unwrap();
    let (_, ca_cert) = parse_x509_certificate(&ca_der.contents).unwrap();
    let (_, der) = parse_x509_pem(cert.as_bytes()).unwrap();
    let (_, leaf) = parse_x509_certificate(&der.contents).unwrap();
    leaf.verify_signature(Some(ca_cert.public_key())).expect("signed by the project's CA");
    let sans: Vec<String> = leaf.subject_alternative_name().unwrap().unwrap().value.general_names.iter().map(|n| format!("{n}")).collect();
    assert_eq!(sans, ["DNSName(api.svc.internal)"]);
    let life = leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp();
    assert!((3900..=4000).contains(&life), "the lease's TTL plus five minutes: {life}");
    // The CA is limited to its domains and no IP addresses, so trusting it trusts nothing else.
    let nc = ca_cert.name_constraints().unwrap().unwrap().value;
    assert_eq!(nc.permitted_subtrees.as_ref().map(Vec::len), Some(1));
    assert_eq!(nc.excluded_subtrees.as_ref().map(Vec::len), Some(2));
    // A name outside its domains is refused; managed certificates live twice their rotation.
    let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": link("certificate", "managed", json!("web.example.com")) }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e["error"].as_str().unwrap().contains("doesn't sign for web.example.com"), "{e}");
    // A managed certificate rotates before it ends, or isn't taken.
    for options in [json!({ "names": "web.svc.internal" }), json!({ "names": "web.svc.internal", "rotate_days": 60 })] {
        let (s, e) = api.call(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": { "kind": "pki", "ref": ca["id"], "field": "certificate", "mode": "managed", "options": options } }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    }
    api.ok(Method::PUT, "/environments/staging/secrets/WEB_CERT", Some(json!({ "link": link("certificate", "managed", json!("web.svc.internal, *.web.svc.internal")) }))).await;
    let stored = api.values("staging", "/").await["values"]["WEB_CERT"].as_str().unwrap().to_string();
    let (_, der) = parse_x509_pem(stored.as_bytes()).unwrap();
    let (_, managed) = parse_x509_certificate(&der.contents).unwrap();
    assert_eq!((managed.validity().not_after.timestamp() - managed.validity().not_before.timestamp()) / 86_400, 14);
    // A CSR: its key stays with the device; only its key and names are taken, never what else it
    // asks for (here, to be a CA itself and to sign code).
    let device = rcgen::KeyPair::generate().unwrap();
    let csr = |names: &[&str]| {
        let mut p = rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::CodeSigning];
        p.serialize_request(&device).unwrap().pem().unwrap()
    };
    let at = format!("/cas/{}/sign", ca["id"].as_str().unwrap());
    let sign = |csr: String, days: u32| api.call(Method::POST, &at, Some(json!({ "csr": csr, "days": days })));
    let (s, signed) = sign(csr(&["sensor-7.svc.internal"]), 3).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let (_, der) = parse_x509_pem(signed["certificate"].as_str().unwrap().as_bytes()).unwrap();
    let (_, leaf) = parse_x509_certificate(&der.contents).unwrap();
    leaf.verify_signature(Some(ca_cert.public_key())).expect("signed by the project's CA");
    assert_eq!(leaf.public_key().subject_public_key.data.as_ref(), rcgen::PublicKeyData::der_bytes(&device), "the device's own key");
    assert!(!leaf.is_ca() && !leaf.extended_key_usage().unwrap().unwrap().value.code_signing, "not what the CSR asked beyond its names");
    assert_eq!((leaf.validity().not_after.timestamp() - leaf.validity().not_before.timestamp()) / 86_400, 3);
    assert_eq!(sign(csr(&["sensor-7.example.com"]), 3).await.0, StatusCode::BAD_REQUEST, "a name outside the CA's domains");
    assert_eq!(sign(csr(&["sensor-7.svc.internal"]), 91).await.0, StatusCode::BAD_REQUEST, "longer than the CA allows");
    // Revoking: a lost device's certificate by hand, a lease's when it ends. The CA's CRL (public)
    // lists both, and every certificate names where it is.
    let id = ca["id"].as_str().unwrap();
    let crl_url = format!("https://roll.example/v1/secrets/crl/{id}");
    assert!(der.contents.windows(crl_url.len()).any(|w| w == crl_url.as_bytes()), "the CRL distribution point");
    let listed = api.ok(Method::GET, &format!("/cas/{id}/certificates"), None).await;
    assert!(listed.as_array().unwrap().iter().any(|c| c["serial"] == signed["serial"] && c["source"] == "csr") && listed.as_array().unwrap().iter().any(|c| c["source"] == "link"), "{listed}");
    let revoke_at = format!("/cas/{id}/revoke");
    let revoke = |serial: Value| api.call(Method::POST, &revoke_at, Some(json!({ "serial": serial })));
    assert_eq!(revoke(signed["serial"].clone()).await.0, StatusCode::NO_CONTENT);
    assert_eq!(revoke(json!("00")).await.0, StatusCode::NOT_FOUND);
    // A CRL fetched now is kept a minute; a revocation still shows in the next one.
    assert_eq!(api.call(Method::GET, &format!("/v1/secrets/crl/{id}?format=pem"), None).await.0, StatusCode::OK);
    let lease_cert = parse_x509_pem(cert.as_bytes()).unwrap().1;
    let lease_serial = parse_x509_certificate(&lease_cert.contents).unwrap().1.raw_serial().to_vec();
    assert_eq!(api.call(Method::DELETE, &format!("/leases/{}", r["leases"][0].as_str().unwrap()), None).await.0, StatusCode::NO_CONTENT);
    let (s, crl) = api.call(Method::GET, &format!("/v1/secrets/crl/{id}?format=pem"), None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, pem) = parse_x509_pem(crl.as_str().unwrap().as_bytes()).unwrap();
    let (_, crl) = parse_x509_crl(&pem.contents).unwrap();
    crl.verify_signature(ca_cert.public_key()).expect("signed by the CA");
    let revoked: Vec<Vec<u8>> = crl.iter_revoked_certificates().map(|r| r.raw_serial().to_vec()).collect();
    assert!(revoked.contains(&leaf.raw_serial().to_vec()) && revoked.contains(&lease_serial), "both revoked: {revoked:?}");
    // A CA whose revocations still matter keeps serving them: it can't be deleted until they end.
    let devices = api.ok(Method::POST, "/cas", Some(json!({ "name": "devices", "domains": ["dev.internal"] }))).await;
    let dev_id = devices["id"].as_str().unwrap();
    let one = api.ok(Method::POST, &format!("/cas/{dev_id}/sign"), Some(json!({ "csr": csr(&["d1.dev.internal"]), "days": 2 }))).await;
    api.call(Method::POST, &format!("/cas/{dev_id}/revoke"), Some(json!({ "serial": one["serial"] }))).await;
    let (s, e) = api.call(Method::DELETE, &format!("/cas/{dev_id}"), None).await;
    assert!(s == StatusCode::CONFLICT && e.to_string().contains("revocation list"), "{e}");
    // A CSR's certificate ending within two weeks is health's to say (nothing renews it); a
    // revoked one isn't.
    let d2 = api.ok(Method::POST, &format!("/cas/{dev_id}/sign"), Some(json!({ "csr": csr(&["d2.dev.internal"]), "days": 5 }))).await;
    let health = api.ok(Method::GET, "/health", None).await;
    let ending: Vec<&Value> = health.as_array().unwrap().iter().filter(|f| f["issue"] == "certificate_expiring").collect();
    assert_eq!(ending.iter().map(|f| (f["key"].as_str().unwrap(), f["path"].as_str().unwrap())).collect::<Vec<_>>(), [("d2.dev.internal", "devices")], "{health}");
    // OCSP, beside the CRL: one revoked and one good certificate, answered and signed by the CA.
    {
        use der::{Decode, Encode};
        let ca_pem = devices["certificate"].as_str().unwrap();
        let (_, pem) = parse_x509_pem(ca_pem.as_bytes()).unwrap();
        let ca = x509_cert::Certificate::from_der(&pem.contents).unwrap();
        let sha1 = |b: &[u8]| der::asn1::OctetString::new(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, b).as_ref().to_vec()).unwrap();
        let (name_hash, key_hash) = (sha1(&ca.tbs_certificate.subject.to_der().unwrap()), sha1(ca.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes()));
        let ask = |serial: &str| x509_ocsp::Request {
            req_cert: x509_ocsp::CertId {
                hash_algorithm: x509_cert::spki::AlgorithmIdentifierOwned { oid: der::oid::ObjectIdentifier::new_unwrap("1.3.14.3.2.26"), parameters: None },
                issuer_name_hash: name_hash.clone(),
                issuer_key_hash: key_hash.clone(),
                serial_number: x509_cert::serial_number::SerialNumber::new(&(0..serial.len()).step_by(2).map(|i| u8::from_str_radix(&serial[i..i + 2], 16).unwrap()).collect::<Vec<_>>()).unwrap(),
            },
            single_request_extensions: None,
        };
        let request = x509_ocsp::OcspRequest { tbs_request: x509_ocsp::TbsRequest { version: Default::default(), requestor_name: None, request_list: vec![ask(one["serial"].as_str().unwrap()), ask(d2["serial"].as_str().unwrap())], request_extensions: None }, optional_signature: None };
        let req = Request::builder().method(Method::POST).uri(format!("/v1/secrets/ocsp/{dev_id}")).body(Body::from(request.to_der().unwrap())).unwrap();
        let resp = api.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let answer = x509_ocsp::OcspResponse::from_der(&bytes).unwrap();
        let basic = x509_ocsp::BasicOcspResponse::from_der(answer.response_bytes.unwrap().response.as_bytes()).unwrap();
        let statuses: Vec<&str> = basic.tbs_response_data.responses.iter().map(|r| match r.cert_status { x509_ocsp::CertStatus::Good(_) => "good", x509_ocsp::CertStatus::Revoked(_) => "revoked", x509_ocsp::CertStatus::Unknown(_) => "unknown" }).collect();
        assert_eq!(statuses, ["revoked", "good"]);
        let public = ca.tbs_certificate.subject_public_key_info.subject_public_key.raw_bytes();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, public).verify(&basic.tbs_response_data.to_der().unwrap(), basic.signature.raw_bytes()).expect("signed by the CA");
    }
    // A CA a key links to stays.
    assert_eq!(api.call(Method::DELETE, &format!("/cas/{}", ca["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_ssh_ca_signs_short_lived_user_certificates_for_its_principals() {
    use base64::Engine;
    use ed25519_dalek::Verifier;
    let api = Api::new().await;
    let ca = api.ok(Method::POST, "/ssh-cas", Some(json!({ "name": "ops", "principals": ["deploy", "ubuntu"], "max_hours": 8 }))).await;
    let ca_pub = ca["public_key"].as_str().unwrap();
    assert!(ca_pub.starts_with("ssh-ed25519 AAAA"), "{ca_pub}");
    let link = |field: &str, principals: &str| json!({ "kind": "ssh", "ref": ca["id"], "field": field, "mode": "dynamic", "options": { "principals": principals, "ttl_minutes": 30 } });
    // A login the CA doesn't sign for is refused when it's linked.
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/ROOT_CERT", Some(json!({ "link": link("certificate", "root") }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("doesn't sign for root"), "{e}");
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "SSH_CERT", "link": link("certificate", "deploy") }, { "op": "set", "key": "SSH_KEY", "link": link("private_key", "deploy") }] }))).await;
    let r = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "job:release" }))).await;
    let (cert, key) = (r["values"]["SSH_CERT"].as_str().unwrap(), r["values"]["SSH_KEY"].as_str().unwrap());
    assert!(key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    // The certificate: signed by the CA's key over everything before its signature, for `deploy`,
    // for the lease's TTL plus five minutes.
    let blob = base64::engine::general_purpose::STANDARD.decode(cert.split(' ').nth(1).unwrap()).unwrap();
    fn take(b: &[u8], p: &mut usize) -> Vec<u8> {
        let n = u32::from_be_bytes(b[*p..*p + 4].try_into().unwrap()) as usize;
        let s = b[*p + 4..*p + 4 + n].to_vec();
        *p += 4 + n;
        s
    }
    let mut p = 0usize;
    assert_eq!(take(&blob, &mut p), b"ssh-ed25519-cert-v01@openssh.com");
    take(&blob, &mut p); // nonce
    take(&blob, &mut p); // the user's key
    p += 8 + 4; // serial, type
    take(&blob, &mut p); // key id
    assert_eq!(&take(&blob, &mut p)[4..], b"deploy");
    let (after, before) = (u64::from_be_bytes(blob[p..p + 8].try_into().unwrap()), u64::from_be_bytes(blob[p + 8..p + 16].try_into().unwrap()));
    assert_eq!(before - after, 36 * 60, "the lease's TTL plus five minutes, and a minute of skew");
    p += 16;
    for _ in 0..3 {
        take(&blob, &mut p); // critical options, extensions, reserved
    }
    let signer = take(&blob, &mut p);
    let signed_to = p;
    let sig = take(&blob, &mut p);
    let ca_key = base64::engine::general_purpose::STANDARD.decode(ca_pub.split(' ').nth(1).unwrap()).unwrap();
    assert_eq!(signer, ca_key, "signed by the project's CA");
    let public = ed25519_dalek::VerifyingKey::from_bytes(ca_key[ca_key.len() - 32..].try_into().unwrap()).unwrap();
    public.verify(&blob[..signed_to], &ed25519_dalek::Signature::from_slice(&sig[sig.len() - 64..]).unwrap()).expect("the CA's signature");
    assert_eq!(api.call(Method::DELETE, &format!("/ssh-cas/{}", ca["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    // Host certificates: a server's own key signed for names under the CA's host domains, and the
    // known_hosts line that trusts them.
    for public in ["com", "co.uk"] {
        assert_eq!(api.call(Method::POST, "/ssh-cas", Some(json!({ "name": "wide", "principals": [], "host_domains": [public] }))).await.0, StatusCode::BAD_REQUEST, "{public}");
    }
    let hosts = api.ok(Method::POST, "/ssh-cas", Some(json!({ "name": "fleet", "principals": [], "host_domains": ["svc.internal"] }))).await;
    let hosts_pub = hosts["public_key"].as_str().unwrap();
    assert_eq!(hosts["known_hosts"], format!("@cert-authority *.svc.internal,svc.internal {hosts_pub}"));
    let server = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]);
    let mut server_blob = vec![];
    for part in [&b"ssh-ed25519"[..], &server.verifying_key().to_bytes()[..]] {
        server_blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        server_blob.extend_from_slice(part);
    }
    let line = format!("ssh-ed25519 {} root@db1", base64::engine::general_purpose::STANDARD.encode(&server_blob));
    let sign_at = format!("/ssh-cas/{}/sign-host", hosts["id"].as_str().unwrap());
    let sign = |hosts: Value| api.call(Method::POST, &sign_at, Some(json!({ "public_key": line, "hosts": hosts, "days": 30 })));
    assert_eq!(sign(json!(["db1.example.com"])).await.0, StatusCode::BAD_REQUEST, "a name outside its domains");
    let (s, signed) = sign(json!(["db1.svc.internal"])).await;
    assert_eq!(s, StatusCode::OK, "{signed}");
    let blob = base64::engine::general_purpose::STANDARD.decode(signed["certificate"].as_str().unwrap().split(' ').nth(1).unwrap()).unwrap();
    let mut p = 0usize;
    assert_eq!(take(&blob, &mut p), b"ssh-ed25519-cert-v01@openssh.com");
    take(&blob, &mut p); // nonce
    assert_eq!(take(&blob, &mut p), server.verifying_key().to_bytes(), "the server's own key");
    p += 8;
    assert_eq!(u32::from_be_bytes(blob[p..p + 4].try_into().unwrap()), 2, "a host certificate");
    p += 4;
    take(&blob, &mut p); // key id
    assert_eq!(&take(&blob, &mut p)[4..], b"db1.svc.internal");
    p += 16;
    for _ in 0..3 {
        take(&blob, &mut p);
    }
    let signer = take(&blob, &mut p);
    let signed_to = p;
    let sig = take(&blob, &mut p);
    let fleet_key = base64::engine::general_purpose::STANDARD.decode(hosts_pub.split(' ').nth(1).unwrap()).unwrap();
    assert_eq!(signer, fleet_key);
    ed25519_dalek::VerifyingKey::from_bytes(fleet_key[fleet_key.len() - 32..].try_into().unwrap()).unwrap().verify(&blob[..signed_to], &ed25519_dalek::Signature::from_slice(&sig[sig.len() - 64..]).unwrap()).expect("the CA's signature");
    // A person's own key (in an agent, on a token): a user certificate for logins the CA allows.
    let user_at = format!("/ssh-cas/{}/sign-user", ca["id"].as_str().unwrap());
    assert_eq!(api.call(Method::POST, &user_at, Some(json!({ "public_key": line, "principals": ["root"] }))).await.0, StatusCode::BAD_REQUEST);
    let mine = api.ok(Method::POST, &user_at, Some(json!({ "public_key": line, "principals": ["deploy"], "ttl_minutes": 30 }))).await;
    let blob = base64::engine::general_purpose::STANDARD.decode(mine["certificate"].as_str().unwrap().split(' ').nth(1).unwrap()).unwrap();
    let mut p = 0usize;
    take(&blob, &mut p);
    take(&blob, &mut p);
    assert_eq!(take(&blob, &mut p), server.verifying_key().to_bytes(), "the person's own key");
    p += 8;
    assert_eq!(u32::from_be_bytes(blob[p..p + 4].try_into().unwrap()), 1, "a user certificate");
}

#[tokio::test]
async fn a_sensitive_keys_value_reaches_only_roll_s_runtimes() {
    let api = Api::new().await;
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "PAYMENTS_KEY", "value": "sk_live_sensitive", "tags": ["sensitive"] }, { "op": "set", "key": "WRAPPED", "value": "Bearer ${PAYMENTS_KEY}" }] }))).await;
    // Not to a person or a token: left out of values (named in `withheld`), refused on its own, not
    // carried by a reference, not in history.
    let v = api.values("production", "/").await;
    assert!(v["values"].get("PAYMENTS_KEY").is_none() && v["withheld"] == json!(["PAYMENTS_KEY"]), "{v}");
    assert!(v["problems"].to_string().contains("PAYMENTS_KEY is sensitive"), "{v}");
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/PAYMENTS_KEY/value", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.call(Method::GET, "/environments/production/secrets/PAYMENTS_KEY/versions/1/value", None).await.0, StatusCode::FORBIDDEN);
    // Sensitive is for good.
    let (s, e) = api.call(Method::PUT, "/environments/production/secrets/PAYMENTS_KEY", Some(json!({ "tags": [] }))).await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("sensitive for good"), "{e}");
    // roll's own runtimes (apps, previews, checks) get it, and the reference resolves for them.
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let got = tokio::task::spawn_blocking(move || s.values(&p, "production", "/", true, Some("app:web"), &roll_secrets::Caller::consumer("app:web")).unwrap()).await.unwrap();
    assert_eq!((got.values["PAYMENTS_KEY"].expose(), got.values["WRAPPED"].expose()), ("sk_live_sensitive", "Bearer sk_live_sensitive"));
    // …and are told which values never to show, a key that holds one through a reference too.
    assert_eq!(got.sensitive, ["PAYMENTS_KEY", "WRAPPED"]);
    // A sandbox's or a check's resolve isn't a runtime's.
    let (s, p) = (api.secrets.clone(), api.project.clone());
    let sandbox = tokio::task::spawn_blocking(move || s.values(&p, "production", "/", true, Some("sandbox:x"), &roll_secrets::Caller::consumer("sandbox:x").not_runtime()).unwrap()).await.unwrap();
    assert!(!sandbox.values.contains_key("PAYMENTS_KEY"));

    // Tagging a key that already has a value: its older versions hold the value too, so none shows.
    api.ok(Method::PUT, "/environments/staging/secrets/OLD_KEY", Some(json!({ "value": "sk_live_old" }))).await;
    api.ok(Method::PUT, "/environments/staging/secrets/OLD_KEY", Some(json!({ "tags": ["sensitive"] }))).await;
    assert_eq!(api.call(Method::GET, "/environments/staging/secrets/OLD_KEY/versions/1/value", None).await.0, StatusCode::FORBIDDEN);
    // No test of a guess: the leak scan, a local file and another environment's copy don't say.
    assert_eq!(api.ok(Method::POST, "/leaks", Some(json!({ "candidates": ["sk_live_old"] }))).await, json!([]));
    api.ok(Method::PUT, "/environments/development/secrets/OLD_KEY", Some(json!({ "value": "sk_live_old" }))).await;
    let cmp = api.ok(Method::GET, "/compare?from=development&to=staging", None).await;
    assert_eq!(cmp["rows"].as_array().unwrap().iter().find(|r| r["key"] == "OLD_KEY").unwrap()["status"], "hidden");
}

#[tokio::test]
async fn an_encryption_key_encrypts_for_apps_and_never_leaves_roll() {
    use base64::Engine;
    let b64 = |s: &[u8]| base64::engine::general_purpose::STANDARD.encode(s);
    let api = Api::new().await;
    let k = api.ok(Method::POST, "/transit", Some(json!({ "name": "cards" }))).await;
    assert_eq!(k["latest_version"], 1);
    assert!(!api.ok(Method::GET, "/transit", None).await.to_string().contains("\"key\""), "no key material in a listing");
    // Encrypt bound to a context; it opens under that context only.
    let one = api.ok(Method::POST, "/transit/cards/encrypt", Some(json!({ "plaintext": b64(b"4111 1111 1111 1111"), "context": "order:42" }))).await;
    let ct = one["ciphertext"].as_str().unwrap().to_string();
    assert!(ct.starts_with("roll:v1:"), "{ct}");
    let open = |ct: &str, context: &str| api.call(Method::POST, "/transit/cards/decrypt", Some(json!({ "ciphertext": ct, "context": context })));
    assert_eq!(open(&ct, "order:42").await.1["plaintext"], b64(b"4111 1111 1111 1111"));
    assert_eq!(open(&ct, "order:43").await.0, StatusCode::BAD_REQUEST);
    // Rotation: new ciphertexts use v2, old ones still open, rewrap moves them without the plaintext.
    assert_eq!(api.ok(Method::POST, "/transit/cards/rotate", None).await["latest_version"], 2);
    let re = api.ok(Method::POST, "/transit/cards/rewrap", Some(json!({ "items": [{ "ciphertext": ct, "context": "order:42" }] }))).await;
    let ct2 = re["items"][0]["ciphertext"].as_str().unwrap().to_string();
    assert!(ct2.starts_with("roll:v2:") && open(&ct2, "order:42").await.1["plaintext"] == b64(b"4111 1111 1111 1111"));
    // Retiring v1: what it encrypted no longer opens.
    api.ok(Method::PATCH, "/transit/cards", Some(json!({ "min_decrypt_version": 2 }))).await;
    let (s, e) = open(&ct, "order:42").await;
    assert!(s == StatusCode::BAD_REQUEST && e.to_string().contains("retired"), "{e}");
    // A data key for local envelope encryption: its ciphertext opens to it.
    let dk = api.ok(Method::POST, "/transit/cards/datakey", Some(json!({}))).await;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(dk["plaintext"].as_str().unwrap()).unwrap().len(), 32);
    assert_eq!(api.call(Method::POST, "/transit/cards/decrypt", Some(json!({ "ciphertext": dk["ciphertext"] }))).await.1["plaintext"], dk["plaintext"]);
    // Signatures: by the newest version, checked here or by anyone with the public key.
    let sig = api.ok(Method::POST, "/transit/cards/sign", Some(json!({ "input": b64(b"invoice 7") }))).await["signature"].as_str().unwrap().to_string();
    assert!(sig.starts_with("roll:v2:"), "{sig}");
    let check = |input: &[u8], sig: &str| api.ok(Method::POST, "/transit/cards/verify", Some(json!({ "items": [{ "input": b64(input), "signature": sig }] })));
    assert_eq!(check(b"invoice 7", &sig).await["items"][0]["valid"], true);
    assert_eq!(check(b"invoice 8", &sig).await["items"][0]["valid"], false);
    assert_eq!(check(b"invoice 7", &sig.replace("roll:v2:", "roll:v9:")).await["items"][0]["valid"], false, "an unknown version doesn't verify");
    let public = api.ok(Method::GET, "/transit/cards/public", None).await;
    let raw = base64::engine::general_purpose::STANDARD.decode(public[1]["public_key"].as_str().unwrap()).unwrap();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&sig["roll:v2:".len()..]).unwrap();
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, raw).verify(b"invoice 7", &bytes).expect("a stock Ed25519 check passes");
    // HMACs: stable for one input (a blind index), checked only by writers.
    let mac = |input: &[u8]| api.ok(Method::POST, "/transit/cards/hmac", Some(json!({ "input": b64(input) })));
    let m = mac(b"ada@example.com").await["hmac"].clone();
    assert_eq!(mac(b"ada@example.com").await["hmac"], m);
    assert_ne!(mac(b"bob@example.com").await["hmac"], m);
    assert_eq!(api.ok(Method::POST, "/transit/cards/hmac/verify", Some(json!({ "input": b64(b"ada@example.com"), "hmac": m }))).await["valid"], true);
    // A backup carries the key: restored into another project, it still opens what it encrypted.
    let secret = [9u8; 32];
    let b = api.ok(Method::POST, "/backup", Some(json!({ "recipient": roll_secrets::crypto::public_key(&secret) }))).await;
    let sealed: roll_secrets::crypto::Sealed = serde_json::from_value(b["sealed"].clone()).unwrap();
    let archive: Value = serde_json::from_slice(&roll_secrets::crypto::open_sealed(&secret, b["context"].as_str().unwrap(), &sealed).unwrap()).unwrap();
    let (_, other) = api.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "restored" }))).await;
    let to = |p: &str| format!("/v1/secrets/projects/{}{p}", other["id"].as_str().unwrap());
    api.ok(Method::POST, &to("/restore"), Some(archive)).await;
    let back = api.call(Method::POST, &to("/transit/cards/decrypt"), Some(json!({ "ciphertext": ct2, "context": "order:42" }))).await;
    assert_eq!(back.1["plaintext"], b64(b"4111 1111 1111 1111"), "{back:?}");
    assert_eq!(api.ok(Method::POST, &to("/transit/cards/sign"), Some(json!({ "input": b64(b"invoice 7") }))).await["signature"], sig.as_str(), "and signs as it did");
    // A key brought from Vault: Vault's own ciphertexts (aes256-gcm96, no AAD) open under the
    // version that was Vault's, and rewrap into roll's form.
    let vault_ct = |key: &[u8; 32], plain: &[u8]| {
        use aes_gcm::aead::{Aead, KeyInit};
        let nonce = [7u8; 12];
        let mut raw = nonce.to_vec();
        raw.extend(aes_gcm::Aes256Gcm::new_from_slice(key).unwrap().encrypt(aes_gcm::Nonce::from_slice(&nonce), plain).unwrap());
        raw
    };
    let (k1, k2) = ([3u8; 32], [4u8; 32]);
    assert_eq!(api.call(Method::POST, "/transit", Some(json!({ "name": "legacy", "key": b64(&[1u8; 16]) }))).await.0, StatusCode::BAD_REQUEST, "not 32 bytes");
    let made = api.ok(Method::POST, "/transit", Some(json!({ "name": "legacy", "key": b64(&k1), "vault_version": 1 }))).await;
    assert_eq!((made["versions"][0]["imported"].clone(), made["versions"][0]["vault_version"].clone()), (json!(true), json!(1)));
    api.ok(Method::POST, "/transit/legacy/rotate", Some(json!({ "key": b64(&k2), "vault_version": 2 }))).await;
    for (key, n, plain) in [(&k1, 1, &b"from vault v1"[..]), (&k2, 2, &b"from vault v2"[..])] {
        let ct = format!("vault:v{n}:{}", b64(&vault_ct(key, plain)));
        assert_eq!(api.ok(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": ct }))).await["plaintext"], b64(plain));
        let moved = api.ok(Method::POST, "/transit/legacy/rewrap", Some(json!({ "ciphertext": ct }))).await["ciphertext"].as_str().unwrap().to_string();
        assert!(moved.starts_with("roll:v2:"), "{moved}");
        assert_eq!(api.ok(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": moved }))).await["plaintext"], b64(plain));
    }
    let wrong = format!("vault:v1:{}", b64(&vault_ct(&k2, b"x")));
    assert_eq!(api.call(Method::POST, "/transit/legacy/decrypt", Some(json!({ "ciphertext": wrong }))).await.0, StatusCode::BAD_REQUEST);
    // Deleting makes everything unreadable for good: only with the name repeated.
    assert_eq!(api.call(Method::DELETE, "/transit/cards", None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.call(Method::DELETE, "/transit/cards?confirm=cards", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(open(&ct2, "order:42").await.0, StatusCode::NOT_FOUND);
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
async fn a_customer_managed_key_wraps_the_data_key_and_revoking_it_locks_roll_out() {
    let api = Api::new().await;
    api.set("production", "STRIPE_KEY", "sk_live_123").await;
    let key = "arn:aws:kms:eu-west-1:123456789012:key/k1";
    // A key whose policy would wrap any project's data key (another customer's, say) is refused.
    let (s, e) = api.call(Method::PUT, "/kms", Some(json!({ "key": key }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"].as_str().unwrap().contains("doesn't pin the project"), "{e}");
    *api.kms.pinned.lock().unwrap() = Some(api.project.clone());
    let set = api.ok(Method::PUT, "/kms", Some(json!({ "key": key }))).await;
    assert_eq!(set["key"], key);
    assert_eq!(api.ok(Method::GET, "/kms", None).await["key"], key);
    let stored: String = api.db.read(|c| c.query_row("SELECT dek FROM projects WHERE project_id = ?", [api.project.as_str()], |r| r.get(0))).unwrap();
    assert!(stored.starts_with("kms:v1:"), "{stored}");
    assert_eq!(api.values("production", "/").await["values"]["STRIPE_KEY"], "sk_live_123", "the values didn't change");
    let node = |keyring: Keyring| {
        let opts = Options { keyring: Ok(keyring), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: Some(api.kms.clone()), sts: None, outside: None, clouds: None };
        Arc::new(Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap())
    };
    // The deployment key rotates: health counts the wrapped project as not resealed until the
    // sweeper reseals it inside its wrap (the previous key can't be dropped before).
    let rotated = node(Keyring::new(&"n".repeat(32), Some(&"k".repeat(32))).unwrap());
    assert_eq!(rotated.health().unwrap()["resealed"], json!(0));
    rotated.sweep().unwrap();
    assert_eq!(rotated.health().unwrap()["resealed"], "all");
    let fresh = node(Keyring::new(&"n".repeat(32), None).unwrap());
    let values = |s: Arc<Secrets>| {
        let req = Request::get(format!("/v1/secrets/projects/{}/environments/production/values", api.project)).body(Body::empty()).unwrap();
        async move {
            let res = roll_secrets::router(s).oneshot(req).await.unwrap();
            (res.status(), String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
        }
    };
    assert!(values(fresh.clone()).await.1.contains("sk_live_123"), "opens with the new deployment key alone");
    // The customer revokes it: a node without the key in memory is refused (423, not an outage).
    *api.kms.revoked.lock().unwrap() = true;
    let (status, body) = values(node(Keyring::new(&"n".repeat(32), None).unwrap())).await;
    assert_eq!(status, StatusCode::LOCKED);
    assert!(body.contains("kms_denied") && !body.contains("sk_live"), "{body}");
    // Nor can the wrapping be changed then, even by a node that has the key in memory.
    assert_eq!(roll_secrets::router(fresh).oneshot(Request::delete(format!("/v1/secrets/projects/{}/kms", api.project)).body(Body::empty()).unwrap()).await.unwrap().status(), StatusCode::LOCKED);
    // Back with roll alone: no KMS needed any more.
    *api.kms.revoked.lock().unwrap() = false;
    let opts = Options { keyring: Ok(Keyring::new(&"n".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: Some(api.kms.clone()), sts: None, outside: None, clouds: None };
    let after = Arc::new(Secrets::new(api.db.clone(), api.ws.clone(), opts).unwrap());
    assert!(roll_secrets::router(after.clone()).oneshot(Request::delete(format!("/v1/secrets/projects/{}/kms", api.project)).body(Body::empty()).unwrap()).await.unwrap().status().is_success());
    *api.kms.revoked.lock().unwrap() = true;
    assert!(values(after).await.1.contains("sk_live_123"));
}

#[tokio::test]
async fn an_aws_role_gives_each_lease_a_short_lived_session() {
    let api = Api::new().await;
    let role = json!({ "name": "deploy", "role_arn": "arn:aws:iam::123456789012:role/roll" });
    // A role whose trust policy takes any ExternalId (another customer's that trusts roll, say)
    // is refused: it must pin this project's.
    let (s, e) = api.call(Method::POST, "/aws-roles", Some(role.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"].as_str().unwrap().contains(&format!("sts:ExternalId = roll-secrets:{}", api.project)), "{e}");
    *api.sts.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let made = api.ok(Method::POST, "/aws-roles", Some(role)).await;
    let id = made["id"].as_str().unwrap().to_string();
    // Its three keys share one session per resolve; each resolve gets a session of its own.
    for (key, field) in [("AWS_ACCESS_KEY_ID", "access_key_id"), ("AWS_SECRET_ACCESS_KEY", "secret_access_key"), ("AWS_SESSION_TOKEN", "session_token")] {
        api.ok(Method::PUT, &format!("/environments/production/secrets/{key}"), Some(json!({ "link": { "kind": "aws", "ref": id, "field": field, "mode": "dynamic", "options": { "ttl_minutes": 15 } } }))).await;
    }
    let before = *api.sts.sessions.lock().unwrap();
    let a = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    let b = api.ok(Method::POST, "/environments/production/resolve", Some(json!({ "consumer": "app:web" }))).await;
    assert_eq!(*api.sts.sessions.lock().unwrap() - before, 2, "one session per resolve, for all three keys");
    let n = a["values"]["AWS_ACCESS_KEY_ID"].as_str().unwrap().trim_start_matches("ASIA").to_string();
    assert_eq!((a["values"]["AWS_SECRET_ACCESS_KEY"].as_str(), a["values"]["AWS_SESSION_TOKEN"].as_str()), (Some(format!("secret-{n}").as_str()), Some(format!("token-{n}").as_str())));
    assert_ne!(a["values"]["AWS_SESSION_TOKEN"], b["values"]["AWS_SESSION_TOKEN"]);
    assert_eq!(a["dynamic"].as_array().map(Vec::len), Some(3));
    // An AWS session can't be extended: resolve again for a new one.
    let lease = a["leases"][0].as_str().unwrap();
    let (s, _) = api.call(Method::POST, &format!("/leases/{lease}/renew"), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A role keys still link to stays.
    assert_eq!(api.call(Method::DELETE, &format!("/aws-roles/{id}"), None).await.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_iam_users_access_keys_take_turns() {
    let api = Api::new().await;
    *api.sts.pinned.lock().unwrap() = Some(format!("roll-secrets:{}", api.project));
    let role = api.ok(Method::POST, "/aws-roles", Some(json!({ "name": "keys", "role_arn": "arn:aws:iam::123456789012:role/roll" }))).await;
    let (s, _) = api.call(Method::POST, "/aws-users", Some(json!({ "name": "ci", "role": role["id"], "user": "nobody" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a user the role can't see is refused");
    let user = api.ok(Method::POST, "/aws-users", Some(json!({ "name": "ci", "role": role["id"], "user": "legacy-ci" }))).await;
    let link = |field: &str| json!({ "kind": "aws_iam_user", "ref": user["id"], "field": field, "mode": "managed" });
    api.ok(Method::POST, "/environments/production/changes", Some(json!({ "ops": [{ "op": "set", "key": "AWS_ACCESS_KEY_ID", "link": link("access_key_id") }, { "op": "set", "key": "AWS_SECRET_ACCESS_KEY", "link": link("secret_access_key") }] }))).await;
    let key = || async { api.values("production", "/").await["values"]["AWS_ACCESS_KEY_ID"].as_str().unwrap().to_string() };
    let keys = || api.sts.keys.lock().unwrap().0.clone();
    assert_eq!(key().await, "AKIA1");
    // A rotation makes the second key; the first works through its grace.
    api.ok(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!((key().await, keys()), ("AKIA2".to_string(), vec!["AKIA1".to_string(), "AKIA2".to_string()]));
    // Both slots are taken: another rotation is refused, and nothing is deleted to make room.
    let (s, e) = api.call(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!((key().await, keys().len()), ("AKIA2".to_string(), 2));
    // The grace ends: the old key goes, and the slot is free again.
    let p = api.project.clone();
    api.db.write(move |c| c.execute("UPDATE leases SET expires_at = 0 WHERE project_id = ?", [p.as_str()])).unwrap();
    let s = api.secrets.clone();
    tokio::task::spawn_blocking(move || s.sweep().unwrap()).await.unwrap();
    assert_eq!(keys(), ["AKIA2"]);
    api.ok(Method::POST, "/environments/production/secrets/AWS_ACCESS_KEY_ID/rotate", None).await;
    assert_eq!(key().await, "AKIA3");
    // What's in use stays.
    assert_eq!(api.call(Method::DELETE, &format!("/aws-roles/{}", role["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
    assert_eq!(api.call(Method::DELETE, &format!("/aws-users/{}", user["id"].as_str().unwrap()), None).await.0, StatusCode::CONFLICT);
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
    // The cluster rotates its signing key: the identity's keys are replaced, its id kept.
    let rotated = Issuer::new();
    let (_, before) = api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await;
    let k = api.ok(Method::PUT, &format!("/identities/{}/jwks", p["id"].as_str().unwrap()), Some(json!({ "jwks": rotated.jwks().to_string() }))).await;
    assert_eq!((k["id"].clone(), k["pinned_keys"].clone()), (p["id"].clone(), json!(1)));
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&rotated))).await.0, StatusCode::OK);
    assert_eq!(api.call(Method::POST, "/v1/secrets/login", Some(pod(&cluster))).await.0, StatusCode::UNAUTHORIZED, "the old key no longer");
    // What it gave out under the old key ends with it (a rotation is often for a leaked key).
    let (s, _) = api.req(Method::GET, "/environments/staging/values", None, before["token"].as_str()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

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
