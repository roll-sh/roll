//! Multi-tenant nodes: routing by SASL user and TLS SNI, the PROXY protocol, scale to zero with
//! data surviving, eviction with status events, read-only databases, metrics.

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use nosql_engine::Dialect;
use nosql_proto::client::{Client, ClientConfig, ClientError, TlsConfig};
use nosql_proto::event::{EventType, StatusChangeType};
use nosql_proto::{Event, QueryParams, ResultBody, Value, Version, code};
use nosql_server::{DbSpec, Directory, ListenOptions, NodeConfig, net};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn q() -> QueryParams {
    QueryParams::default()
}

fn rows(r: ResultBody) -> nosql_proto::Rows {
    match r {
        ResultBody::Rows(r) => r,
        other => panic!("not rows: {other:?}"),
    }
}

fn server_error(e: ClientError) -> nosql_proto::ErrorBody {
    match e {
        ClientError::Server(b) => b,
        other => panic!("not a server error: {other:?}"),
    }
}

/// n1: ScyllaDB, password `cassandra`/`pw1`, 4 shards. n2: Cassandra, AllowAll. n3: read-only.
/// n5: Cassandra, password `admin`/`pw5`, CassandraAuthorizer, read-only.
fn catalog(n: &NodeServer) {
    let mut n1 = DbSpec::new("n1", Dialect::Scylla).with_password("cassandra", "pw1").with_token("tok1");
    n1.config.settings.shards = 4;
    n.catalog.insert(n1);
    n.catalog.insert(DbSpec::new("n2", Dialect::Cassandra));
    let mut n3 = DbSpec::new("n3", Dialect::Scylla);
    n3.read_only = true;
    n.catalog.insert(n3);
    let mut n5 = DbSpec::new("n5", Dialect::Cassandra).with_password("admin", "pw5");
    n5.config.settings.authorizer = "CassandraAuthorizer".into();
    n5.config.settings.auth_bcrypt_log2_rounds = 4;
    n5.read_only = true;
    n.catalog.insert(n5);
}

async fn start(cfg: NodeConfig, opts: ListenOptions) -> NodeServer {
    let n = node(cfg, opts).await;
    catalog(&n);
    n
}

fn user(addr: std::net::SocketAddr, u: &str, p: &str) -> ClientConfig {
    let mut c = cfg(addr, Version::V4).credentials(u, p);
    c.max_version = Version::V4;
    c
}

