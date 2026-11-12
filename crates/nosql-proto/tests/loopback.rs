//! The client against a small in-process server built on `ServerCodec`: every version and
//! compression, SASL PLAIN, TLS, events, paging, warnings, multi-segment v5 messages,
//! timeouts and multiplexing. The reference images use AllowAllAuthenticator and no TLS, so
//! those paths are exercised here.

#![allow(clippy::field_reassign_with_default)] // QueryParams are built field by field

use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use nosql_proto::client::{Client, ClientConfig, ClientError, TlsConfig};
use nosql_proto::*;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::rustls;
use tokio_util::codec::Framed;

const AUTHENTICATOR: &str = "org.apache.cassandra.auth.PasswordAuthenticator";

fn rows(from: i32, to: i32, page: Option<i32>, cell: Option<Bytes>) -> Rows {
    let mut b = RowsBuilder::new();
    let end = page.map_or(to, |p| (from + p).min(to));
    for i in from..end {
        b.cell(Some(&i.to_be_bytes()));
        b.cell(cell.as_deref());
        b.end_row();
    }
    let mut md = RowsMetadata::new(vec![ColSpec::new("ks", "t", "n", ColType::Int), ColSpec::new("ks", "t", "x", ColType::Blob)]);
    if end < to {
        md.paging_state = Some(Bytes::copy_from_slice(&end.to_be_bytes()));
    }
    b.finish(md)
}

async fn serve<S: AsyncRead + AsyncWrite + Unpin>(io: S, auth: bool) {
    let mut conn = Framed::new(io, ServerCodec::new(Dialect::Cassandra));
    let mut ctx = Ctx::new(Version::V4);
    while let Some(item) = conn.next().await {
        let frame = match item.unwrap() {
            Inbound::Reject(r) => {
                let close = r.close;
                conn.send(r).await.unwrap();
                if close {
                    return;
                }
                continue;
            }
            Inbound::Frame(f) => f,
        };
        ctx.version = frame.version;
        let req = match RequestFrame::decode(ctx, &frame) {
            Ok(r) => r,
            Err(e) => {
                conn.send(ResponseFrame::error(frame.stream, e.to_error(Dialect::Cassandra)).to_frame(ctx)).await.unwrap();
                continue;
            }
        };
        let mut resp = ResponseFrame::new(frame.stream, Response::Ready);
        let mut then_segments = None;
        let mut then_event = false;
        match req.request {
            Request::Options => resp.response = Response::Supported(supported::cassandra()),
            Request::Startup(s) => {
                conn.codec_mut().decoder.established(frame.version);
                match s.validate(Dialect::Cassandra, frame.version) {
                    Ok(c) => {
                        conn.codec_mut().established(frame.version, c);
                        if frame.version.uses_segments() {
                            then_segments = Some(c == Some(Compression::Lz4));
                        }
                        if auth {
                            resp.response = Response::Authenticate(AUTHENTICATOR.into());
                        }
                    }
                    Err(e) => resp.response = Response::Error(e),
                }
            }
            Request::AuthResponse(token) => {
                resp.response = if token.as_deref() == Some(&b"\0user\0pass"[..]) {
                    Response::AuthSuccess(None)
                } else {
                    Response::Error(ErrorBody::new(code::BAD_CREDENTIALS, "Provided username user and/or password are incorrect"))
                };
            }
            Request::Register(names) => match EventType::parse_all(&names, Dialect::Cassandra) {
                Ok(_) => then_event = true,
                Err(e) => resp.response = Response::Error(e.to_error(Dialect::Cassandra)),
            },
            Request::Query(q) => {
                let from = q.params.paging_state.as_deref().map_or(0, |b| i32::from_be_bytes(b.try_into().unwrap()));
                resp.response = match q.query.as_str() {
                    "SLOW" => continue,
                    "ROWS" => Response::Result(ResultBody::Rows(rows(from, 10, q.params.page_size, None))),
                    "BIG" => Response::Result(ResultBody::Rows(rows(0, 2, None, Some(Bytes::from(vec![7u8; 300_000]))))),
                    "WARN" => {
                        resp.warnings = vec!["careful".into()];
                        Response::Result(ResultBody::Void)
                    }
                    "TRACE" if req.tracing => {
                        resp.tracing_id = Some([1; 16]);
                        Response::Result(ResultBody::Void)
                    }
                    "ECHO" => {
                        let mut b = RowsBuilder::new();
                        for v in &q.params.values {
                            b.cell(v.as_bytes().map(|b| &b[..]));
                        }
                        b.end_row();
                        let cols = (0..q.params.values.len()).map(|i| ColSpec::new("ks", "t", format!("c{i}"), ColType::Blob)).collect();
                        Response::Result(ResultBody::Rows(b.finish(RowsMetadata::new(cols))))
                    }
                    other => Response::Error(ErrorBody::new(code::SYNTAX_ERROR, format!("line 1:0 no viable alternative at input '{other}'"))),
                }
            }
            other => resp.response = Response::Error(ErrorBody::invalid(format!("unsupported {:?}", other.opcode()))),
        }
        conn.send(resp.to_frame(ctx)).await.unwrap();
        if let Some(lz4) = then_segments {
            conn.codec_mut().start_segments(lz4);
        }
        if then_event {
            let e = Event::SchemaChange(SchemaChange::object(SchemaChangeType::Created, SchemaTarget::Table, "ks", "t"));
            conn.send(ResponseFrame::event(e).to_frame(ctx)).await.unwrap();
        }
    }
}

