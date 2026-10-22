//! `/v1/storage/roles` and `POST /roles/{name}/sessions`: roles for temporary credentials, and
//! the credentials the control plane mints, used against the S3 API through `aws-sdk-s3`.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::ProvideErrorMetadata;
use reqwest::{Client, StatusCode};
use s3_control::{Control, ControlConfig};
use s3_store::{Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

struct T {
    api: String,
    s3: String,
    store: Arc<Store>,
    clock: Arc<AtomicI64>,
    http: Client,
    _dir: tempfile::TempDir,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

async fn start() -> T {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-control-roles").await;
    let clock = Arc::new(AtomicI64::new(now_ms()));
    let c = clock.clone();
    let mut sc = StoreConfig::new(dir.path().join("meta")).manual();
    sc.clock = Arc::new(move || c.load(Ordering::SeqCst));
    let store = Store::open(sc, backend).await.unwrap();
    let s3_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3_listener.local_addr().unwrap().port());
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: dir.path().to_path_buf(), ..Default::default() });
    tokio::spawn(async move { s3_server::serve(s3_listener, server).await.unwrap() });
    let control = Control::new(store.clone(), ControlConfig::local(s3.clone())).await.unwrap();
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://127.0.0.1:{}", api_listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(api_listener, s3_control::api::router(control)).await.unwrap() });
    T { api, s3, store, clock, http: Client::new(), _dir: dir }
}

impl T {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = self.http.request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), format!("{}{path}", self.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: not JSON: {text}")))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method, path, body).await;
        assert_eq!(s, StatusCode::OK, "{method} {path}: {v}");
        v
    }

    async fn err(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, String) {
        let (s, v) = self.call(method, path, body).await;
        assert!(!s.is_success(), "{method} {path} should fail: {v}");
        (s, v["error"].as_str().expect("an {error} body").to_string())
    }

    /// An S3 client with a minted session's credentials.
    fn s3_client(&self, s: &Value) -> aws_sdk_s3::Client {
        let creds = Credentials::new(s["access_key_id"].as_str().unwrap(), s["secret_access_key"].as_str().unwrap(), Some(s["session_token"].as_str().unwrap().to_string()), None, "control");
        let conf = aws_sdk_s3::Config::builder().behavior_version(BehaviorVersion::latest()).region(Region::new("us-east-1")).endpoint_url(&self.s3).credentials_provider(creds).force_path_style(true).build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}

async fn put(c: &aws_sdk_s3::Client, b: &str, k: &str) -> Result<(), String> {
    c.put_object().bucket(b).key(k).body(aws_sdk_s3::primitives::ByteStream::from_static(b"x")).send().await.map(|_| ()).map_err(|e| e.as_service_error().and_then(|s| s.code()).unwrap_or("?").to_string())
}

