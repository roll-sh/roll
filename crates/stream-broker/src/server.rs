//! The Kafka listener. Each connection reads frames in order and dispatches them; work that has
//! to wait (fsync for `acks=all`, long-poll fetches, rebalances) runs as its own task so later
//! pipelined requests aren't blocked, while responses are still written in request order as the
//! protocol requires.
//!
//! On the fleet (`fleet.rs`) the first request that needs the connection's stream decides where
//! it runs: here under the stream's lease, or spliced to the owner with what was read so far.
//! When the stream moves the connection closes (after a moment, on a handoff) and the client
//! reconnects to the same hostname, which reaches the new owner.

use crate::auth;
use crate::broker::{Broker, Stream};
use crate::fleet::{Hop, Io, Lease, Routed};
use crate::handlers::{self, Handled, Reply};
use bytes::{BufMut, Bytes, BytesMut};
use futures::future::BoxFuture;
use kafka_protocol::messages::{ApiKey, RequestHeader, ResponseHeader, ResponseKind};
use kafka_protocol::protocol::{Decodable, Encodable};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Per-connection state.
pub struct Session {
    pub peer: SocketAddr,
    pub client_id: String,
    /// The tenant: from SASL, or the default stream for unauthenticated listeners.
    pub stream: Option<Arc<Stream>>,
    pub principal: Option<String>,
    pub mechanism: Option<auth::Mechanism>,
    pub sasl: Option<auth::Session>,
    /// After SaslHandshake v0 the next frames are raw SASL tokens, not Kafka requests.
    pub raw_sasl: bool,
    pub tls: bool,
    /// The broker address (host, port) metadata gives this connection: the config's, or the
    /// hostname the client connected with (the edge: one virtual broker per tenant hostname).
    pub advertised: Arc<(String, u16)>,
    /// The fleet: the connection serves its stream on this node, under the stream's lease.
    pub lease: Option<Lease>,
    /// The stream can't change any more (the connection was routed or spliced for it): a SASL
    /// re-authentication as another stream's key fails.
    pub pinned: bool,
}

#[allow(clippy::large_enum_variant)]
enum Out {
    Response { correlation_id: i32, api: ApiKey, version: i16, body: Body },
    Raw(Bytes),
    Close,
    /// Write what's queued and hand the transport back (the connection is spliced elsewhere).
    Detach,
}

#[allow(clippy::large_enum_variant)]
enum Body {
    Now(ResponseKind),
    Later(JoinHandle<ResponseKind>),
}

pub async fn serve(broker: Arc<Broker>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(broker.config.listen).await?;
    serve_on(broker, listener).await
}

pub async fn serve_on(broker: Arc<Broker>, listener: TcpListener) -> anyhow::Result<()> {
    let tls = match &broker.config.tls {
        Some(files) => Some(crate::tls::acceptor(files)?),
        None => None,
    };
    info!(addr = %listener.local_addr()?, tls = tls.is_some(), "kafka listener up");
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let b = broker.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                Some(acceptor) => match acceptor.accept(sock).await {
                    Ok(s) => serve_connection(b, s, peer, true, None).await,
                    Err(e) => debug!(%peer, error = %e, "TLS handshake failed"),
                },
                None => serve_connection(b, sock, peer, false, None).await,
            }
        });
    }
}

/// Serve one connection whose transport is already set up (a TLS stream from the edge, a
/// socket). `advertised`: the broker address to give the client instead of the config's.
pub async fn serve_connection<S: Io>(b: Arc<Broker>, io: S, peer: SocketAddr, tls: bool, advertised: Option<(String, u16)>) {
    run(b, io, Bytes::new(), peer, tls, advertised, None).await
}

/// Serve a connection another node accepted and spliced here (the fleet): it authenticated as
/// `stream` there, `first` is what that node read of it, and the client was told `advertised`.
pub async fn serve_spliced<S: Io>(b: Arc<Broker>, io: S, first: Bytes, peer: SocketAddr, stream: &str, advertised: Option<(String, u16)>) {
    let Some(stream) = b.stream(stream) else {
        debug!(%peer, stream, "a spliced connection for a stream that doesn't exist");
        return;
    };
    run(b, io, first, peer, true, advertised, Some(stream)).await
}