async fn create(c: &Client, ks: &str) {
    c.query(&format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"), q()).await.unwrap();
    c.query(&format!("CREATE TABLE {ks}.t (k int PRIMARY KEY, v text)"), q()).await.unwrap();
}

#[tokio::test]
async fn routing_by_sasl_user() {
    init_tracing();
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let a = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    create(&a, "srv_r1").await;
    a.query("INSERT INTO srv_r1.t (k, v) VALUES (1, 'one')", q()).await.unwrap();
    // n2 allows everyone, and doesn't see n1's data.
    let b = Client::connect(user(n.addr, "whoever@n2", "x")).await.unwrap();
    let e = server_error(b.query("SELECT v FROM srv_r1.t", q()).await.unwrap_err());
    assert_eq!(e.code, code::INVALID);
    // Unknown databases and users without a database.
    for u in ["cassandra@nope", "cassandra"] {
        let e = Client::connect(user(n.addr, u, "pw1")).await.unwrap_err();
        let e = server_error(e);
        assert_eq!((e.code, e.message.as_str()), (code::BAD_CREDENTIALS, "Username and/or password are incorrect"), "{u}");
    }
    // The role keeps any `@` before the last one.
    let c = Client::connect(user(n.addr, "x@y@n2", "x")).await.unwrap();
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    // Both databases are loaded now.
    assert!(n.node.get_loaded("n1").is_some() && n.node.get_loaded("n2").is_some());
    assert!(n.node.get_loaded("n3").is_none());
}

/// SASL rounds on a node: a failed AUTH_RESPONSE leaves the connection unbound, so the next
/// one may fix the password or name another database; once bound, a new negotiation logs in
/// again inside that database only, and a failed one keeps the session.
#[tokio::test]
async fn sasl_rounds_route_each_attempt() {
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let auth = |u: &str, p: &str| {
        let tok = format!("\0{u}\0{p}");
        let mut b = (tok.len() as i32).to_be_bytes().to_vec();
        b.extend_from_slice(tok.as_bytes());
        envelope(4, 0, 2, 15, &b)
    };
    let bad = Some((code::BAD_CREDENTIALS, "Username and/or password are incorrect".to_string()));
    let mut r = common::Raw::connect(n.addr).await;
    r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
    let f = r.read().await.unwrap();
    assert_eq!((f.opcode, &f.body[2..]), (3, &b"org.apache.cassandra.auth.PasswordAuthenticator"[..]));
    for (u, p) in [("cassandra@n1", "wrong"), ("cassandra@nope", "pw1"), ("nobody@n1", "pw1")] {
        r.send(&auth(u, p)).await;
        assert_eq!(r.read().await.unwrap().error(), bad, "{u}");
    }
    assert!(n.node.get_loaded("n1").is_some(), "the failed attempts loaded n1");
    r.send(&auth("cassandra@n1", "pw1")).await;
    assert_eq!(r.read().await.unwrap().opcode, 0x10);
    let query = |sql: &str| envelope(4, 0, 3, 7, &query_body(sql, 0));
    r.send(&query("CREATE KEYSPACE srv_sasl WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}")).await;
    assert_eq!(r.read().await.unwrap().opcode, 8);
    // Ready: `role@n2` is a role of n1 now, and a failure keeps the logged-in session.
    r.send(&auth("x@n2", "y")).await;
    assert_eq!(r.read().await.unwrap().error(), bad);
    r.send(&query("SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'srv_sasl'")).await;
    let f = r.read().await.unwrap();
    assert_eq!(f.opcode, 8, "{:?}", f.error());
    r.send(&auth("cassandra@n1", "pw1")).await;
    assert_eq!(r.read().await.unwrap().opcode, 0x10);
    // An AllowAll database after failed attempts elsewhere.
    let mut r = common::Raw::connect(n.addr).await;
    r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
    assert_eq!(r.read().await.unwrap().opcode, 3);
    r.send(&auth("cassandra@n1", "wrong")).await;
    assert_eq!(r.read().await.unwrap().error(), bad);
    r.send(&auth("anyone@n2", "y")).await;
    assert_eq!(r.read().await.unwrap().opcode, 0x10);
    r.send(&query("SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = 'srv_sasl'")).await;
    let f = r.read().await.unwrap();
    assert_eq!(f.opcode, 8);
    assert_eq!(i32::from_be_bytes(f.body[f.body.len() - 4..].try_into().unwrap()), 0, "n2 has no srv_sasl");
}

#[tokio::test]
async fn sni_routing_over_tls() {
    let ck = rcgen::generate_simple_self_signed(vec!["*.nosql.test".into()]).unwrap();
    let acceptor = net::tls_acceptor(ck.cert.pem().as_bytes(), ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let cfg = NodeConfig { domain: Some("nosql.test".into()), ..Default::default() };
    let n = start(cfg, ListenOptions { tls: Some(acceptor), ..Default::default() }).await;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let tls = |name: &str| TlsConfig::with_roots(roots.clone(), name).unwrap();
    // n2 by SNI: Cassandra's dialect from the first byte (v5, its SUPPORTED map), no auth.
    let c = Client::connect(ClientConfig::new(n.addr.to_string()).tls(tls("n2.nosql.test"))).await.unwrap();
    assert_eq!(c.version(), Version::V5);
    assert_eq!(c.supported(), &nosql_proto::supported::cassandra());
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    // n1 by SNI needs its password; the user may name the database too.
    let base = || ClientConfig::new(n.addr.to_string()).tls(tls("n1.nosql.test"));
    assert!(matches!(Client::connect(base()).await.unwrap_err(), ClientError::Auth(_)));
    for u in ["cassandra", "cassandra@n1"] {
        let c = Client::connect(base().credentials(u, "pw1")).await.unwrap();
        assert_eq!(c.version(), Version::V4);
        assert_eq!(c.supported().get("SCYLLA_NR_SHARDS").unwrap(), ["4"]);
        assert_eq!(c.supported().get("SCYLLA_HOST_ID").unwrap(), [nosql_types::timeuuid::format_uuid(&nosql_engine::DbConfig::new("n1", Dialect::Scylla).host_id())]);
    }
    // An unknown name routes by user instead.
    let c = Client::connect(ClientConfig::new(n.addr.to_string()).tls(tls("zz.nosql.test")).credentials("a@n2", "b")).await.unwrap();
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    // The clients table shows the TLS session (Java's names) and the role.
    let r = rows(c.query("SELECT ssl_enabled, ssl_protocol, ssl_cipher_suite, username FROM system_views.clients", q()).await.unwrap()).to_vec();
    assert_eq!(r.len(), 2, "both connections to n2");
    let text = |i: usize| String::from_utf8_lossy(r[0][i].as_ref().unwrap()).into_owned();
    assert_eq!(r[0][0].as_deref(), Some(&[1u8][..]));
    assert_eq!(text(1), "TLSv1.3");
    assert!(text(2).starts_with("TLS_") && !text(2).starts_with("TLS13_"), "{}", text(2));
    assert_eq!(text(3), "anonymous", "n2 allows everyone");
    // ScyllaDB connections report their shard (n1 has 4).
    let c = Client::connect(base().credentials("cassandra", "pw1")).await.unwrap();
    let r = rows(c.query("SELECT shard_id, ssl_enabled FROM system.clients", q()).await.unwrap()).to_vec();
    assert_eq!(r.len(), 1);
    assert!(i32::from_be_bytes(r[0][0].as_ref().unwrap()[..].try_into().unwrap()) < 4);
    assert_eq!(r[0][1].as_deref(), Some(&[1u8][..]));
}

/// SCYLLA_SHARD in the SUPPORTED answer to an OPTIONS sent on `s`.
async fn shard_of<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(s: &mut S) -> String {
    s.write_all(&envelope(4, 0, 1, 5, &[])).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let mut got = Vec::new();
    while got.len() < 9 || got.len() < 9 + u32::from_be_bytes(got[5..9].try_into().unwrap()) as usize {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "closed");
        got.extend_from_slice(&buf[..n]);
    }
    let mut r = nosql_proto::wire::Reader::new(Bytes::copy_from_slice(&got[9..]));
    let map = r.string_multimap().unwrap();
    map.iter().find(|(k, _)| k == "SCYLLA_SHARD").unwrap().1[0].clone()
}

#[tokio::test]
async fn proxy_protocol_on_the_shard_aware_port() {
    // n1 (4 shards) is the default database, so OPTIONS presents its shard count.
    let cfg = NodeConfig { default_db: Some("n1".into()), ..Default::default() };
    let n = start(cfg, ListenOptions { proxy_protocol: true, shard_aware: true, ..Default::default() }).await;
    // v1: the client's port from the header picks the shard.
    let mut s = tokio::net::TcpStream::connect(n.addr).await.unwrap();
    s.write_all(b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 19042\r\n").await.unwrap();
    assert_eq!(shard_of(&mut s).await, (51234 % 4).to_string());
    // v2, IPv4.
    let mut s = tokio::net::TcpStream::connect(n.addr).await.unwrap();
    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    h.extend_from_slice(&[0x21, 0x11, 0, 12, 198, 51, 100, 9, 10, 0, 0, 1]);
    h.extend_from_slice(&51235u16.to_be_bytes());
    h.extend_from_slice(&19042u16.to_be_bytes());
    s.write_all(&h).await.unwrap();
    assert_eq!(shard_of(&mut s).await, (51235 % 4).to_string());
    // LOCAL (health checks): the socket's own port.
    let mut s = tokio::net::TcpStream::connect(n.addr).await.unwrap();
    let port = s.local_addr().unwrap().port();
    let mut h = b"\r\n\r\n\0\r\nQUIT\n".to_vec();
    h.extend_from_slice(&[0x20, 0x00, 0, 0]);
    s.write_all(&h).await.unwrap();
    assert_eq!(shard_of(&mut s).await, (port % 4).to_string());
    // No header: closed.
    let mut s = tokio::net::TcpStream::connect(n.addr).await.unwrap();
    s.write_all(&envelope(4, 0, 1, 5, &[])).await.unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await, Ok(Ok(0)) | Ok(Err(_))));
}

