//! A control plane with its CQL listener and API on ephemeral ports, on temp-dir storage (or
//! S3 under a unique prefix when `ROLL_E2E_S3_BUCKET` is set).
#![allow(dead_code)]

use bytes::Bytes;
use nosql_control::{Control, NosqlConfig};
use nosql_proto::client::{Client, ClientConfig, ClientError};
use nosql_proto::{QueryParams, ResultBody};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub struct Env {
    pub control: Arc<Control>,
    pub api: String,
    pub cql: SocketAddr,
    /// The Alternator (DynamoDB API) listener.
    pub alternator: SocketAddr,
    pub http: reqwest::Client,
    pub home: tempfile::TempDir,
}

pub async fn env() -> Env {
    env_with(|_| {}).await
}

pub async fn env_with(f: impl FnOnce(&mut NosqlConfig)) -> Env {
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cql = listener.local_addr().unwrap();
    let alt_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let alternator = alt_listener.local_addr().unwrap();
    let mut cfg = NosqlConfig::local(home.path().to_path_buf());
    cfg.cql_port = cql.port();
    cfg.alternator_port = Some(alternator.port());
    cfg.cql_shard_aware_port = None;
    cfg.rest_port = None;
    cfg.gc_interval = None;
    cfg.expiry_interval = Duration::from_secs(3600);
    cfg.metering_interval = Duration::from_secs(3600);
    cfg.import_interval = Duration::from_secs(3600);
    cfg.storage.maintenance_interval = Duration::from_millis(200);
    cfg.storage.manifest_interval = Duration::from_millis(500);
    f(&mut cfg);
    let control = Control::new(cfg, nosql_storage::test_objects("control")).await.unwrap();
    let dir: Arc<dyn nosql_server::Directory> = control.node.clone();
    tokio::spawn(nosql_server::net::serve(listener, dir.clone(), nosql_server::ListenOptions::default()));
    tokio::spawn(nosql_server::alternator::serve(alt_listener, dir, Default::default()));
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let router = nosql_control::api::router(control.clone());
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    Env { control, api, cql, alternator, http: reqwest::Client::new(), home }
}

impl Env {
    pub async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        let status = r.status().as_u16();
        let text = r.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    pub async fn get(&self, path: &str) -> Value {
        let (s, v) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "GET {path}: {v}");
        v
    }

    pub async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert_eq!(s, 200, "POST {path}: {v}");
        v
    }

    pub async fn patch(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::PATCH, path, Some(body)).await;
        assert_eq!(s, 200, "PATCH {path}: {v}");
        v
    }

    /// A CQL connection with the database's credentials (as the API shows them).
    pub async fn connect(&self, db: &Value) -> Client {
        self.try_connect(db).await.unwrap()
    }

    pub async fn try_connect(&self, db: &Value) -> Result<Client, ClientError> {
        let u = db["credentials"]["username"].as_str().unwrap();
        let p = db["credentials"]["password"].as_str().unwrap();
        self.connect_as(u, p).await
    }

    pub async fn connect_as(&self, user: &str, password: &str) -> Result<Client, ClientError> {
        Client::connect(ClientConfig::new(self.cql.to_string()).credentials(user, password)).await
    }
}

pub fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

pub async fn q(c: &Client, cql: &str) -> Vec<Vec<Option<Bytes>>> {
    match c.query(cql, QueryParams::default()).await {
        Ok(ResultBody::Rows(r)) => r.to_vec(),
        Ok(_) => vec![],
        Err(e) => panic!("{cql}: {e}"),
    }
}

pub async fn exec(c: &Client, cql: &str) {
    if let Err(e) = c.query(cql, QueryParams::default()).await {
        panic!("{cql}: {e}");
    }
}

pub fn server_code(e: ClientError) -> (i32, String) {
    match e {
        ClientError::Server(b) => (b.code, b.message),
        other => panic!("not a server error: {other:?}"),
    }
}

pub fn text(c: &Option<Bytes>) -> String {
    String::from_utf8_lossy(c.as_deref().unwrap_or_default()).into_owned()
}

pub fn int(c: &Option<Bytes>) -> i32 {
    let b = c.as_deref().expect("not null");
    i32::from_be_bytes(b.try_into().unwrap())
}

/// `SELECT v FROM ks.t WHERE k = ?` style: the text values of a one-column result, sorted.
pub async fn texts(c: &Client, cql: &str) -> Vec<String> {
    let mut v: Vec<String> = q(c, cql).await.iter().map(|r| text(&r[0])).collect();
    v.sort();
    v
}

pub async fn keyspace(c: &Client, ks: &str) {
    exec(c, &format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}")).await;
    exec(c, &format!("CREATE TABLE {ks}.t (k int PRIMARY KEY, v text)")).await;
}