async fn run<S: Io>(b: Arc<Broker>, io: S, first: Bytes, peer: SocketAddr, tls: bool, advertised: Option<(String, u16)>, spliced: Option<Arc<Stream>>) {
    let advertised = Arc::new(advertised.unwrap_or_else(|| (b.config.advertised_host.clone(), b.config.advertised_port)));
    let (r, w) = tokio::io::split(io);
    let mut reader = BufReader::with_capacity(64 * 1024, std::io::Cursor::new(first).chain(r));
    let (tx, rx) = mpsc::channel::<Out>(b.config.max_in_flight.max(1));
    let writer = tokio::spawn(write_loop(BufWriter::with_capacity(64 * 1024, w), rx));
    let was_spliced = spliced.is_some();
    let stream = spliced.or_else(|| b.config.default_stream.as_deref().and_then(|s| b.find_stream(s)));
    let mut session = Session {
        peer,
        client_id: String::new(),
        stream,
        principal: None,
        mechanism: None,
        sasl: None,
        raw_sasl: false,
        tls,
        advertised,
        lease: None,
        pinned: was_spliced,
    };
    // Resolves when the stream moves off this node: the connection then closes.
    let mut leaving: BoxFuture<'static, ()> = Box::pin(std::future::pending());
    let max = b.config.max_request_bytes;
    loop {
        let len = tokio::select! {
            biased;
            _ = &mut leaving => break,
            r = reader.read_u32() => match r {
                Ok(len) => len as usize,
                Err(_) => break,
            },
        };
        // Raw SASL tokens can be short; Kafka request headers are at least 8 bytes.
        if len > max || len < if session.raw_sasl { 1 } else { 8 } {
            debug!(%peer, len, "rejecting frame size");
            break;
        }
        let mut buf = BytesMut::zeroed(len);
        if reader.read_exact(&mut buf).await.is_err() {
            break;
        }
        let raw = buf.freeze();
        let mut frame = raw.clone();

        if session.raw_sasl {
            match handlers::sasl::raw_token(&b, &mut session, &frame) {
                Some(resp) => {
                    if tx.send(Out::Raw(resp)).await.is_err() {
                        break;
                    }
                    continue;
                }
                None => break,
            }
        }

        let api_key = i16::from_be_bytes([frame[0], frame[1]]);
        let version = i16::from_be_bytes([frame[2], frame[3]]);
        let Ok(api) = ApiKey::try_from(api_key) else {
            debug!(%peer, api_key, "unknown api key; closing");
            break;
        };
        let header = match RequestHeader::decode(&mut frame, api.request_header_version(version)) {
            Ok(h) => h,
            Err(e) => {
                debug!(%peer, error = %e, "bad request header");
                break;
            }
        };
        if let Some(c) = &header.client_id {
            session.client_id.clear();
            session.client_id.push_str(c);
        }
        let correlation_id = header.correlation_id;

        if !handlers::supported(api, version) {
            if api == ApiKey::ApiVersions {
                // Answer at v0 with what we support so the client can pick a version.
                let resp = handlers::api_versions_unsupported();
                let _ = tx.send(Out::Response { correlation_id, api, version: 0, body: Body::Now(resp) }).await;
                continue;
            }
            debug!(%peer, ?api, version, "unsupported version; closing");
            break;
        }
        let needs_stream = !matches!(api, ApiKey::ApiVersions | ApiKey::SaslHandshake | ApiKey::SaslAuthenticate);
        if session.stream.is_none() && needs_stream {
            debug!(%peer, ?api, "request before authentication; closing");
            break;
        }

        if b.fleet.is_some() && needs_stream && session.lease.is_none() {
            let stream = session.stream.clone().expect("checked above");
            match b.route_stream(&stream).await {
                Routed::Local(lease) => {
                    leaving = leave_when_moving(&b, &lease);
                    session.lease = Some(lease);
                    session.pinned = true;
                }
                // A connection spliced here never moves on again: the client reconnects instead.
                Routed::Forward(node) if !was_spliced => {
                    let _ = tx.send(Out::Detach).await;
                    drop(tx);
                    let Ok(Some(w)) = writer.await else { return };
                    let mut first = BytesMut::with_capacity(4 + raw.len() + reader.buffer().len());
                    first.put_u32(len as u32);
                    first.put_slice(&raw);
                    first.put_slice(reader.buffer());
                    let (_, r) = reader.into_inner().into_inner();
                    let io = r.unsplit(w.into_inner());
                    let hop = Hop { stream: stream.id.clone(), peer, advertised: Some((*session.advertised).clone()), first: first.freeze() };
                    debug!(%peer, stream = %stream.id, to = %node.id, "splicing the connection to the stream's owner");
                    if let Err(e) = b.fleet().splicer.splice(&node, Box::new(io), hop).await {
                        debug!(%peer, stream = %stream.id, to = %node.id, error = %e, "a spliced connection ended with an error");
                    }
                    return;
                }
                _ => {
                    debug!(%peer, stream = %stream.id, "the stream has no owner that can serve it; closing");
                    break;
                }
            }
        }

        let Handled { reply, throttle_ms } = handlers::handle(&b, &mut session, &header, api, version, frame).await;
        if let Some(s) = &session.stream {
            s.usage.requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let out = match reply {
            Reply::Now(r) => Out::Response { correlation_id, api, version, body: Body::Now(r) },
            Reply::Later(f) => Out::Response { correlation_id, api, version, body: Body::Later(tokio::spawn(f)) },
            Reply::None => continue,
            Reply::Close => Out::Close,
        };
        let close = matches!(out, Out::Close);
        if tx.send(out).await.is_err() || close {
            break;
        }
        if throttle_ms > 0 {
            // KIP-219: the response carries the throttle time; stop reading for that long.
            tokio::time::sleep(Duration::from_millis(throttle_ms as u64)).await;
        }
    }
    drop(tx);
    let _ = writer.await;
    // Requests in flight are answered before the lease's guard goes (a handoff waits for it).
    drop(session);
}

/// Resolves when the connection should close because its stream is leaving this node: at once
/// if the lease is lost, or at a random moment within a short spread on a handoff, so the
/// stream's clients don't all reconnect at the same instant. The owner keeps serving meanwhile.
fn leave_when_moving(b: &Arc<Broker>, lease: &Lease) -> BoxFuture<'static, ()> {
    let ending = lease.guard.ending();
    let cluster = b.fleet().cluster.clone();
    let key = lease.guard.key().to_string();
    Box::pin(async move {
        ending.await;
        if cluster.owns(&key) {
            let spread = (cluster.config().handoff_timeout / 4).min(Duration::from_secs(1));
            let r = u16::from_be_bytes(crate::ids::random_bytes::<2>()) as f64 / u16::MAX as f64;
            tokio::time::sleep(spread.mul_f64(r)).await;
        }
    })
}