#[tokio::test]
async fn proxy_protocol_with_tls() {
    use tokio_rustls::rustls::pki_types::ServerName;
    let ck = rcgen::generate_simple_self_signed(vec!["*.nosql.test".into()]).unwrap();
    let acceptor = net::tls_acceptor(ck.cert.pem().as_bytes(), ck.signing_key.serialize_pem().as_bytes()).unwrap();
    let cfg = NodeConfig { domain: Some("nosql.test".into()), ..Default::default() };
    let n = start(cfg, ListenOptions { tls: Some(acceptor), proxy_protocol: true, shard_aware: true, ..Default::default() }).await;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let tls = tokio_rustls::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls));
    let mut tcp = tokio::net::TcpStream::connect(n.addr).await.unwrap();
    tcp.write_all(b"PROXY TCP4 203.0.113.7 10.0.0.1 40003 19042\r\n").await.unwrap();
    let mut s = connector.connect(ServerName::try_from("n1.nosql.test").unwrap(), tcp).await.unwrap();
    assert_eq!(shard_of(&mut s).await, (40003 % 4).to_string());
}

async fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if f() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn idle_unload_and_reload_keep_the_data() {
    let cfg = NodeConfig { idle_timeout: Some(Duration::from_millis(300)), ..Default::default() };
    let n = start(cfg, ListenOptions::default()).await;
    {
        let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
        create(&c, "srv_idle").await;
        for i in 0..50 {
            c.query("INSERT INTO srv_idle.t (k, v) VALUES (?, ?)", QueryParams { values: vec![Value::Set(int(i)), Value::Set(text(&format!("v{i}")))], ..q() })
                .await
                .unwrap();
        }
        // Not unloaded while connected.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(n.node.get_loaded("n1").is_some());
    }
    assert!(wait_for(|| n.node.get_loaded("n1").is_none()).await, "unloaded once idle");
    let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    let r = rows(c.query("SELECT k, v FROM srv_idle.t", q()).await.unwrap());
    assert_eq!(r.len(), 50);
    let m = n.node.metrics("n1").unwrap();
    assert_eq!(m.loads, 2);
    assert_eq!(m.connections, 1);
    // A zero idle timeout never unloads.
    let mut keep = DbSpec::new("n4", Dialect::Scylla);
    keep.idle_timeout = Some(Duration::ZERO);
    n.catalog.insert(keep);
    drop(Client::connect(user(n.addr, "a@n4", "x")).await.unwrap());
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(n.node.get_loaded("n4").is_some());
    assert_eq!(n.node.idle_timeout("n4"), None);
    // A new idle timeout applies to the loaded database at once.
    n.node.set_idle_timeout("n4", Some(Duration::from_millis(100)));
    assert_eq!(n.node.idle_timeout("n4"), Some(Duration::from_millis(100)));
    assert!(wait_for(|| n.node.get_loaded("n4").is_none()).await, "unloaded with the new timeout");
    // The next load takes the spec's (zero: never) again.
    drop(Client::connect(user(n.addr, "a@n4", "x")).await.unwrap());
    assert_eq!(n.node.idle_timeout("n4"), None);
    // `None` falls back to the node's default.
    n.node.set_idle_timeout("n4", None);
    assert_eq!(n.node.idle_timeout("n4"), Some(Duration::from_millis(300)));
    assert!(wait_for(|| n.node.get_loaded("n4").is_none()).await);
}

