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
    // Elasticsearch and OpenSearch send no `Date` header.
    assert_eq!(r.header("date"), None);
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

/// Everything the server sends on `s` until it closes the connection.
async fn until_closed(s: &mut TcpStream) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await.expect("the server closes the connection").unwrap();
    out
}

#[tokio::test]
async fn body_limit() {
    let t = start(NodeConfig { default_cluster: Some("dev".into()), max_body_bytes: 1024, ..NodeConfig::default() }, ListenOptions::default()).await;
    // As Elasticsearch: an empty "413 Request Entity Too Large", and the connection closes.
    let expect_413 = |raw: Vec<u8>| {
        let text = String::from_utf8_lossy(&raw).into_owned();
        assert!(text.starts_with("HTTP/1.1 413 Request Entity Too Large\r\n"), "{text}");
        assert!(text.to_ascii_lowercase().contains("\r\nconnection: close\r\n"), "{text}");
        assert!(text.to_ascii_lowercase().contains("\r\ncontent-length: 0\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "no body: {text}");
    };
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(&request("PUT", "/big", &[("Content-Type", "application/json")], &vec![b' '; 4096])).await.unwrap();
    expect_413(until_closed(&mut s).await);
    // A chunked body found too long while reading.
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    let chunk = vec![b' '; 800];
    let mut raw = b"POST /_search HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..2 {
        raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        raw.extend_from_slice(&chunk);
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"0\r\n\r\n");
    s.write_all(&raw).await.unwrap();
    expect_413(until_closed(&mut s).await);
    // A small gzip body that inflates past the limit.
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&vec![b' '; 10_000]).unwrap();
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(&request("POST", "/_search", &[("Content-Type", "application/json"), ("Content-Encoding", "gzip")], &z.finish().unwrap())).await.unwrap();
    expect_413(until_closed(&mut s).await);
}