#[tokio::test]
async fn role_lifecycle() {
    let t = start().await;
    let key = t.ok("POST", "/v1/storage/keys", Some(json!({ "name": "backend", "permission": "write" }))).await;
    let key_id = key["id"].as_str().unwrap();
    let policy = json!({ "Version": "2012-10-17", "Statement": [{ "Effect": "Allow", "Action": "s3:*", "Resource": "arn:aws:s3:::uploads/*" }] });
    let r = t
        .ok(
            "POST",
            "/v1/storage/roles",
            Some(json!({
                "name": "browser-upload", "description": "Browser uploads", "permission": "write", "buckets": ["uploads"], "policy": policy,
                "max_session_s": 3600,
                "trust": { "keys": [key_id], "oidc": [{ "issuer": "https://accounts.google.com", "audiences": ["123.apps.googleusercontent.com"], "subject": "1*" }], "external_id": "ext-123" },
            })),
        )
        .await;
    let account = t.store.account_id();
    assert_eq!(r["arn"], format!("arn:aws:iam::{account}:role/browser-upload"));
    assert!(r["id"].as_str().unwrap().starts_with("RR"));
    assert_eq!(r["permission"], "write");
    assert_eq!(r["buckets"], json!(["uploads"]));
    assert_eq!(r["policy"]["Statement"][0]["Resource"], "arn:aws:s3:::uploads/*", "the policy comes back as JSON");
    assert_eq!(r["trust"]["oidc"][0]["audiences"][0], "123.apps.googleusercontent.com");
    assert_eq!(r["trust"]["external_id"], "ext-123");
    assert_eq!(r["generation"], 1);
    let listed = t.ok("GET", "/v1/storage/roles", None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(t.ok("GET", "/v1/storage/roles/browser-upload", None).await["name"], "browser-upload");

    // PATCH changes some fields; `null` clears; the generation goes up.
    let p = t.ok("PATCH", "/v1/storage/roles/browser-upload", Some(json!({ "permission": "read", "policy": null, "buckets": null }))).await;
    assert_eq!((p["permission"].as_str(), p["policy"].is_null(), p["buckets"].is_null(), p["generation"].as_i64()), (Some("read"), true, true, Some(2)));
    assert_eq!(p["trust"]["keys"][0], key_id, "untouched fields stay");
    let rv = t.ok("POST", "/v1/storage/roles/browser-upload/revoke", None).await;
    assert!(rv["revoked_before"].as_i64().unwrap() > 0);

    // Errors.
    let t = &t;
    let bad = |body: Value| async move { t.err("POST", "/v1/storage/roles", Some(body)).await };
    assert_eq!(bad(json!({ "name": "browser-upload", "permission": "read" })).await.0, StatusCode::CONFLICT);
    assert_eq!(bad(json!({ "name": "bad/name", "permission": "read" })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!({ "permission": "read" })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!({ "name": "r1", "permission": "root" })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!({ "name": "r1", "permission": "read", "colour": "red" })).await.0, StatusCode::BAD_REQUEST);
    assert!(bad(json!({ "name": "r1", "permission": "read", "trust": { "keys": ["RKNOSUCHKEY000000000"] } })).await.1.contains("no access key"));
    assert!(bad(json!({ "name": "r1", "permission": "read", "policy": { "Statement": [{ "Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "*" }] } })).await.1.contains("principal"));
    assert!(bad(json!({ "name": "r1", "permission": "read", "policy": { "Statement": [{ "Effect": "Maybe", "Action": "s3:*", "Resource": "*" }] } })).await.1.contains("policy"));
    assert_eq!(bad(json!({ "name": "r1", "permission": "read", "max_session_s": 50000 })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!({ "name": "r1", "permission": "read", "trust": { "oidc": [{ "issuer": "ftp://x", "audiences": ["a"] }] } })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(bad(json!({ "name": "r1", "permission": "read", "trust": { "oidc": [{ "issuer": "https://x", "audiences": [] }] } })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("PATCH", "/v1/storage/roles/browser-upload", Some(json!({ "name": "other" }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("PATCH", "/v1/storage/roles/nope", Some(json!({}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.err("GET", "/v1/storage/roles/nope", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(t.ok("DELETE", "/v1/storage/roles/browser-upload", None).await["deleted"], "browser-upload");
    assert_eq!(t.err("DELETE", "/v1/storage/roles/browser-upload", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_for_backends() {
    let t = start().await;
    for b in ["uploads", "private"] {
        t.ok("POST", "/v1/storage/buckets", Some(json!({ "name": b }))).await;
    }
    t.ok("POST", "/v1/storage/roles", Some(json!({ "name": "web", "permission": "write", "buckets": ["uploads", "private"], "max_session_s": 7200 }))).await;
    let s = t.ok("POST", "/v1/storage/roles/web/sessions", Some(json!({ "duration_s": 300, "buckets": ["uploads"], "session_name": "user-42", "tags": { "user": "42" } }))).await;
    assert!(s["access_key_id"].as_str().unwrap().starts_with("RT"));
    let exp = s["expiration"].as_i64().unwrap();
    assert!((exp - (now_ms() + 300_000)).abs() < 5_000);
    let account = t.store.account_id();
    assert_eq!(s["assumed_role_arn"], format!("arn:aws:sts::{account}:assumed-role/web/user-42"));
    assert_eq!(s["env"]["AWS_SESSION_TOKEN"], s["session_token"]);
    assert_eq!(s["env"]["AWS_ENDPOINT_URL"], t.s3.as_str());
    assert_eq!(s["buckets"], json!(["uploads"]));
    let c = t.s3_client(&s);
    put(&c, "uploads", "a.jpg").await.unwrap();
    assert_eq!(put(&c, "private", "a.jpg").await.unwrap_err(), "AccessDenied", "narrowed to one bucket");

    // A read-only session with a session policy.
    let policy = json!({ "Statement": [{ "Effect": "Allow", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::uploads/a.jpg" }] });
    let r = t.ok("POST", "/v1/storage/roles/web/sessions", Some(json!({ "permission": "read", "policy": policy }))).await;
    let rc = t.s3_client(&r);
    rc.get_object().bucket("uploads").key("a.jpg").send().await.unwrap();
    assert_eq!(put(&rc, "uploads", "b.jpg").await.unwrap_err(), "AccessDenied");

    // Expiry by the store's clock.
    t.clock.fetch_add(301_000, Ordering::SeqCst);
    assert_eq!(put(&c, "uploads", "late.jpg").await.unwrap_err(), "ExpiredToken");
    t.clock.store(now_ms(), Ordering::SeqCst);

    // Never more than the role.
    let t = &t;
    let bad = |body: Value| async move { t.err("POST", "/v1/storage/roles/web/sessions", Some(body)).await };
    assert!(bad(json!({ "permission": "admin" })).await.1.contains("more than the role"));
    assert!(bad(json!({ "buckets": ["elsewhere"] })).await.1.contains("outside the role"));
    assert!(bad(json!({ "duration_s": 7201 })).await.1.contains("duration_s"));
    assert!(bad(json!({ "duration_s": 10 })).await.1.contains("duration_s"));
    assert!(bad(json!({ "policy": { "Statement": "nope" } })).await.1.contains("policy"));
    assert!(bad(json!({ "tags": { "aws:x": "y" } })).await.1.contains("aws:"));
    assert!(bad(json!({ "session_name": "a b" })).await.1.contains("RoleSessionName"));
    assert_eq!(bad(json!({ "nope": 1 })).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(t.err("POST", "/v1/storage/roles/nope/sessions", Some(json!({}))).await.0, StatusCode::NOT_FOUND);

    // Revoking the role's sessions ends them; rotating the token key keeps them.
    let s2 = t.ok("POST", "/v1/storage/roles/web/sessions", Some(json!({}))).await;
    let c2 = t.s3_client(&s2);
    put(&c2, "uploads", "b").await.unwrap();
    let k1 = t.ok("POST", "/v1/storage/session-keys/rotate", None).await["key_id"].as_u64().unwrap();
    put(&c2, "uploads", "b").await.unwrap();
    assert_eq!(t.ok("POST", "/v1/storage/session-keys/rotate", None).await["key_id"].as_u64().unwrap(), k1 + 1);
    assert_eq!(put(&c2, "uploads", "b").await.unwrap_err(), "InvalidToken", "two rotations retire the key");
    t.clock.fetch_add(1, Ordering::SeqCst);
    let s3 = t.ok("POST", "/v1/storage/roles/web/sessions", Some(json!({}))).await;
    let c3 = t.s3_client(&s3);
    put(&c3, "uploads", "c").await.unwrap();
    t.ok("POST", "/v1/storage/roles/web/revoke", None).await;
    assert_eq!(put(&c3, "uploads", "c").await.unwrap_err(), "InvalidToken");
}