#[tokio::test]
async fn evict_disconnects_with_status_change() {
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    create(&c, "srv_ev").await;
    c.query("INSERT INTO srv_ev.t (k, v) VALUES (1, 'kept')", q()).await.unwrap();
    let mut events = c.register(&[EventType::StatusChange]).await.unwrap();
    let other = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    assert!(n.node.evict("n1").await.unwrap());
    let e = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
    assert_eq!(e, Event::StatusChange { change: StatusChangeType::Down, node: "127.0.0.1:9042".parse().unwrap() });
    assert!(c.query("SELECT key FROM system.local", q()).await.is_err());
    assert!(other.query("SELECT key FROM system.local", q()).await.is_err());
    assert!(n.node.get_loaded("n1").is_none());
    let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    let r = rows(c.query("SELECT v FROM srv_ev.t WHERE k = 1", q()).await.unwrap());
    assert_eq!(r.to_vec(), vec![vec![Some(text("kept"))]]);
    // Evicting or unloading something not loaded is a no-op.
    assert!(!n.node.evict("n3").await.unwrap());
    assert!(!n.node.unload("nope").await.unwrap());
    // Announcements reach registered connections.
    let mut topo = c.register(&[EventType::TopologyChange]).await.unwrap();
    let addr: std::net::SocketAddr = "10.1.2.3:9042".parse().unwrap();
    assert!(n.node.announce("n1", nosql_engine::Event::TopologyChange { change: "MOVED_NODE", address: addr }));
    let e = tokio::time::timeout(Duration::from_secs(5), topo.recv()).await.unwrap().unwrap();
    assert_eq!(e, Event::TopologyChange { change: nosql_proto::event::TopologyChangeType::MovedNode, node: addr });
    assert!(!n.node.announce("n3", nosql_engine::Event::StatusChange { change: "UP", address: addr }));
}

