//! The HTTP/1.1 listener (hyper): keep-alive and pipelining, optional TLS (with SNI routing)
//! and PROXY protocol, gzip/deflate request bodies and gzip responses, `HEAD` answered with the
//! `GET` response's length and no body, and `http.max_content_length` enforced before the body
//! is buffered.
//!
//! A request is authenticated and pins its cluster first ([`Node::enter`]), then its body is
//! read — with [`BODY_IDLE_TIMEOUT`] between chunks and [`BODY_TIMEOUT`] overall — and only then
//! does it take one of the cluster's concurrency permits, so stalled uploads can't exhaust them.

use crate::net::read_proxy_header;
use crate::node::Node;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use search_es::{EsError, EsRequest, EsResponse};
use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Longest wait for the next chunk of a request body.
pub const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest time to read a whole request body.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Default)]
pub struct ListenOptions {
    pub tls: Option<TlsAcceptor>,
    /// Expect a PROXY protocol header on every connection (behind an NLB / HAProxy).
    pub proxy_protocol: bool,
    /// Overrides [`BODY_IDLE_TIMEOUT`] and [`BODY_TIMEOUT`] (tests).
    pub body_timeouts: Option<(Duration, Duration)>,
}

/// Accepts connections until the listener fails.
pub async fn serve(listener: TcpListener, node: Arc<Node>, opts: ListenOptions) -> anyhow::Result<()> {
    loop {
        let (mut sock, _peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Out of file descriptors and similar: back off instead of spinning.
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let node = node.clone();
        let opts = opts.clone();
        tokio::spawn(async move {
            if opts.proxy_protocol {
                match tokio::time::timeout(Duration::from_secs(5), read_proxy_header(&mut sock)).await {
                    Ok(Ok(_)) => {}
                    _ => return,
                }
            }
            match opts.tls {
                Some(acceptor) => {
                    let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await else { return };
                    let sni = tls.get_ref().1.server_name().map(str::to_string);
                    serve_connection(tls, node, sni, opts.body_timeouts).await;
                }
                None => serve_connection(sock, node, None, opts.body_timeouts).await,
            }
        });
    }
}

/// Binds `addr` and serves in the background; returns the bound address (tests, dev server).
pub async fn spawn(addr: &str, node: Arc<Node>, opts: ListenOptions) -> anyhow::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let task = tokio::spawn(async move {
        if let Err(e) = serve(listener, node, opts).await {
            tracing::error!(error = %e, "search listener stopped");
        }
    });
    Ok((local, task))
}

async fn serve_connection<S>(io: S, node: Arc<Node>, sni: Option<String>, body_timeouts: Option<(Duration, Duration)>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let sni = sni.map(Arc::<str>::from);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let node = node.clone();
        let sni = sni.clone();
        let timeouts = body_timeouts.unwrap_or((BODY_IDLE_TIMEOUT, BODY_TIMEOUT));
        async move { Ok::<_, Infallible>(respond(node, req, sni.as_deref(), timeouts).await) }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    // A client gets 30 s to send each request's headers (slowloris protection).
    builder
        .keep_alive(true)
        .pipeline_flush(true)
        .max_buf_size(1 << 20)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    if let Err(e) = builder.serve_connection(TokioIo::new(io), svc).await {
        tracing::trace!(error = %e, "connection closed");
    }
}

fn head_of(req: &Request<Incoming>) -> EsRequest {
    let uri = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let mut headers = Vec::with_capacity(req.headers().len());
    for (k, v) in req.headers() {
        headers.push((k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()));
    }
    EsRequest { method: req.method().as_str().to_string(), uri, headers, body: Bytes::new() }
}

/// Whether the client takes gzip: `Accept-Encoding` lists `gzip` (or `x-gzip`, or `*`) with a
/// non-zero quality.
fn accepts_gzip(header: Option<&str>) -> bool {
    let Some(h) = header else { return false };
    h.split(',').any(|item| {
        let mut parts = item.split(';');
        let coding = parts.next().unwrap_or("").trim();
        if !(coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") || coding == "*") {
            return false;
        }
        // `q=0` means "not acceptable".
        !parts.any(|p| {
            let p = p.trim();
            let Some(q) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) else { return false };
            q.trim().parse::<f32>().is_ok_and(|q| q <= 0.0)
        })
    })
}

enum BodyError {
    TooLong,
    TimedOut,
    Broken,
}

/// Reads the whole body, at most `max` bytes, within the body timeouts.
async fn read_body(body: Incoming, max: usize, (idle, total): (Duration, Duration)) -> Result<Bytes, BodyError> {
    let mut body = Limited::new(body, max);
    let deadline = tokio::time::Instant::now() + total;
    let mut out = bytes::BytesMut::new();
    loop {
        let next = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + idle), body.frame()).await;
        match next {
            Err(_) => return Err(BodyError::TimedOut),
            Ok(None) => return Ok(out.freeze()),
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    out.extend_from_slice(&data);
                }
            }
            Ok(Some(Err(e))) => {
                return Err(if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() { BodyError::TooLong } else { BodyError::Broken });
            }
        }
    }
}

