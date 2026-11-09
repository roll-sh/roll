//! The Kafka listener. Each connection reads frames in order and dispatches them; work that has
//! to wait (fsync for `acks=all`, long-poll fetches, rebalances) runs as its own task so later
//! pipelined requests aren't blocked, while responses are still written in request order as the
//! protocol requires.

use crate::auth;
use crate::broker::{Broker, Stream};
use crate::handlers::{self, Handled, Reply};
use bytes::{BufMut, Bytes, BytesMut};
use kafka_protocol::messages::{ApiKey, RequestHeader, ResponseHeader, ResponseKind};
use kafka_protocol::protocol::{Decodable, Encodable};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
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
}

#[allow(clippy::large_enum_variant)]
enum Out {
    Response { correlation_id: i32, api: ApiKey, version: i16, body: Body },
    Raw(Bytes),
    Close,
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
                    Ok(s) => connection(b, s, peer, true).await,
                    Err(e) => debug!(%peer, error = %e, "TLS handshake failed"),
                },
                None => connection(b, sock, peer, false).await,
            }
        });
    }
}

async fn connection<S: AsyncRead + AsyncWrite + Send + 'static>(b: Arc<Broker>, io: S, peer: SocketAddr, tls: bool) {
    let (r, w) = tokio::io::split(io);
    let mut reader = BufReader::with_capacity(64 * 1024, r);
    let (tx, rx) = mpsc::channel::<Out>(b.config.max_in_flight.max(1));
    let writer = tokio::spawn(write_loop(BufWriter::with_capacity(64 * 1024, w), rx));
    let default_stream = b.config.default_stream.as_deref().and_then(|s| b.find_stream(s));
    let mut session = Session {
        peer,
        client_id: String::new(),
        stream: default_stream,
        principal: None,
        mechanism: None,
        sasl: None,
        raw_sasl: false,
        tls,
    };
    let max = b.config.max_request_bytes;
    while let Ok(len) = reader.read_u32().await {
        let len = len as usize;
        // Raw SASL tokens can be short; Kafka request headers are at least 8 bytes.
        if len > max || len < if session.raw_sasl { 1 } else { 8 } {
            debug!(%peer, len, "rejecting frame size");
            break;
        }
        let mut buf = BytesMut::zeroed(len);
        if reader.read_exact(&mut buf).await.is_err() {
            break;
        }
        let mut frame = buf.freeze();

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
        if session.stream.is_none() && !matches!(api, ApiKey::ApiVersions | ApiKey::SaslHandshake | ApiKey::SaslAuthenticate) {
            debug!(%peer, ?api, "request before authentication; closing");
            break;
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
}

async fn write_loop<W: AsyncWrite + Unpin>(mut w: BufWriter<W>, mut rx: mpsc::Receiver<Out>) {
    let mut buf = BytesMut::with_capacity(64 * 1024);
    while let Some(out) = rx.recv().await {
        match out {
            Out::Close => break,
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
                break;
            }
            buf.clear();
        }
    }
    if !buf.is_empty() {
        let _ = w.write_all(&buf).await;
    }
    let _ = w.flush().await;
    let _ = w.shutdown().await;
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
