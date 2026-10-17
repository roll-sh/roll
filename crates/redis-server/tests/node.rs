//! Multi-tenant nodes: routing, scale to zero, lazy restore.

use redis_engine::{Config, Reply};
use redis_server::client::RespClient;
use redis_server::directory::AuthRequest;
use redis_server::node::{DbSpec, Node, NodeConfig, Secret, StaticCatalog, route};
use redis_server::persist::{Objects, Shipper, ShipperConfig, StorageConfig};
use redis_server::{Directory, serve_tcp};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

fn spec(id: &str, secret: &str) -> DbSpec {
    DbSpec { id: id.into(), config: Config::default(), shards: 1, secrets: vec![Secret { value: secret.into(), read_only: false }] }
}

async fn start(idle: Option<Duration>) -> (SocketAddr, Arc<Node>) {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let o = Objects::new(&StorageConfig::Local(std::env::temp_dir().join(format!("roll-node-{n}")))).unwrap();
    let cfg = ShipperConfig { flush_interval: Duration::from_millis(20), ..Default::default() };
    let shipper = Shipper::start("n1", o, cfg).await.unwrap();
    let catalog = Arc::new(StaticCatalog::default());
    catalog.insert(spec("alpha", "pa"));
    catalog.insert(spec("beta", "pb"));
    let node = Node::new(catalog, Some(shipper), NodeConfig { idle_timeout: idle, domain: Some("redis.test".into()) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir: Arc<dyn Directory> = node.clone();
    tokio::spawn(serve_tcp(listener, dir));
    (addr, node)
}

#[test]
fn routing() {
    let r = |user: Option<&'static str>, pass: &'static str, sni: Option<&'static str>| {
        route(&AuthRequest { user: user.map(str::as_bytes), password: pass.as_bytes(), sni }, Some("redis.test"))
            .map(|(db, u, s)| (db, u, String::from_utf8(s.to_vec()).unwrap()))
    };
    assert_eq!(r(None, "alpha_secret", None), Some(("alpha".into(), None, "secret".into())));
    assert_eq!(r(Some("app@beta"), "pw", None), Some(("beta".into(), Some("app".into()), "pw".into())));
    assert_eq!(r(Some("default"), "pw", Some("gamma.redis.test")), Some(("gamma".into(), Some("default".into()), "pw".into())));
    assert_eq!(r(None, "nounderscore", None), None);
}

#[tokio::test]
async fn tenants_are_isolated() {
    let (addr, _node) = start(None).await;
    let mut a = RespClient::connect(addr).await.unwrap();
    assert!(matches!(a.cmd(["GET", "k"]).await.unwrap(), Reply::Error(e) if e.starts_with("NOAUTH")));
    assert!(matches!(a.cmd(["AUTH", "alpha_wrong"]).await.unwrap(), Reply::Error(e) if e.starts_with("WRONGPASS")));
    assert!(matches!(a.cmd(["AUTH", "beta_pa"]).await.unwrap(), Reply::Error(e) if e.starts_with("WRONGPASS")));
    assert_eq!(a.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(a.cmd(["SET", "k", "from-alpha"]).await.unwrap(), Reply::Ok);
    let mut b = RespClient::connect(addr).await.unwrap();
    assert_eq!(b.cmd(["AUTH", "app@beta", "pb"]).await.unwrap(), Reply::Ok);
    assert_eq!(b.cmd(["GET", "k"]).await.unwrap(), Reply::Nil);
    assert_eq!(b.cmd(["DBSIZE"]).await.unwrap(), Reply::Int(0));
    // Pub/sub is per database too.
    assert_eq!(b.cmd(["PUBLISH", "c", "x"]).await.unwrap(), Reply::Int(0));
}

#[tokio::test]
async fn scales_to_zero_and_back() {
    let (addr, node) = start(Some(Duration::from_millis(200))).await;
    {
        let mut c = RespClient::connect(addr).await.unwrap();
        assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
        for i in 0..500 {
            c.send(["SET", &format!("key:{i}"), &format!("value {i}")]).await.unwrap();
        }
        for _ in 0..500 {
            c.read().await.unwrap();
        }
        assert!(!c.cmd(["HELLO", "3"]).await.unwrap().is_error());
        // A connected client keeps the database loaded even when idle.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(node.get_loaded("alpha").is_some());
    }
    // Disconnected and idle: unloaded.
    let mut unloaded = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if node.get_loaded("alpha").is_none() {
            unloaded = true;
            break;
        }
    }
    assert!(unloaded, "idle database should unload");
    // Reconnecting restores it.
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["DBSIZE"]).await.unwrap(), Reply::Int(500));
    assert_eq!(c.cmd(["GET", "key:499"]).await.unwrap(), Reply::bulk(b"value 499"));
}

#[tokio::test]
async fn http_wakes_databases_too() {
    let (addr, node) = start(Some(Duration::from_millis(100))).await;
    let dir: Arc<dyn Directory> = node.clone();
    let http = redis_server::http::start(dir).await.unwrap();
    let c = reqwest::Client::new();
    let r: serde_json::Value = c.post(format!("http://{http}/")).bearer_auth("beta_pb").json(&["SET", "h", "1"]).send().await.unwrap().json().await.unwrap();
    assert_eq!(r, serde_json::json!({"result": "OK"}));
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if node.get_loaded("beta").is_none() {
            break;
        }
    }
    assert!(node.get_loaded("beta").is_none());
    let r: serde_json::Value = c.get(format!("http://{http}/get/h")).bearer_auth("beta_pb").send().await.unwrap().json().await.unwrap();
    assert_eq!(r, serde_json::json!({"result": "1"}));
    let _ = addr;
}