async fn respond(node: Arc<Node>, req: Request<Incoming>, sni: Option<&str>, timeouts: (Duration, Duration)) -> Response<Full<Bytes>> {
    let mut es = head_of(&req);
    let gzip_ok = accepts_gzip(es.header("accept-encoding"));
    let mut lease = match node.enter(&es, sni).await {
        Ok(l) => l,
        Err(r) => return to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok),
    };
    let dialect = lease.loaded.spec().dialect;
    let max = lease.max_body_bytes(&node);
    let too_long = |len: u64| search_es::rest::error_response(&EsError::content_too_long(len, max as u64), &es, dialect);
    if let Some(len) = es.header("content-length").and_then(|v| v.trim().parse::<u64>().ok())
        && len > max as u64
    {
        return to_hyper(too_long(len), gzip_ok);
    }
    let body = match read_body(req.into_body(), max, timeouts).await {
        Ok(b) => b,
        Err(BodyError::TooLong) => return to_hyper(too_long(max as u64 + 1), gzip_ok),
        Err(BodyError::TimedOut) => {
            let e = EsError::new(408, "timeout_exception", "timed out reading the request body");
            let mut r = to_hyper(search_es::rest::error_response(&e, &es, dialect), gzip_ok);
            r.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
            return r;
        }
        // The client went away mid-body: nobody to answer.
        Err(BodyError::Broken) => {
            let mut r = Response::new(Full::new(Bytes::new()));
            *r.status_mut() = StatusCode::BAD_REQUEST;
            r.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
            return r;
        }
    };
    let body = match es.header("content-encoding").map(|v| v.trim().to_ascii_lowercase()) {
        Some(enc) if enc == "gzip" || enc == "x-gzip" || enc == "deflate" => match inflate(&body, &enc, max) {
            Ok(b) => b,
            Err(Inflate::TooLong) => return to_hyper(too_long(max as u64 + 1), gzip_ok),
            Err(Inflate::Corrupt(msg)) => {
                return to_hyper(search_es::rest::error_response(&EsError::illegal_argument(msg), &es, dialect), gzip_ok);
            }
        },
        _ => body,
    };
    es.body = body;
    if let Err(r) = lease.acquire(&es).await {
        return to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok);
    }
    let resp = lease.handle(es).await;
    drop(lease);
    to_hyper(resp, gzip_ok)
}

enum Inflate {
    TooLong,
    Corrupt(String),
}

/// Decompresses a request body, refusing to expand past `max` bytes.
fn inflate(body: &[u8], enc: &str, max: usize) -> Result<Bytes, Inflate> {
    let mut out = Vec::with_capacity(body.len().saturating_mul(4).min(max));
    let reader: Box<dyn Read> = if enc == "gzip" || enc == "x-gzip" { Box::new(flate2::read::GzDecoder::new(body)) } else { Box::new(flate2::read::ZlibDecoder::new(body)) };
    let mut limited = reader.take(max as u64 + 1);
    limited.read_to_end(&mut out).map_err(|e| Inflate::Corrupt(format!("failed to decompress request body: {e}")))?;
    if out.len() > max {
        return Err(Inflate::TooLong);
    }
    Ok(Bytes::from(out))
}

fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 3 + 32), flate2::Compression::new(3));
    enc.write_all(body).ok()?;
    enc.finish().ok()
}

fn to_hyper(resp: EsResponse, accepts_gzip: bool) -> Response<Full<Bytes>> {
    let EsResponse { status, headers, body, head } = resp;
    let (body, gzipped) = if accepts_gzip && !body.is_empty() {
        match gzip(&body) {
            Some(z) => (Bytes::from(z), true),
            None => (body, false),
        }
    } else {
        (body, false)
    };
    let len = body.len();
    let mut out = Response::new(if head { Full::new(Bytes::new()) } else { Full::new(body) });
    *out.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = out.headers_mut();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v.as_str())) {
            h.append(name, value);
        }
    }
    if gzipped {
        h.insert(hyper::header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    h.insert(hyper::header::CONTENT_LENGTH, HeaderValue::from(len));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_encoding() {
        assert!(accepts_gzip(Some("gzip")));
        assert!(accepts_gzip(Some("deflate, gzip;q=0.5")));
        assert!(accepts_gzip(Some("x-gzip")));
        assert!(accepts_gzip(Some("*")));
        assert!(!accepts_gzip(Some("gzip;q=0")));
        assert!(!accepts_gzip(Some("gzip; q=0.0, deflate")));
        assert!(!accepts_gzip(Some("identity")));
        assert!(!accepts_gzip(None));
    }

    #[test]
    fn x_gzip_bodies() {
        let z = gzip(b"{}").unwrap();
        assert_eq!(&inflate(&z, "x-gzip", 100).ok().unwrap()[..], b"{}");
    }

    #[test]
    fn inflate_is_bounded() {
        let big = vec![b'a'; 10_000];
        let z = gzip(&big).unwrap();
        assert_eq!(inflate(&z, "gzip", 20_000).ok().unwrap().len(), 10_000);
        assert!(matches!(inflate(&z, "gzip", 100), Err(Inflate::TooLong)));
        assert!(matches!(inflate(b"not gzip", "gzip", 100), Err(Inflate::Corrupt(_))));
    }
}
