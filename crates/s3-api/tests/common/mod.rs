//! An in-process roll storage with its S3 API and developer API, and an HTTP client for it.
#![allow(dead_code)]

use s3_store::{AuthUpdate, Backend, Secret, Store, StoreConfig};
use serde_json::{Value, json};
use std::sync::Arc;

pub const SECRET: &str = "test-jwt-secret-with-at-least-thirty-two-chars";
pub const ACCESS: &str = "rollapitestadmin";
pub const ACCESS_SECRET: &str = "rollapitestsecret0123456789";

pub struct T {
    pub dir: tempfile::TempDir,
    pub backend: Arc<dyn Backend>,
    pub store: Arc<Store>,
    pub server: Arc<s3_server::Server>,
    pub api: Arc<s3_api::Api>,
    pub url: String,
    pub s3: String,
    pub account: String,
    pub anon: String,
    pub service: String,
    pub http: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for T {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub fn store_config(dir: &std::path::Path) -> StoreConfig {
    StoreConfig::new(dir.join("meta")).manual()
}

pub async fn start() -> T {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-api").await;
    start_on(dir, backend).await
}

/// A server over `dir`'s metadata and `backend` (again, after a "restart").
pub async fn start_on(dir: tempfile::TempDir, backend: Arc<dyn Backend>) -> T {
    let store = Store::open(store_config(dir.path()), backend.clone()).await.unwrap();
    store.ensure_key(ACCESS, ACCESS_SECRET, "admin").await.unwrap();
    let account = store.default_account_id();
    let project = store.set_api_auth(&account, AuthUpdate { jwt_secret: Some(Some(Secret::new(SECRET))), ..Default::default() }).await.unwrap();
    let spool = dir.path().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let server = s3_server::Server::new(store.clone(), s3_server::Config { spool_dir: spool.clone(), ..Default::default() });
    let api = s3_api::Api::new(server.clone(), s3_api::Config { spool_dir: spool, sweep_interval: None, ..Default::default() });
    let s3l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let apil = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let s3 = format!("http://127.0.0.1:{}", s3l.local_addr().unwrap().port());
    let url = format!("http://127.0.0.1:{}/storage/v1", apil.local_addr().unwrap().port());
    let (srv, a) = (server.clone(), api.clone());
    let tasks = vec![
        tokio::spawn(async move {
            let _ = s3_server::serve(s3l, srv).await;
        }),
        tokio::spawn(async move {
            let _ = s3_api::serve(apil, a).await;
        }),
    ];
    T { dir, backend, store, server, api, url, s3, account, anon: project.anon_key.expose().to_string(), service: project.service_key.expose().to_string(), http: reqwest::Client::new(), tasks }
}

impl T {
    /// Stop everything and start again on the same metadata, object storage and spool (a
    /// gateway restart).
    pub async fn restart(mut self) -> T {
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().unwrap());
        let backend = self.backend.clone();
        for t in self.tasks.drain(..) {
            t.abort();
            let _ = t.await;
        }
        self.store.close().await;
        drop(self);
        start_on(dir, backend).await
    }
}

/// A user token (HS256 with the project's secret).
pub fn user_token(sub: &str, extra: Value) -> String {
    let mut claims = json!({ "sub": sub, "role": "authenticated", "exp": now_s() + 3600, "email": format!("{sub}@example.com") });
    if let (Some(c), Some(e)) = (claims.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            c.insert(k.clone(), v.clone());
        }
    }
    s3_store::api::jwt::sign_hs256(SECRET.as_bytes(), &claims)
}

pub fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Who a request comes from.
#[derive(Clone)]
pub enum As<'a> {
    Service,
    Anon,
    User(&'a str),
    Token(&'a str),
    Nobody,
}

pub struct Resp {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The Supabase error's `statusCode` (HTTP 400 with the real status in the body).
    pub fn code(&self) -> String {
        self.json()["statusCode"].as_str().unwrap_or_default().to_string()
    }

    pub fn h(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl T {
    pub fn req(&self, method: reqwest::Method, path: &str, who: As) -> reqwest::RequestBuilder {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{}{path}", self.url) };
        let b = self.http.request(method, url);
        let user;
        let (apikey, bearer) = match who {
            As::Service => (Some(self.service.as_str()), Some(self.service.as_str())),
            As::Anon => (Some(self.anon.as_str()), Some(self.anon.as_str())),
            As::User(sub) => {
                user = user_token(sub, json!({}));
                (Some(self.anon.as_str()), Some(user.as_str()))
            }
            As::Token(t) => (Some(self.anon.as_str()), Some(t)),
            As::Nobody => (None, None),
        };
        let b = match apikey {
            Some(k) => b.header("apikey", k),
            None => b,
        };
        match bearer {
            Some(t) => b.header("authorization", format!("Bearer {t}")),
            None => b,
        }
    }

    pub async fn send(&self, b: reqwest::RequestBuilder) -> Resp {
        let r = b.send().await.unwrap();
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let body = r.bytes().await.unwrap().to_vec();
        Resp { status, headers, body }
    }

    pub async fn json(&self, method: reqwest::Method, path: &str, who: As<'_>, body: Value) -> Resp {
        self.send(self.req(method, path, who).json(&body)).await
    }

    pub async fn get(&self, path: &str, who: As<'_>) -> Resp {
        self.send(self.req(reqwest::Method::GET, path, who)).await
    }

    pub async fn upload(&self, bucket: &str, key: &str, who: As<'_>, ct: &str, data: &[u8]) -> Resp {
        self.send(self.req(reqwest::Method::POST, &format!("/object/{bucket}/{key}"), who).header("content-type", ct).body(data.to_vec())).await
    }

    /// A bucket (by the service key).
    pub async fn bucket(&self, name: &str, public: bool) {
        let r = self.json(reqwest::Method::POST, "/bucket", As::Service, json!({ "name": name, "public": public })).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    pub async fn rules(&self, bucket: &str, preset: &str) {
        let r = self.json(reqwest::Method::POST, &format!("/roll/rules/{bucket}"), As::Service, json!({ "preset": preset })).await;
        assert_eq!(r.status, 200, "{}", r.text());
    }

    /// An S3 client with the admin key.
    pub fn s3_client(&self) -> aws_sdk_s3::Client {
        use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
        let conf = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&self.s3)
            .credentials_provider(Credentials::new(ACCESS, ACCESS_SECRET, None, None, "test"))
            .force_path_style(true)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }
}

/// Supabase's error body shape.
pub fn assert_error(r: &Resp, status: &str, code: &str) {
    assert_eq!(r.status, if status.starts_with('5') { status.parse().unwrap() } else { 400 }, "{}", r.text());
    let v = r.json();
    assert_eq!(v["statusCode"], status, "{v}");
    assert_eq!(v["code"], code, "{v}");
    assert!(v["error"].is_string() && v["message"].is_string(), "{v}");
}