async fn write_loop<W: AsyncWrite + Unpin>(mut w: BufWriter<W>, mut rx: mpsc::Receiver<Out>) -> Option<BufWriter<W>> {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    let mut detach = false;
    while let Some(out) = rx.recv().await {
        match out {
            Out::Close => break,
            Out::Detach => {
                detach = true;
                break;
            }
            Out::Raw(data) => {
                buf.put_u32(data.len() as u32);
                buf.put_slice(&data);
            }
            Out::Response { correlation_id, api, version, body } => {
                let resp = match body {
                    Body::Now(r) => r,
                    Body::Later(h) => match h.await {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(error = %e, ?api, "request handler failed");
                            break;
                        }
                    },
                };
                if let Err(e) = encode_response(&mut buf, correlation_id, api, version, &resp) {
                    warn!(error = %e, ?api, version, "encoding response");
                    break;
                }
            }
        }
        // Coalesce pipelined responses into one write.
        if rx.is_empty() || buf.len() > 1 << 20 {
            if w.write_all(&buf).await.is_err() || w.flush().await.is_err() {
                return None;
            }
            buf.clear();
        }
    }
    if !buf.is_empty() && w.write_all(&buf).await.is_err() {
        return None;
    }
    if w.flush().await.is_err() {
        return None;
    }
    if detach {
        return Some(w);
    }
    let _ = w.shutdown().await;
    None
}

/// Append one framed response. On failure nothing is appended, so a bad response can never
/// leave a torn frame on the wire.
pub fn encode_response(buf: &mut BytesMut, correlation_id: i32, api: ApiKey, version: i16, resp: &ResponseKind) -> anyhow::Result<()> {
    let start = buf.len();
    let result = (|| {
        buf.put_u32(0);
        let mut header = ResponseHeader::default();
        header.correlation_id = correlation_id;
        header.encode(buf, api.response_header_version(version))?;
        resp.encode(buf, version)
    })();
    if let Err(e) = result {
        buf.truncate(start);
        return Err(e);
    }
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}