async fn plain_server(auth: bool) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            tokio::spawn(serve(s, auth));
        }
    });
    addr
}

fn combos() -> Vec<(Version, Option<Compression>)> {
    let mut v = vec![];
    for ver in [Version::V3, Version::V4, Version::V5, Version::V6] {
        for c in [None, Some(Compression::Lz4), Some(Compression::Snappy)] {
            if !(ver.uses_segments() && c == Some(Compression::Snappy)) {
                v.push((ver, c));
            }
        }
    }
    v
}

async fn exercise(c: &Client, label: &str) {
    // Paging.
    let mut p = QueryParams::default();
    p.page_size = Some(3);
    let all = c.pages("ROWS", p).collect().await.unwrap();
    assert_eq!(all.len(), 10, "{label}");
    assert_eq!(all[9][0].as_deref(), Some(&9i32.to_be_bytes()[..]));
    // Values both ways, including null and unset.
    let mut p = QueryParams::default();
    p.values = vec![Value::new(&b"abc"[..]), Value::Null];
    if c.version() >= Version::V4 {
        p.values.push(Value::Unset);
    }
    let ResultBody::Rows(r) = c.query("ECHO", p).await.unwrap() else { panic!() };
    assert_eq!(r.to_vec()[0][..2], [Some(Bytes::from_static(b"abc")), None]);
    // 300 KB cells: several segments on v5.
    let ResultBody::Rows(r) = c.query("BIG", QueryParams::default()).await.unwrap() else { panic!() };
    assert_eq!(r.to_vec()[1][1].as_ref().map(Bytes::len), Some(300_000), "{label}");
    // Warnings exist from v4.
    let resp = c.send(RequestFrame::new(0, Request::Query(Query { query: "WARN".into(), params: QueryParams::default() }))).await.unwrap();
    assert_eq!(resp.warnings.len(), usize::from(c.version() >= Version::V4), "{label}");
    // Tracing.
    let resp = c
        .send(RequestFrame { stream: 0, tracing: true, custom_payload: None, request: Request::Query(Query { query: "TRACE".into(), params: QueryParams::default() }) })
        .await
        .unwrap();
    assert_eq!(resp.tracing_id, Some([1; 16]));
    // Errors.
    let Err(ClientError::Server(e)) = c.query("NOPE", QueryParams::default()).await else { panic!() };
    assert_eq!(e.code, code::SYNTAX_ERROR);
    // Many requests in flight at once.
    let futs: Vec<_> = (0..200).map(|_| c.query("WARN", QueryParams::default())).collect();
    for r in futures::future::join_all(futs).await {
        assert_eq!(r.unwrap(), ResultBody::Void);
    }
}

#[tokio::test]
async fn every_version_and_compression_with_auth() {
    let addr = plain_server(true).await;
    for (v, comp) in combos() {
        let label = format!("{v} {comp:?}");
        let c = Client::connect(ClientConfig::new(&addr).version(v).compression(comp).credentials("user", "pass")).await.unwrap();
        assert_eq!(c.version(), v);
        exercise(&c, &label).await;
        let mut rx = c.register(&[EventType::SchemaChange]).await.unwrap();
        let e = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
        assert_eq!(e.event_type(), EventType::SchemaChange);
        let Err(ClientError::Server(e)) = c.request(Request::Register(vec!["FOO".into()])).await else { panic!() };
        assert_eq!(e.message, "Invalid value 'FOO' for Type");
    }
}

#[tokio::test]
async fn auth_failures() {
    let addr = plain_server(true).await;
    let Err(ClientError::Server(e)) = Client::connect(ClientConfig::new(&addr).credentials("user", "nope")).await else { panic!() };
    assert_eq!(e.code, code::BAD_CREDENTIALS);
    assert!(matches!(Client::connect(ClientConfig::new(&addr)).await, Err(ClientError::Auth(_))));
}

#[tokio::test]
async fn negotiates_v5_and_reports_startup_errors() {
    let addr = plain_server(false).await;
    // Our server speaks Cassandra's versions; v6 is beta, v5 is negotiated by default.
    assert_eq!(Client::connect(ClientConfig::new(&addr)).await.unwrap().version(), Version::V5);
    let mut cfg = ClientConfig::new(&addr);
    cfg.startup.push(("COMPRESSION".into(), "zstd".into()));
    let Err(ClientError::Server(e)) = Client::connect(cfg.version(Version::V4)).await else { panic!() };
    assert_eq!(e.message, "Unknown compression algorithm: zstd");
}

