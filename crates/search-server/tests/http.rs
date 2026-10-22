//! The node over HTTP: framing, keep-alive and pipelining, compression, HEAD, limits, auth and
//! tenant routing, TLS with SNI, idle unload.

mod common;

use common::*;
use search_server::{ListenOptions, NodeConfig};
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

#[tokio::test]
async fn root_info_and_product_header() {
    let t = start_dev().await;
    let r = send(t.addr, "GET", "/", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("X-elastic-product"), Some("Elasticsearch"));
    assert!(r.text().contains("\"cluster_name\" : \"dev\""), "{}", r.text());
    assert!(r.text().contains("\"tagline\" : \"You Know, for Search\""));
}

#[tokio::test]
async fn keep_alive_and_pipelining() {
    let t = start_dev().await;
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    // Three requests written back to back on one connection.
    let mut batch = request("PUT", "/pipelined", &[("Content-Type", "application/json")], b"{}");
    batch.extend(request("GET", "/pipelined/_settings?filter_path=*.settings.index.number_of_shards", &[], b""));
    batch.extend(request("HEAD", "/pipelined", &[], b""));
    s.write_all(&batch).await.unwrap();
    let mut buf = Vec::new();
    let a = read_response(&mut s, &mut buf, false).await;
    let b = read_response(&mut s, &mut buf, false).await;
    let c = read_response(&mut s, &mut buf, true).await;
    assert_eq!(a.status, 200, "{}", a.text());
    assert_eq!(b.text(), r#"{"pipelined":{"settings":{"index":{"number_of_shards":"1"}}}}"#);
    assert_eq!(c.status, 200);
    // The connection stays usable.
    let d = send_on(&mut s, "GET", "/_cluster/health?filter_path=status", &[], b"").await;
    assert_eq!(d.text(), r#"{"status":"yellow"}"#);
}

#[tokio::test]
async fn head_sends_length_without_body() {
    let t = start_dev().await;
    let get = send(t.addr, "GET", "/missing-index", &[], b"").await;
    let head = send(t.addr, "HEAD", "/missing-index", &[], b"").await;
    assert_eq!(get.status, 404);
    assert_eq!(head.status, 404);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length").map(|l| l.parse::<usize>().unwrap()), Some(get.body.len()));
}

#[tokio::test]
async fn gzip_both_ways() {
    let t = start_dev().await;
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(br#"{"settings":{"number_of_replicas":0}}"#).unwrap();
    let body = z.finish().unwrap();
    let r = send(t.addr, "PUT", "/zipped", &[("Content-Type", "application/json"), ("Content-Encoding", "gzip")], &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = send(t.addr, "GET", "/_cluster/health?filter_path=status", &[("Accept-Encoding", "gzip")], b"").await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(&r.body[..]).read_to_string(&mut text).unwrap();
    assert_eq!(text, r#"{"status":"green"}"#);
}

#[tokio::test]
async fn body_limit() {
    let t = start(NodeConfig { default_cluster: Some("dev".into()), max_body_bytes: 1024, ..NodeConfig::default() }, ListenOptions::default()).await;
    let big = vec![b' '; 4096];
    let r = send(t.addr, "PUT", "/big", &[("Content-Type", "application/json")], &big).await;
    assert_eq!(r.status, 413);
    assert!(r.text().contains("content_too_long_exception"), "{}", r.text());
}

#[tokio::test]
async fn tenant_routing_and_auth() {
    let t = start(NodeConfig { domain: Some("search.test".into()), ..NodeConfig::default() }, ListenOptions::default()).await;
    let name = |r: &Resp| r.text().split("\"cluster_name\" : \"").nth(1).map(|s| s.split('"').next().unwrap().to_string());
    let r = send(t.addr, "GET", "/", &[("Authorization", &basic("c1", "pw1"))], b"").await;
    assert_eq!(name(&r).as_deref(), Some("c1"));
    let r = send(t.addr, "GET", "/", &[("Authorization", &basic("bob@c2", "pw2"))], b"").await;
    assert_eq!(name(&r).as_deref(), Some("c2"));
    let r = send(t.addr, "GET", "/", &[("Authorization", "Bearer c2_pw2")], b"").await;
    assert_eq!(name(&r).as_deref(), Some("c2"));
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode("c1:pw1");
    let r = send(t.addr, "GET", "/", &[("Authorization", &format!("ApiKey {key}"))], b"").await;
    assert_eq!(name(&r).as_deref(), Some("c1"));
    // Host label + any user name.
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    let raw = format!("GET / HTTP/1.1\r\nHost: c2.search.test:9200\r\nAuthorization: {}\r\n\r\n", basic("elastic", "pw2"));
    s.write_all(raw.as_bytes()).await.unwrap();
    let r = read_response(&mut s, &mut Vec::new(), false).await;
    assert_eq!(name(&r).as_deref(), Some("c2"));
    // Failures.
    let r = send(t.addr, "GET", "/_cat/indices", &[], b"").await;
    assert_eq!(r.status, 401);
    assert_eq!(r.header("WWW-Authenticate"), Some("Basic realm=\"security\", charset=\"UTF-8\""));
    assert!(r.text().contains("missing authentication credentials for REST request [/_cat/indices]"), "{}", r.text());
    let r = send(t.addr, "GET", "/", &[("Authorization", &basic("c1", "pw2"))], b"").await;
    assert_eq!(r.status, 401);
    assert!(r.text().contains("unable to authenticate user [c1] for REST request [/]"));
    // The tenants are isolated.
    send(t.addr, "PUT", "/only-in-c1", &[("Authorization", &basic("c1", "pw1"))], b"").await;
    let r = send(t.addr, "HEAD", "/only-in-c1", &[("Authorization", &basic("c2", "pw2"))], b"").await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn idle_clusters_unload_and_wake() {
    let t = start(NodeConfig { idle_unload: Some(Duration::from_millis(60)), ..NodeConfig::default() }, ListenOptions::default()).await;
    let auth = basic("c1", "pw1");
    let r = send(t.addr, "PUT", "/kept", &[("Authorization", &auth)], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(t.node.loaded(), vec!["c1".to_string()]);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !t.node.loaded().is_empty() || t.node.reaper_running() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("idle cluster unloads, reaper stops");
    let r = send(t.addr, "HEAD", "/kept", &[("Authorization", &auth)], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(t.node.loaded(), vec!["c1".to_string()]);
}

#[tokio::test]
async fn tls_with_sni_routing() {
    let cert = rcgen::generate_simple_self_signed(vec!["c1.search.test".to_string(), "c2.search.test".to_string()]).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();
    let acceptor = search_server::net::tls_acceptor(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();
    let t = start(NodeConfig { domain: Some("search.test".into()), ..NodeConfig::default() }, ListenOptions { tls: Some(acceptor), ..ListenOptions::default() }).await;

    use tokio_rustls::rustls;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tcp = TcpStream::connect(t.addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("c2.search.test").unwrap();
    let mut tls = connector.connect(server_name, tcp).await.unwrap();
    let r = send_on(&mut tls, "GET", "/", &[("Authorization", &basic("elastic", "pw2"))], b"").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("\"cluster_name\" : \"c2\""));
}

#[tokio::test]
async fn method_and_path_errors_over_http() {
    let t = start_dev().await;
    let r = send(t.addr, "DELETE", "/_cluster/health", &[], b"").await;
    assert_eq!(r.status, 405);
    assert_eq!(r.header("Allow"), Some("GET"));
    let r = send(t.addr, "OPTIONS", "/_cluster/health", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Allow"), Some("GET"));
    let r = send(t.addr, "PUT", "/idx", &[("Content-Type", "text/csv")], b"a,b").await;
    assert_eq!(r.status, 406);
}

#[tokio::test]
async fn proxy_protocol_header_is_consumed() {
    let t = start(NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() }, ListenOptions { proxy_protocol: true, ..ListenOptions::default() }).await;
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 9200\r\n").await.unwrap();
    let r = send_on(&mut s, "GET", "/_cluster/health?filter_path=status", &[], b"").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), r#"{"status":"green"}"#);
}

#[tokio::test]
async fn stalled_bodies_hold_no_permit_and_time_out() {
    let catalog = Arc::new(search_server::StaticCatalog::new());
    let mut spec = search_server::ClusterSpec::new("dev");
    spec.max_in_flight = 1;
    catalog.insert(spec);
    let node = search_server::Node::new(catalog, Arc::new(search_server::MemoryStores::new()), NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() });
    let opts = ListenOptions { body_timeouts: Some((Duration::from_millis(300), Duration::from_secs(5))), ..ListenOptions::default() };
    let (addr, _task) = search_server::spawn("127.0.0.1:0", node.clone(), opts).await.unwrap();
    // Three uploads announce a body and stall: with one permit and a queue of one, they would
    // have locked everyone else out if they held permits while reading.
    let mut stalled = Vec::new();
    for _ in 0..3 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"PUT /x HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
        stalled.push(s);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = send(addr, "GET", "/_cluster/health?filter_path=status", &[], b"").await;
    assert_eq!(r.status, 200, "{}", r.text());
    // Each stalled upload then gets a timeout and the connection is closed.
    for s in &mut stalled {
        let mut buf = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(5), read_response(s, &mut buf, false)).await.expect("answered");
        assert_eq!(r.status, 408);
        assert_eq!(r.header("connection"), Some("close"));
    }
    assert_eq!(node.get_loaded("dev").unwrap().in_flight(), 0);
}

#[tokio::test]
async fn gzip_refused_with_zero_quality_and_x_gzip_bodies() {
    let t = start_dev().await;
    let r = send(t.addr, "GET", "/_cluster/health?filter_path=status", &[("Accept-Encoding", "gzip;q=0")], b"").await;
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.text(), r#"{"status":"green"}"#);
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(b"{}").unwrap();
    let body = z.finish().unwrap();
    let r = send(t.addr, "PUT", "/xzipped", &[("Content-Type", "application/json"), ("Content-Encoding", "x-gzip")], &body).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test]
async fn duplicate_opaque_id_is_rejected() {
    let t = start_dev().await;
    let r = send(t.addr, "GET", "/_cluster/health", &[("X-Opaque-Id", "a"), ("X-Opaque-Id", "b")], b"").await;
    assert_eq!(r.status, 400);
    assert_eq!(
        r.text(),
        r#"{"error":{"root_cause":[{"type":"illegal_argument_exception","reason":"multiple values for single-valued header [X-Opaque-Id]."}],"type":"illegal_argument_exception","reason":"multiple values for single-valued header [X-Opaque-Id]."},"status":400}"#
    );
    assert_eq!(r.header("X-Opaque-Id"), Some("a"));
}