#[tokio::test]
async fn read_only_databases() {
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let c = Client::connect(user(n.addr, "a@n3", "x")).await.unwrap();
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    let e = server_error(
        c.query("CREATE KEYSPACE srv_ro WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", q()).await.unwrap_err(),
    );
    assert_eq!((e.code, e.message.as_str()), (code::UNAUTHORIZED, "This database is read-only"));
    // Role and permission statements too; listing works.
    let c = Client::connect(user(n.addr, "admin@n5", "pw5")).await.unwrap();
    for cql in ["CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true", "GRANT SELECT ON ALL KEYSPACES TO admin", "ALTER ROLE admin WITH PASSWORD = 'x'"] {
        let e = server_error(c.query(cql, q()).await.unwrap_err());
        assert_eq!((e.code, e.message.as_str()), (code::UNAUTHORIZED, "This database is read-only"), "{cql}");
    }
    assert!(!rows(c.query("LIST ROLES", q()).await.unwrap()).is_empty());
    c.query("LIST ALL PERMISSIONS", q()).await.unwrap();
    // The node switches a loaded database live, without reconnecting.
    assert!(n.node.set_read_only("n5", false));
    c.query("CREATE ROLE r WITH PASSWORD = 'r' AND LOGIN = true", q()).await.unwrap();
    assert!(n.node.set_read_only("n5", true));
    let e = server_error(c.query("DROP ROLE r", q()).await.unwrap_err());
    assert_eq!(e.message, "This database is read-only");
    assert!(!n.node.set_read_only("nope", true));
    // The platform credential is the engine's superuser role.
    let e = server_error(Client::connect(user(n.addr, "admin@n5", "wrong")).await.unwrap_err());
    assert_eq!((e.code, e.message.as_str()), (code::BAD_CREDENTIALS, "Provided username admin and/or password are incorrect"));
}

#[tokio::test]
async fn metrics_per_database() {
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    c.query("SELECT key FROM system.local", q()).await.unwrap();
    assert!(c.query("SELEC", q()).await.is_err());
    let m = n.node.metrics("n1").unwrap();
    assert_eq!(m.connections, 1);
    assert_eq!(m.connections_total, 1);
    assert!(m.requests >= 2, "{m:?}");
    assert!(m.errors >= 1, "{m:?}");
    assert!(m.bytes_in > 50 && m.bytes_out > 100, "{m:?}");
    drop(c);
    assert!(wait_for(|| n.node.metrics("n1").unwrap().connections == 0).await);
    assert!(n.node.metrics("n2").is_none());
    assert_eq!(n.node.all_metrics().len(), 1);
}

#[tokio::test]
async fn single_mode_default_database() {
    let cfg = NodeConfig { default_db: Some("n2".into()), ..Default::default() };
    let n = start(cfg, ListenOptions::default()).await;
    // No SNI, no credentials: the default database, which allows everyone.
    let c = Client::connect(cfg_v(n.addr)).await.unwrap();
    assert_eq!(c.supported(), &nosql_proto::supported::cassandra());
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    // Every connection without SNI lands there (READY comes before any credentials).
    let c = Client::connect(user(n.addr, "cassandra@n1", "pw1")).await.unwrap();
    create(&c, "srv_sm").await;
    let d = Client::connect(cfg_v(n.addr)).await.unwrap();
    assert_eq!(rows(d.query("SELECT v FROM srv_sm.t", q()).await.unwrap()).len(), 0);
    assert!(n.node.get_loaded("n1").is_none());
    // Users without a database go to the default; users naming one go there.
    let dir: Arc<dyn Directory> = n.node.clone();
    assert_eq!(dir.route_user("bob").await.unwrap().0.id, "n2");
    assert_eq!(dir.route_user("bob@n1").await.unwrap().0.id, "n1");
    let (spec, role) = dir.route_user("bob@nope").await.unwrap();
    assert_eq!((spec.id.as_str(), role.as_str()), ("n2", "bob@nope"));
}

fn cfg_v(addr: std::net::SocketAddr) -> ClientConfig {
    ClientConfig::new(addr.to_string())
}

#[tokio::test]
async fn token_routing() {
    let n = start(NodeConfig::default(), ListenOptions::default()).await;
    let dir: Arc<dyn Directory> = n.node.clone();
    assert_eq!(dir.route_token(Some("n1_tok1")).await.unwrap().id, "n1");
    assert!(dir.route_token(Some("n1_wrong")).await.is_none());
    assert!(dir.route_token(Some("n2_tok1")).await.is_none(), "n2 has no tokens");
    assert!(dir.route_token(None).await.is_none());
}
