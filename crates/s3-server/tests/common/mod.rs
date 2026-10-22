//! An in-process roll storage server and clients for it.
#![allow(dead_code)]

use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use s3_server::{Config, Server};
use s3_store::{Store, StoreConfig};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const ACCESS: &str = "rolltestadmin";
pub const SECRET: &str = "rolltestsecret0123456789";
pub const REGION: &str = "us-east-1";

pub struct TestServer {
    pub store: Arc<Store>,
    pub endpoint: String,
    pub port: u16,
    pub client: Client,
    pub dir: tempfile::TempDir,
}

pub async fn start() -> TestServer {
    start_with(|_| {}).await
}

pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let backend = s3_store::test_backend("s3-server").await;
    let store = Store::open(StoreConfig::new(dir.path().join("meta")).manual(), backend).await.unwrap();
    store.ensure_key(ACCESS, SECRET, "admin").await.unwrap();
    let mut cfg = Config { spool_dir: dir.path().to_path_buf(), ..Config::default() };
    tweak(&mut cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Server::new(store.clone(), cfg);
    tokio::spawn(async move { s3_server::serve(listener, server).await.unwrap() });
    let endpoint = format!("http://127.0.0.1:{port}");
    let client = client(&endpoint, ACCESS, SECRET);
    TestServer { store, endpoint, port, client, dir }
}

pub fn client(endpoint: &str, access: &str, secret: &str) -> Client {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(REGION))
        .endpoint_url(endpoint)
        .credentials_provider(Credentials::new(access, secret, None, None, "test"))
        .force_path_style(true)
        .build();
    Client::from_conf(conf)
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

pub fn md5_hex(b: &[u8]) -> String {
    use md5::Digest as _;
    hex::encode(md5::Md5::digest(b))
}

pub fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Deterministic test data.
pub fn data(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8).collect()
}

/// A raw request signed with SigV4 (every given header signed). `payload_hash` defaults to the
/// body's SHA-256.
pub struct Raw<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: Vec<(&'a str, String)>,
    pub body: Vec<u8>,
    pub payload_hash: Option<String>,
}

impl<'a> Raw<'a> {
    pub fn new(method: &'a str, path: &'a str) -> Self {
        Self { method, path, query: "", headers: Vec::new(), body: Vec::new(), payload_hash: None }
    }

    /// The `Authorization` value and the headers to send.
    pub fn sign(&self, t: &TestServer, access: &str, secret: &str) -> (String, Vec<(String, String)>) {
        let host = format!("127.0.0.1:{}", t.port);
        let amz_date = s3_proto::time::amz_date(now_s());
        let hash = self.payload_hash.clone().unwrap_or_else(|| sha256_hex(&self.body));
        let mut headers: Vec<(String, String)> = vec![("host".into(), host), ("x-amz-date".into(), amz_date), ("x-amz-content-sha256".into(), hash)];
        headers.extend(self.headers.iter().map(|(k, v)| (k.to_string(), v.clone())));
        let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let auth = s3_proto::sigv4::authorization(self.method, self.path, self.query, &refs, access, secret, REGION);
        (auth, headers)
    }

    pub async fn send(self, t: &TestServer) -> reqwest::Response {
        self.send_as(t, ACCESS, SECRET).await
    }

    pub async fn send_as(self, t: &TestServer, access: &str, secret: &str) -> reqwest::Response {
        let (auth, headers) = self.sign(t, access, secret);
        let url = if self.query.is_empty() { format!("{}{}", t.endpoint, self.path) } else { format!("{}{}?{}", t.endpoint, self.path, self.query) };
        let mut req = reqwest::Client::new().request(reqwest::Method::from_bytes(self.method.as_bytes()).unwrap(), url).header("authorization", auth);
        for (k, v) in headers {
            if k != "host" {
                req = req.header(k, v);
            }
        }
        req.body(self.body).send().await.unwrap()
    }
}

/// The `<Code>` of an S3 XML error body.
pub fn error_code(body: &str) -> String {
    body.split("<Code>").nth(1).and_then(|s| s.split("</Code>").next()).unwrap_or_default().to_string()
}