#[tokio::test]
async fn long_request_lines_and_headers_are_refused_like_elasticsearch() {
    let t = start_dev().await;
    // `GET <path> HTTP/1.1` of exactly 4096 bytes passes; one more byte doesn't.
    let path = |len: usize| format!("/{}", "a".repeat(len - "GET  HTTP/1.1".len() - 1));
    let r = send(t.addr, "GET", &path(4096), &[], b"").await;
    assert_eq!(r.status, 404, "{}", r.text());
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(&request("GET", &path(4097), &[], b"")).await.unwrap();
    let raw = String::from_utf8_lossy(&until_closed(&mut s).await).into_owned();
    assert!(raw.starts_with("HTTP/1.1 400 "), "{raw}");
    assert!(
        raw.ends_with(r#"{"error":{"root_cause":[{"type":"too_long_http_line_exception","reason":"An HTTP line is larger than 4096 bytes."}],"type":"too_long_http_line_exception","reason":"An HTTP line is larger than 4096 bytes."},"status":400}"#),
        "{raw}"
    );
    // Header lines (without their CRLFs) may total 16384 bytes. `request` adds `Host: localhost`.
    let host = "Host: localhost".len();
    let value = |total: usize| "b".repeat(total - host - "X-Big: ".len());
    let ok = value(16384);
    let r = send(t.addr, "GET", "/", &[("X-Big", &ok)], b"").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let big = value(16385);
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(&request("GET", "/", &[("X-Big", &big)], b"")).await.unwrap();
    let raw = String::from_utf8_lossy(&until_closed(&mut s).await).into_owned();
    assert!(raw.starts_with("HTTP/1.1 400 "), "{raw}");
    assert!(
        raw.ends_with(r#"{"error":{"root_cause":[{"type":"too_long_http_header_exception","reason":"HTTP header is larger than 16384 bytes."}],"type":"too_long_http_header_exception","reason":"HTTP header is larger than 16384 bytes."},"status":400}"#),
        "{raw}"
    );
}

#[tokio::test]
async fn broken_compressed_bodies_behave_like_elasticsearch() {
    let t = start_dev().await;
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(br#"{"f":1}"#).unwrap();
    let z = z.finish().unwrap();
    let gz = [("Content-Type", "application/json"), ("Content-Encoding", "gzip")];
    // Cut after the gzip header: the body is empty.
    let r = send(t.addr, "PUT", "/broken/_doc/1", &gz, &z[..10]).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.text().contains(r#""type":"parse_exception","reason":"request body is required""#), "{}", r.text());
    // Not gzip at all: the connection closes without an answer.
    let mut s = TcpStream::connect(t.addr).await.unwrap();
    s.write_all(&request("PUT", "/broken/_doc/1", &gz, b"not gzip at all")).await.unwrap();
    assert!(until_closed(&mut s).await.is_empty());
    // Raw deflate is accepted as `deflate`, like zlib-wrapped.
    let mut d = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    d.write_all(br#"{"f":2}"#).unwrap();
    let r = send(t.addr, "PUT", "/broken/_doc/2", &[("Content-Type", "application/json"), ("Content-Encoding", "deflate")], &d.finish().unwrap()).await;
    assert_eq!(r.status, 201, "{}", r.text());
}

#[tokio::test]
async fn head_is_never_gzipped() {
    let t = start_dev().await;
    let get = send(t.addr, "GET", "/", &[], b"").await;
    let head = send(t.addr, "HEAD", "/", &[("Accept-Encoding", "gzip")], b"").await;
    assert_eq!(head.status, 200);
    assert_eq!(head.header("content-encoding"), None);
    assert_eq!(head.header("content-length").map(|l| l.parse::<usize>().unwrap()), Some(get.body.len()));
    // Large responses are gzipped off the worker threads, with the same result.
    let docs: String = (0..400).map(|i| format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"text\":\"{}\"}}\n", "lorem ipsum ".repeat(30))).collect();
    let r = send(t.addr, "POST", "/big/_bulk?refresh=true", &[("Content-Type", "application/x-ndjson")], docs.as_bytes()).await;
    assert_eq!(r.status, 200);
    let plain = send(t.addr, "GET", "/big/_search?size=400", &[], b"").await;
    assert!(plain.body.len() > 64 * 1024);
    let zipped = send(t.addr, "GET", "/big/_search?size=400", &[("Accept-Encoding", "gzip")], b"").await;
    assert_eq!(zipped.header("content-encoding"), Some("gzip"));
    let mut text = Vec::new();
    flate2::read::GzDecoder::new(&zipped.body[..]).read_to_end(&mut text).unwrap();
    // The same answer (`took` may differ by a digit).
    let hits = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap()["hits"].clone();
    assert_eq!(hits(&text), hits(&plain.body));
}

/// A node whose one cluster `dev` has a 1000-byte in-flight body budget.
async fn budgeted(bytes: usize) -> (Arc<search_server::Node>, std::net::SocketAddr) {
    let catalog = Arc::new(search_server::StaticCatalog::new());
    let mut spec = search_server::ClusterSpec::new("dev");
    spec.max_in_flight_bytes = Some(bytes);
    catalog.insert(spec);
    let node = search_server::Node::new(catalog, Arc::new(search_server::MemoryStores::new()), NodeConfig { default_cluster: Some("dev".into()), ..NodeConfig::default() });
    let (addr, _task) = search_server::spawn("127.0.0.1:0", node.clone(), ListenOptions::default()).await.unwrap();
    (node, addr)
}

#[tokio::test]
async fn chunked_bodies_count_against_the_byte_budget() {
    let (node, addr) = budgeted(1000).await;
    // Chunked uploads that stall after 900 bytes: the first holds 900 bytes of the budget...
    let mut first = TcpStream::connect(addr).await.unwrap();
    let head = b"POST /_bulk HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n";
    let mut raw = head.to_vec();
    raw.extend_from_slice(format!("{:x}\r\n{}\r\n", 900, " ".repeat(900)).as_bytes());
    first.write_all(&raw).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node.get_loaded("dev").is_none_or(|l| l.bytes_in_flight() < 900) {
        assert!(std::time::Instant::now() < deadline, "the first chunk is reserved");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // ...so the next one is refused with 429 when its chunk arrives,
    let mut second = TcpStream::connect(addr).await.unwrap();
    second.write_all(&raw).await.unwrap();
    let r = String::from_utf8_lossy(&until_closed(&mut second).await).into_owned();
    assert!(r.starts_with("HTTP/1.1 429 "), "{r}");
    assert!(r.contains("es_rejected_execution_exception") && r.contains("coordinating_operation_bytes=900"), "{r}");
    // and so is a declared body.
    let r = send(addr, "POST", "/_bulk", &[("Content-Type", "application/x-ndjson")], &[b' '; 900]).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(node.get_loaded("dev").unwrap().bytes_in_flight(), 900);
    drop(first);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node.get_loaded("dev").unwrap().bytes_in_flight() > 0 {
        assert!(std::time::Instant::now() < deadline, "a closed upload gives its bytes back");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn inflated_bodies_count_against_the_byte_budget() {
    let (node, addr) = budgeted(200_000).await;
    // An upload holds 150 kB of the budget (declared, then stalled).
    let mut held = TcpStream::connect(addr).await.unwrap();
    held.write_all(b"POST /_bulk HTTP/1.1\r\nHost: x\r\nContent-Type: application/x-ndjson\r\nContent-Length: 150000\r\n\r\n{").await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node.get_loaded("dev").is_none_or(|l| l.bytes_in_flight() < 150_000) {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A 1 kB gzip body that inflates to 1 MB doesn't fit in what's left.
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&vec![b' '; 1 << 20]).unwrap();
    let z = z.finish().unwrap();
    assert!(z.len() < 5000);
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&request("POST", "/_search", &[("Content-Type", "application/json"), ("Content-Encoding", "gzip")], &z)).await.unwrap();
    let r = String::from_utf8_lossy(&until_closed(&mut s).await).into_owned();
    assert!(r.starts_with("HTTP/1.1 429 "), "{r}");
    assert_eq!(node.get_loaded("dev").unwrap().bytes_in_flight(), 150_000);
    // Alone, the same body inflates (and only its output stays reserved while it runs).
    drop(held);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node.get_loaded("dev").unwrap().bytes_in_flight() > 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = send(addr, "POST", "/_search", &[("Content-Type", "application/json"), ("Content-Encoding", "gzip")], &z).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(node.get_loaded("dev").unwrap().bytes_in_flight(), 0);
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

/// Every way to name a cluster and prove access, and every way to fail, with Elasticsearch's
/// 401 shape (`security_exception`, `WWW-Authenticate` with Basic, Bearer and ApiKey).
#[tokio::test]
async fn auth_matrix() {
    use base64::Engine;
    let t = start(NodeConfig { domain: Some("search.test".into()), ..NodeConfig::default() }, ListenOptions::default()).await;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    // c3: a user-bound credential, an API key id and a hashed secret.
    let mut spec = search_server::ClusterSpec::new("c3");
    spec.credentials.push(search_server::Credential::password("bobpw").for_user("bob"));
    spec.credentials.push(search_server::Credential::hashed(search_server::secret_hash("keysecret")).for_api_key("k1"));
    t.catalog.insert(spec);
    let name = |r: &Resp| r.text().split("\"cluster_name\" : \"").nth(1).map(|s| s.split('"').next().unwrap().to_string());
    let host = |h: &'static str| ("Host", h);
    let ok_cases: Vec<(Vec<(&str, String)>, &str)> = vec![
        (vec![("Authorization", basic("c1", "pw1"))], "c1"),
        (vec![("Authorization", basic("anyone@c1", "pw1"))], "c1"),
        (vec![("Authorization", format!("ApiKey {}", b64("c2:pw2")))], "c2"),
        (vec![("Authorization", "Bearer c2_pw2".to_string())], "c2"),
        (vec![("Authorization", "bearer c2_pw2".to_string())], "c2"),
        (vec![("Authorization", basic("bob@c3", "bobpw"))], "c3"),
        (vec![("Authorization", format!("ApiKey {}", b64("k1@c3:keysecret")))], "c3"),
    ];
    for (headers, want) in &ok_cases {
        let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = send(t.addr, "GET", "/", &h, b"").await;
        assert_eq!((r.status, name(&r).as_deref()), (200, Some(*want)), "{headers:?}: {}", r.text());
    }
    // Host label routing (the TLS SNI label works the same; see tls_with_sni_routing).
    for (auth, h) in [(basic("elastic", "pw2"), "c2.search.test"), ("Bearer pw1".to_string(), "c1.search.test:443")] {
        let mut s = TcpStream::connect(t.addr).await.unwrap();
        let raw = format!("GET / HTTP/1.1\r\nHost: {h}\r\nAuthorization: {auth}\r\n\r\n");
        s.write_all(raw.as_bytes()).await.unwrap();
        let r = read_response(&mut s, &mut Vec::new(), false).await;
        assert_eq!(r.status, 200, "{h}: {}", r.text());
    }
    let _ = host;
    let fail_cases: Vec<(Vec<(&str, String)>, &str)> = vec![
        (vec![], "missing authentication credentials for REST request [/_search]"),
        (vec![("Authorization", "Digest abc".to_string())], "missing authentication credentials for REST request [/_search]"),
        (vec![("Authorization", "Basic %%%".to_string())], "invalid basic authentication header encoding"),
        (vec![("Authorization", format!("Basic {}", b64("nocolon")))], "invalid basic authentication header value"),
        (vec![("Authorization", "ApiKey %%%".to_string())], "unable to authenticate with provided credentials and anonymous access is not allowed for this request"),
        (vec![("Authorization", basic("c1", "pw2"))], "unable to authenticate user [c1] for REST request [/_search]"),
        // Another tenant's password, an unknown cluster: the same answer.
        (vec![("Authorization", basic("c2", "pw1"))], "unable to authenticate user [c2] for REST request [/_search]"),
        (vec![("Authorization", basic("nosuch", "pw1"))], "unable to authenticate user [nosuch] for REST request [/_search]"),
        (vec![("Authorization", basic("alice@c3", "bobpw"))], "unable to authenticate user [alice@c3] for REST request [/_search]"),
        (vec![("Authorization", format!("ApiKey {}", b64("k1@c3:wrong")))], "unable to authenticate with provided credentials and anonymous access is not allowed for this request"),
        (vec![("Authorization", format!("ApiKey {}", b64("k2@c3:keysecret")))], "unable to authenticate with provided credentials and anonymous access is not allowed for this request"),
        (vec![("Authorization", "Bearer c1_wrong".to_string())], "unable to authenticate with provided credentials and anonymous access is not allowed for this request"),
        (vec![("Authorization", "Bearer nounderscore".to_string())], "unable to authenticate with provided credentials and anonymous access is not allowed for this request"),
        (vec![("Authorization", "Bearer ".to_string())], "missing authentication credentials for REST request [/_search]"),
    ];
    for (headers, reason) in &fail_cases {
        let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let r = send(t.addr, "GET", "/_search", &h, b"").await;
        assert_eq!(r.status, 401, "{headers:?}: {}", r.text());
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["error"]["type"], "security_exception", "{v}");
        assert_eq!(v["error"]["reason"], *reason, "{headers:?}");
        assert_eq!(v["error"]["root_cause"][0]["type"], "security_exception");
        assert_eq!(
            v["error"]["header"]["WWW-Authenticate"],
            serde_json::json!(["Basic realm=\"security\", charset=\"UTF-8\"", "Bearer realm=\"security\"", "ApiKey"])
        );
        let challenges: Vec<&str> = r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("www-authenticate")).map(|(_, v)| v.as_str()).collect();
        assert_eq!(challenges, ["Basic realm=\"security\", charset=\"UTF-8\"", "Bearer realm=\"security\"", "ApiKey"]);
        assert_eq!(r.header("X-Elastic-Product"), Some("Elasticsearch"));
        assert_eq!(v["status"], 401);
    }
    // Byte for byte as Elasticsearch 9.5.5 answers (security on).
    let www = r#""header":{"WWW-Authenticate":["Basic realm=\"security\", charset=\"UTF-8\"","Bearer realm=\"security\"","ApiKey"]}"#;
    let expect = |reason: &str, extra: &str, caused: &str| {
        format!(
            r#"{{"error":{{"root_cause":[{{"type":"security_exception","reason":"{reason}"{extra},{www}}}],"type":"security_exception","reason":"{reason}"{extra}{caused},{www}}},"status":401}}"#
        )
    };
    let cases = [
        (format!("ApiKey {}", b64("k1:wrong")), expect(ANON, r#","additional_unsuccessful_credentials":"API key: unable to find apikey with id k1""#, "")),
        ("Bearer abc".to_string(), expect(ANON, r#","additional_unsuccessful_credentials":"oauth2 token: invalid token""#, "")),
        (
            "Basic %%%".to_string(),
            expect("invalid basic authentication header encoding", "", r#","caused_by":{"type":"illegal_argument_exception","reason":"Illegal base64 character 25"}"#),
        ),
    ];
    for (h, want) in cases {
        let r = send(t.addr, "GET", "/_search", &[("Authorization", &h)], b"").await;
        assert_eq!(r.text(), want, "{h}");
    }
    // Nothing was loaded for the failed attempts on unknown clusters.
    assert!(!t.node.loaded().contains(&"nosuch".to_string()));
}

const ANON: &str = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";

/// Users, API keys and access tokens of a cluster's own security authenticate through the node
/// (`<user>@<cluster>`, `<key id>@<cluster>`, or routed by host), as their principal.
#[tokio::test]
async fn native_users_api_keys_and_tokens() {
    use base64::Engine;
    let t = start(NodeConfig { domain: Some("search.test".into()), ..NodeConfig::default() }, ListenOptions::default()).await;
    let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
    let owner = basic("c1", "pw1");
    let json = |r: &Resp| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap();
    let ct = ("Content-Type", "application/json");
    // `send` names `localhost`; routing by host needs the request's only Host header.
    async fn by_host(addr: std::net::SocketAddr, path: &str, auth: &str) -> Resp {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let raw = format!("GET {path} HTTP/1.1\r\nHost: c1.search.test\r\nAuthorization: {auth}\r\n\r\n");
        s.write_all(raw.as_bytes()).await.unwrap();
        read_response(&mut s, &mut Vec::new(), false).await
    }
    let r = send(t.addr, "PUT", "/_security/role/reader", &[("Authorization", &owner), ct], br#"{"cluster":["monitor","manage_own_api_key"],"indices":[{"names":["logs*"],"privileges":["read"]}]}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let r = send(t.addr, "PUT", "/_security/user/alice", &[("Authorization", &owner), ct], br#"{"password":"alicepw1","roles":["reader"]}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    // A native user, qualified by cluster or routed by host.
    let alice = basic("alice@c1", "alicepw1");
    let r = send(t.addr, "GET", "/_security/_authenticate", &[("Authorization", &alice)], b"").await;
    assert_eq!((r.status, json(&r)["username"].as_str()), (200, Some("alice")), "{}", r.text());
    assert_eq!(json(&r)["authentication_realm"]["type"], "native");
    let r = by_host(t.addr, "/_security/_authenticate", &basic("alice", "alicepw1")).await;
    assert_eq!((r.status, json(&r)["username"].as_str()), (200, Some("alice")), "{}", r.text());
    // Authorized by its roles: no index creation.
    let r = send(t.addr, "PUT", "/logs-1", &[("Authorization", &alice)], b"").await;
    assert_eq!((r.status, json(&r)["error"]["type"].as_str()), (403, Some("security_exception")), "{}", r.text());
    // A wrong password and another cluster's user fail like unknown credentials.
    for (u, p) in [("alice@c1", "nope"), ("alice@c2", "alicepw1")] {
        let r = send(t.addr, "GET", "/_security/_authenticate", &[("Authorization", &basic(u, p))], b"").await;
        assert_eq!(r.status, 401, "{u}: {}", r.text());
    }
    // `elastic` without a password of its own is the dev-mode owner: never through the node.
    let r = send(t.addr, "GET", "/", &[("Authorization", &basic("elastic@c1", "anything"))], b"").await;
    assert_eq!(r.status, 401, "{}", r.text());
    // An API key the user created.
    let r = send(t.addr, "POST", "/_security/api_key", &[("Authorization", &alice), ct], br#"{"name":"k"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let key = json(&r);
    let (id, secret) = (key["id"].as_str().unwrap().to_string(), key["api_key"].as_str().unwrap().to_string());
    let r = send(t.addr, "GET", "/_security/_authenticate", &[("Authorization", &format!("ApiKey {}", b64(&format!("{id}@c1:{secret}"))))], b"").await;
    assert_eq!((r.status, json(&r)["api_key"]["id"].as_str()), (200, Some(id.as_str())), "{}", r.text());
    let r = send(t.addr, "GET", "/_security/_authenticate", &[("Authorization", &format!("ApiKey {}", b64(&format!("{id}@c1:wrong"))))], b"").await;
    assert_eq!(r.status, 401, "{}", r.text());
    // An access token, routed by host.
    let r = send(t.addr, "POST", "/_security/oauth2/token", &[("Authorization", &owner), ct], br#"{"grant_type":"password","username":"alice","password":"alicepw1"}"#).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let token = json(&r)["access_token"].as_str().unwrap().to_string();
    let r = by_host(t.addr, "/_security/_authenticate", &format!("Bearer {token}")).await;
    assert_eq!((r.status, json(&r)["username"].as_str()), (200, Some("alice")), "{}", r.text());
}

/// `serve_until` stops accepting, lets the request in flight finish (its connection then
/// closes) and returns once every connection is gone.
#[tokio::test]
async fn graceful_shutdown_finishes_requests_in_flight() {
    let catalog = Arc::new(search_server::StaticCatalog::new());
    let mut spec = search_server::ClusterSpec::new("c1").with_password("pw1");
    spec.max_in_flight = 1;
    catalog.insert(spec);
    let node = search_server::Node::new(catalog, Arc::new(search_server::MemoryStores::new()), NodeConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = search_server::Shutdown::new();
    let server = tokio::spawn(search_server::serve_until(listener, node.clone(), ListenOptions::default(), shutdown.clone()));
    let auth = basic("c1", "pw1");
    // An idle keep-alive connection, and a request held behind the cluster's only permit.
    let mut idle = TcpStream::connect(addr).await.unwrap();
    assert_eq!(send_on(&mut idle, "GET", "/", &[("Authorization", &auth)], b"").await.status, 200);
    let mut r = search_es::EsRequest::new("GET", "/");
    r.headers.push(("authorization".into(), auth.clone()));
    let held = node.admit(&r, None).await.unwrap();
    let auth2 = auth.clone();
    let in_flight = tokio::spawn(async move { send(addr, "GET", "/_cluster/health", &[("Authorization", &auth2)], b"").await.status });
    tokio::time::sleep(Duration::from_millis(100)).await;
    shutdown.trigger();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!server.is_finished(), "waits for the request in flight");
    // New connections are refused (the listener is closed).
    assert!(TcpStream::connect(addr).await.is_err());
    drop(held);
    assert_eq!(in_flight.await.unwrap(), 200);
    tokio::time::timeout(Duration::from_secs(5), server).await.expect("server stops").unwrap().unwrap();
    // The idle keep-alive connection was closed.
    let mut buf = [0u8; 1];
    use tokio::io::AsyncReadExt;
    assert_eq!(idle.read(&mut buf).await.unwrap_or(0), 0);
}