#[tokio::test]
async fn timeouts_keep_the_stream_reserved() {
    let addr = plain_server(false).await;
    let mut cfg = ClientConfig::new(&addr);
    cfg.request_timeout = Some(Duration::from_millis(200));
    let c = Client::connect(cfg).await.unwrap();
    assert!(matches!(c.query("SLOW", QueryParams::default()).await, Err(ClientError::Timeout)));
    assert_eq!(c.query("WARN", QueryParams::default()).await.unwrap(), ResultBody::Void);
}

/// Every stream id taken by requests the server hasn't answered (here: timed out ones, whose
/// ids stay reserved): the next request fails at once with `Busy` instead of waiting or reusing
/// an id, and the ids come back with the late answers.
#[tokio::test]
async fn stream_id_exhaustion() {
    const IDS: usize = i16::MAX as usize - 1; // 1..=32766; 0 is the handshake's
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let release = Arc::new(tokio::sync::Notify::new());
    let held_all = Arc::new(tokio::sync::Notify::new());
    let (r, h) = (release.clone(), held_all.clone());
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        let mut conn = Framed::new(s, ServerCodec::new(Dialect::Cassandra));
        let mut held = Vec::new();
        while let Some(Ok(Inbound::Frame(f))) = conn.next().await {
            let ctx = Ctx::new(f.version);
            let resp = match f.opcode {
                Opcode::Options => Response::Supported(supported::cassandra()),
                Opcode::Startup => {
                    conn.codec_mut().established(f.version, None);
                    Response::Ready
                }
                _ => {
                    held.push(f.stream);
                    if held.len() < IDS {
                        continue;
                    }
                    h.notify_one();
                    r.notified().await;
                    let answers: Vec<Frame> = held.drain(..).map(|st| ResponseFrame::new(st, Response::Result(ResultBody::Void)).to_frame(ctx)).collect();
                    conn.send(answers).await.unwrap();
                    continue;
                }
            };
            conn.send(ResponseFrame::new(f.stream, resp).to_frame(ctx)).await.unwrap();
        }
    });
    let mut cfg = ClientConfig::new(&addr).version(Version::V4);
    cfg.request_timeout = Some(Duration::from_millis(300));
    cfg.max_in_flight = 4096;
    let c = Client::connect(cfg).await.unwrap();
    let all = futures::stream::iter(0..IDS).map(|_| c.query("HOLD", QueryParams::default())).buffer_unordered(4096).collect::<Vec<_>>().await;
    let other: Vec<_> = all.iter().filter(|r| !matches!(r, Err(ClientError::Timeout))).take(3).collect();
    assert!(other.is_empty(), "{other:?}");
    held_all.notified().await;
    let busy = tokio::time::timeout(Duration::from_secs(5), c.query("HOLD", QueryParams::default())).await.expect("fails at once");
    assert!(matches!(busy, Err(ClientError::Busy)), "{busy:?}");
    // The late answers free the ids; the connection is still usable.
    release.notify_one();
    let mut ok = false;
    for _ in 0..100 {
        match c.query("HOLD", QueryParams::default()).await {
            Err(ClientError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
            // The server holds new requests again (until it has every id).
            Err(ClientError::Timeout) => {
                ok = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(ok, "ids are free again");
}

#[tokio::test]
async fn closed_connections_fail_pending_requests() {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.unwrap();
        // Handshake, then hang up on the first query.
        let mut conn = Framed::new(s, ServerCodec::new(Dialect::Cassandra));
        while let Some(Ok(Inbound::Frame(f))) = conn.next().await {
            let ctx = Ctx::new(f.version);
            let resp = match f.opcode {
                Opcode::Options => Response::Supported(supported::cassandra()),
                Opcode::Startup => Response::Ready,
                _ => return,
            };
            conn.send(ResponseFrame::new(f.stream, resp).to_frame(ctx)).await.unwrap();
        }
    });
    let c = Client::connect(ClientConfig::new(&addr).version(Version::V4)).await.unwrap();
    assert!(matches!(c.query("SLOW", QueryParams::default()).await, Err(ClientError::Closed)));
    assert!(matches!(c.query("SLOW", QueryParams::default()).await, Err(ClientError::Closed)));
}

#[tokio::test]
async fn tls() {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = ck.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::try_from(ck.signing_key.serialize_der()).unwrap();
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(s).await {
                    serve(tls, false).await;
                }
            });
        }
    });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let tls = TlsConfig::with_roots(roots.clone(), "localhost").unwrap();
    for v in [Version::V4, Version::V5] {
        let c = Client::connect(ClientConfig::new(&addr).version(v).compression(Some(Compression::Lz4)).tls(tls.clone())).await.unwrap();
        exercise(&c, "tls").await;
    }
    // The wrong name fails the handshake.
    let bad = TlsConfig::with_roots(roots, "example.com").unwrap();
    assert!(Client::connect(ClientConfig::new(&addr).tls(bad)).await.is_err());
}

#[test]
fn rows_builder_matches_wire() {
    let r = rows(0, 2, None, None);
    let mut b = BytesMut::new();
    ResultBody::Rows(r.clone()).encode(Version::V4, &mut b);
    let back = ResultBody::decode(Version::V4, &mut Reader::new(b.freeze())).unwrap();
    assert_eq!(back, ResultBody::Rows(r));
}
