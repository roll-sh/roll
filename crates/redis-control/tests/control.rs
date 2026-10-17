//! The Redis control plane end to end: API → node → clients.

use redis_control::{Control, RedisConfig};
use redis_engine::Reply;
use redis_server::client::RespClient;
use redis_server::persist::{Objects, StorageConfig};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

struct Env {
    control: Arc<Control>,
    api: String,
    resp: std::net::SocketAddr,
    http: reqwest::Client,
}

async fn env() -> Env {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let i = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("roll-redis-control-{}-{n}-{i}", std::process::id()));
    let objects = Objects::new(&StorageConfig::Local(home.join("objects"))).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let resp = listener.local_addr().unwrap();
    let mut cfg = RedisConfig::local(home);
    cfg.resp_port = resp.port();
    cfg.flush_interval = Duration::from_millis(20);
    let control = Control::new(cfg, objects).await.unwrap();
    let dir: Arc<dyn redis_server::Directory> = control.node.clone();
    tokio::spawn(redis_server::serve_tcp(listener, dir));
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let router = redis_control::api::router(control.clone());
    tokio::spawn(async move { axum::serve(api_listener, router).await });
    Env { control, api, resp, http: reqwest::Client::new() }
}

impl Env {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.api));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let r = req.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }

    async fn connect(&self, token: &str) -> RespClient {
        let mut c = RespClient::connect(self.resp).await.unwrap();
        assert_eq!(c.cmd(["AUTH", token]).await.unwrap(), Reply::Ok);
        c
    }
}

