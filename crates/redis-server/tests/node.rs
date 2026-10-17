//! Multi-tenant nodes: routing, scale to zero, lazy restore.

use redis_engine::Reply;
use redis_server::client::RespClient;
use redis_server::directory::AuthRequest;
use redis_server::node::{DbSpec, Node, NodeConfig, StaticCatalog, route};
use redis_server::persist::{Shipper, ShipperConfig};
use redis_server::{Directory, serve_tcp};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

fn spec(id: &str, secret: &str) -> DbSpec {
    DbSpec::new(id, secret)
}

async fn start(idle: Option<Duration>) -> (SocketAddr, Arc<Node>) {
    let o = redis_server::persist::test_objects("node");
    let cfg = ShipperConfig { flush_interval: Duration::from_millis(20), ..Default::default() };
    let shipper = Shipper::start("n1", o, cfg).await.unwrap();
    let catalog = Arc::new(StaticCatalog::default());
    catalog.insert(spec("alpha", "pa"));
    catalog.insert(spec("beta", "pb"));
    catalog.insert(DbSpec { replica_of: Some("alpha".into()), ..spec("alpha-ro", "pr") });
    let node = Node::new(catalog, Some(shipper), NodeConfig { idle_timeout: idle, domain: Some("redis.test".into()), ..Default::default() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir: Arc<dyn Directory> = node.clone();
    tokio::spawn(serve_tcp(listener, dir));
    (addr, node)
}

#[test]
fn routing() {
    let r = |user: Option<&'static str>, pass: &'static str, sni: Option<&'static str>| {
        route(&AuthRequest { user: user.map(str::as_bytes), password: pass.as_bytes(), sni, client: None, command: b"auth" }, Some("redis.test"))
            .map(|(db, u, s)| (db, u, String::from_utf8(s.to_vec()).unwrap()))
    };
    assert_eq!(r(None, "alpha_secret", None), Some(("alpha".into(), None, "secret".into())));
    assert_eq!(r(Some("app@beta"), "pw", None), Some(("beta".into(), Some("app".into()), "pw".into())));
    assert_eq!(r(Some("default"), "pw", Some("gamma.redis.test")), Some(("gamma".into(), Some("default".into()), "pw".into())));
    assert_eq!(r(None, "rdb_x4k9_secret", None), Some(("rdb_x4k9".into(), None, "secret".into())));
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
    assert_eq!(b.cmd(["AUTH", "default@beta", "pb"]).await.unwrap(), Reply::Ok);
    assert_eq!(b.cmd(["GET", "k"]).await.unwrap(), Reply::Nil);
    assert_eq!(b.cmd(["DBSIZE"]).await.unwrap(), Reply::Int(0));
    // `user@db` names a tenant ACL user; the platform secret is only the default user's.
    assert!(matches!(b.cmd(["AUTH", "app@beta", "pb"]).await.unwrap(), Reply::Error(e) if e.starts_with("WRONGPASS")));
    assert_eq!(b.cmd(["ACL", "SETUSER", "app", "on", ">apppw", "~*", "+@all"]).await.unwrap(), Reply::Ok);
    let mut app = RespClient::connect(addr).await.unwrap();
    assert_eq!(app.cmd(["AUTH", "app@beta", "apppw"]).await.unwrap(), Reply::Ok);
    assert_eq!(app.cmd(["ACL", "WHOAMI"]).await.unwrap(), Reply::bulk(b"app"));
    assert!(matches!(app.cmd(["AUTH", "app@alpha", "apppw"]).await.unwrap(), Reply::Error(e) if e.starts_with("WRONGPASS")));
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

#[tokio::test]
async fn tls_with_sni_routing() {
    use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
    let ck = rcgen::generate_simple_self_signed(vec!["*.redis.test".into()]).unwrap();
    let cert_pem = ck.cert.pem();
    let acceptor = redis_server::net::tls_acceptor(cert_pem.as_bytes(), ck.signing_key.serialize_pem().as_bytes()).unwrap();

    let (_, node) = start(None).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir: Arc<dyn Directory> = node.clone();
    tokio::spawn(redis_server::net::serve(listener, dir, redis_server::net::ListenOptions { tls: Some(acceptor), proxy_protocol: true }));

    let mut roots = RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // As if behind an NLB with proxy protocol v1.
    tcp.write_all(b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 6379\r\n").await.unwrap();
    let mut tls = connector.connect(ServerName::try_from("beta.redis.test").unwrap(), tcp).await.unwrap();
    // Plain password: the database comes from SNI.
    tls.write_all(b"AUTH pb\r\nCLIENT INFO\r\n").await.unwrap();
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    // +OK, the bulk header and the CLIENT INFO line.
    while got.windows(2).filter(|w| *w == b"\r\n").count() < 3 {
        let n = tls.read(&mut buf).await.unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    let s = String::from_utf8_lossy(&got);
    assert!(s.starts_with("+OK\r\n"), "{s}");
    assert!(s.contains("addr=203.0.113.7:51234"), "{s}");
}

#[tokio::test]
async fn read_replica_endpoint() {
    let (addr, _node) = start(None).await;
    let mut p = RespClient::connect(addr).await.unwrap();
    assert_eq!(p.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(p.cmd(["SET", "before", "1"]).await.unwrap(), Reply::Ok);
    let mut r = RespClient::connect(addr).await.unwrap();
    assert_eq!(r.cmd(["AUTH", "alpha-ro_pr"]).await.unwrap(), Reply::Ok);
    assert_eq!(p.cmd(["SET", "after", "2"]).await.unwrap(), Reply::Ok);
    let mut ok = false;
    for _ in 0..100 {
        if r.cmd(["GET", "after"]).await.unwrap() == Reply::bulk(b"2") {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ok, "replica should catch up");
    assert_eq!(r.cmd(["GET", "before"]).await.unwrap(), Reply::bulk(b"1"));
    assert_eq!(r.cmd(["SET", "x", "1"]).await.unwrap(), Reply::error("READONLY You can't write against a read only replica."));
    assert!(matches!(r.cmd(["ROLE"]).await.unwrap(), Reply::Array(a) if a[0] == Reply::bulk(b"slave")));
}

#[tokio::test]
async fn evict_disconnects_and_clients_reconnect_to_reloaded_state() {
    let (addr, node) = start(None).await;
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["SET", "k", "v"]).await.unwrap(), Reply::Ok);
    assert!(node.evict("alpha").await.unwrap());
    assert!(c.cmd(["PING"]).await.is_err(), "the connection is closed");
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["GET", "k"]).await.unwrap(), Reply::bulk(b"v"));
}

#[tokio::test]
async fn shutdown_restarts_the_database_with_its_data() {
    let (addr, node) = start(None).await;
    let mut c = RespClient::connect(addr).await.unwrap();
    let mut other = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(other.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["SET", "k", "v"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["CONFIG", "SET", "maxmemory-samples", "7"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["SHUTDOWN", "ABORT"]).await.unwrap(), Reply::error("ERR No shutdown in progress."));
    // No reply: the connection closes, and so does every other one.
    assert!(c.cmd(["SHUTDOWN", "NOSAVE"]).await.is_err());
    assert!(other.cmd(["PING"]).await.is_err());
    assert!(node.get_loaded("alpha").is_none(), "unloaded");
    // The next connection finds the data, and a fresh server's runtime state.
    let mut c = RespClient::connect(addr).await.unwrap();
    assert_eq!(c.cmd(["AUTH", "alpha_pa"]).await.unwrap(), Reply::Ok);
    assert_eq!(c.cmd(["GET", "k"]).await.unwrap(), Reply::bulk(b"v"));
    assert_eq!(c.cmd(["CONFIG", "GET", "maxmemory-samples"]).await.unwrap(), Reply::Array(vec![Reply::bulk(b"maxmemory-samples"), Reply::bulk(b"5")]));
}