fn token(v: &Value) -> String {
    v["rest_token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn lifecycle() {
    let e = env().await;
    let db = e.post("/v1/redis/databases", json!({"name": "cache", "max_memory_mb": 64})).await;
    let id = db["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("rdb_"));
    assert_eq!(db["status"]["state"], "idle");
    assert!(db["redis_url"].as_str().unwrap().starts_with("redis://default:rdb_"));
    let (s, v) = e.call(reqwest::Method::POST, "/v1/redis/databases", Some(json!({"name": "cache"}))).await;
    assert_eq!((s, v["error"].as_str().unwrap()), (409, "a database named cache already exists"));
    let (s, _) = e.call(reqwest::Method::POST, "/v1/redis/databases", Some(json!({"name": "x", "eviction_policy": "nope"}))).await;
    assert_eq!(s, 400);

    let mut c = e.connect(&token(&db)).await;
    assert_eq!(c.cmd(["SET", "k", "v"]).await.unwrap(), Reply::Ok);
    // The plan's memory is enforced and can't be raised by the tenant.
    assert_eq!(c.cmd(["CONFIG", "GET", "maxmemory"]).await.unwrap(), Reply::Array(vec![Reply::bulk(b"maxmemory"), Reply::bulk(b"67108864")]));
    assert!(c.cmd(["CONFIG", "SET", "maxmemory", "0"]).await.unwrap().is_error());
    assert_eq!(c.cmd(["CONFIG", "SET", "maxmemory", "32mb"]).await.unwrap(), Reply::Ok);

    // Settings apply live.
    let (s, v) = e.call(reqwest::Method::PATCH, &format!("/v1/redis/databases/{id}"), Some(json!({"eviction_policy": "allkeys-lru", "max_memory_mb": 128}))).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(c.cmd(["CONFIG", "GET", "maxmemory-policy"]).await.unwrap(), Reply::Array(vec![Reply::bulk(b"maxmemory-policy"), Reply::bulk(b"allkeys-lru")]));
    let (_, shown) = e.call(reqwest::Method::GET, "/v1/redis/databases/cache", None).await;
    assert_eq!(shown["status"]["state"], "active");
    assert_eq!(shown["status"]["keys"], 1);

    // The console endpoint.
    let r = e.post(&format!("/v1/redis/databases/{id}/command"), json!({"command": ["INCRBY", "n", 5]})).await;
    assert_eq!(r, json!({"result": 5}));

    // Password rotation: new connections need the new token.
    let rotated = e.post(&format!("/v1/redis/databases/{id}/password"), json!({})).await;
    let mut old = RespClient::connect(e.resp).await.unwrap();
    assert!(old.cmd(["AUTH", &token(&db)]).await.unwrap().is_error());
    e.connect(&token(&rotated)).await;
    assert_eq!(c.cmd(["PING"]).await.unwrap(), Reply::status("PONG"));

    // Usage is metered while loaded.
    e.control.sample(&mut Default::default()).unwrap();
    let (_, u) = e.call(reqwest::Method::GET, &format!("/v1/redis/databases/{id}/usage?days=1"), None).await;
    assert_eq!(u["active_seconds"], 60);
    assert!(u["commands"].as_i64().unwrap() >= 5);
    assert!(u["cost"]["total_usd"].as_f64().unwrap() > 0.0);
    assert!(u["cost"]["elasticache_serverless_usd"].as_f64().unwrap() > u["cost"]["total_usd"].as_f64().unwrap());

    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/redis/databases/{id}"), None).await;
    assert_eq!(s, 200);
    assert!(c.cmd(["PING"]).await.is_err(), "clients of a deleted database are disconnected");
    let (s, _) = e.call(reqwest::Method::GET, &format!("/v1/redis/databases/{id}"), None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn branches_restore_and_replicas() {
    let e = env().await;
    let db = e.post("/v1/redis/databases", json!({"name": "app"})).await;
    let id = db["id"].as_str().unwrap().to_string();
    let mut c = e.connect(&token(&db)).await;
    assert_eq!(c.cmd(["SET", "v", "1"]).await.unwrap(), Reply::Ok);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let t1 = redis_control::control::now();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(c.cmd(["SET", "v", "2"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["SET", "w", "x"]).await.unwrap(), Reply::Ok);

    // A branch at t1 sees v=1 and no w.
    let br = e.post(&format!("/v1/redis/databases/{id}/branches"), json!({"name": "app-t1", "at": t1})).await;
    assert_eq!(br["parent_id"], id.as_str());
    let mut b = e.connect(&token(&br)).await;
    assert_eq!(b.cmd(["GET", "v"]).await.unwrap(), Reply::bulk(b"1"));
    assert_eq!(b.cmd(["EXISTS", "w"]).await.unwrap(), Reply::Int(0));
    assert_eq!(b.cmd(["SET", "v", "branch"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["GET", "v"]).await.unwrap(), Reply::bulk(b"2"));
    // A parent with branches can't be deleted.
    let (s, _) = e.call(reqwest::Method::DELETE, &format!("/v1/redis/databases/{id}"), None).await;
    assert_eq!(s, 409);

    // A read replica follows the primary.
    let rep = e.post(&format!("/v1/redis/databases/{id}/replicas"), json!({})).await;
    assert!(rep["id"].as_str().unwrap().starts_with("rre_"));
    let mut r = e.connect(&token(&rep)).await;
    assert_eq!(r.cmd(["GET", "w"]).await.unwrap(), Reply::bulk(b"x"));
    assert_eq!(c.cmd(["SET", "z", "new"]).await.unwrap(), Reply::Ok);
    let mut seen = false;
    for _ in 0..200 {
        if r.cmd(["GET", "z"]).await.unwrap() == Reply::bulk(b"new") {
            seen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(seen, "replica catches up");
    assert!(r.cmd(["SET", "a", "b"]).await.unwrap().is_error());

    // Restore in place to t1: clients are disconnected and come back to the old state.
    let res = e.post(&format!("/v1/redis/databases/{id}/restore"), json!({"at": t1})).await;
    assert!(res["previous_state"].as_str().unwrap().starts_with(&id));
    assert!(c.cmd(["PING"]).await.is_err());
    let mut c = e.connect(&token(&db)).await;
    assert_eq!(c.cmd(["GET", "v"]).await.unwrap(), Reply::bulk(b"1"));
    assert_eq!(c.cmd(["EXISTS", "w", "z"]).await.unwrap(), Reply::Int(0));
    // …and keeps working (and durable) after the restore.
    assert_eq!(c.cmd(["SET", "after", "restore"]).await.unwrap(), Reply::Ok);
    drop(c);
    assert!(e.post(&format!("/v1/redis/databases/{id}/unload"), json!({})).await["unloaded"].as_bool().unwrap());
    let mut c = e.connect(&token(&db)).await;
    assert_eq!(c.cmd(["MGET", "v", "after"]).await.unwrap(), Reply::Array(vec![Reply::bulk(b"1"), Reply::bulk(b"restore")]));

    // Out of the window.
    let (s, _) = e.call(reqwest::Method::POST, &format!("/v1/redis/databases/{id}/restore"), Some(json!({"at": 1}))).await;
    assert_eq!(s, 400);
}
