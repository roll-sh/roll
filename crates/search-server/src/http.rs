//! The HTTP/1.1 listener (hyper): keep-alive and pipelining, optional TLS (with SNI routing)
//! and PROXY protocol, gzip/deflate request bodies and gzip responses, `HEAD` answered with the
//! `GET` response's length and no body, and `http.max_content_length` enforced before the body
//! is buffered.
//!
//! A request is authenticated and pins its cluster first ([`Node::enter`], which also applies
//! the cluster's request rate), reserves its `Content-Length` from the cluster's in-flight byte
//! budget (a chunked body reserves each frame as it arrives, and a compressed body its output
//! as it inflates), then its body is read — with [`BODY_IDLE_TIMEOUT`] between chunks and
//! [`BODY_TIMEOUT`] overall — and only then does it take the cluster's concurrency permit and a
//! node-wide running slot, so stalled uploads can't exhaust them.
//!
//! Elasticsearch's HTTP limits and failures, as Netty reports them: an `Expect` other than
//! `100-continue` gets an empty `417 Expectation Failed`; a request line over
//! [`MAX_INITIAL_LINE`] bytes or headers over [`MAX_HEADER_SIZE`] get 400
//! `too_long_http_line_exception` / `too_long_http_header_exception` and the connection closes;
//! a body over `http.max_content_length` gets an empty `413 Request Entity Too Large` and the
//! connection closes; a truncated compressed body is passed on as far as it inflates, and one
//! that isn't valid gzip/deflate closes the connection without an answer. `deflate` bodies may
//! be zlib-wrapped or raw (Netty's `ZLIB_OR_NONE`).
//!
//! Inflating a request body or gzipping a response larger than [`INLINE_CODEC_BYTES`] runs on
//! the blocking pool, so a tenant's large bulk can't stall the other tenants' tasks on a worker.
//!
//! [`serve_until`] stops gracefully: it stops accepting, lets every request in flight finish
//! (closing keep-alive connections after their current response) and returns once all
//! connections are gone. A request still running after [`DRAIN_TIMEOUT`] has its task
//! cancelled ([`Node::cancel_tasks`]: a search without a timeout answers with its
//! cancellation instead of holding the stop), and the stop goes ahead after as long again. The
//! caller then shuts the node down ([`Node::shutdown`]), which flushes.

use crate::net::read_proxy_header;
use crate::node::{Node, Rejected};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use search_es::{EsError, EsRequest, EsResponse};
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
/// `http.max_initial_line_length` (Elasticsearch default): the request line, without its CRLF.
pub const MAX_INITIAL_LINE: usize = 4096;
/// `http.max_header_size` (Elasticsearch default): all header lines together, without CRLFs.
pub const MAX_HEADER_SIZE: usize = 16 * 1024;
/// Bodies larger than this are (de)compressed on the blocking pool, not on a worker.
pub const INLINE_CODEC_BYTES: usize = 64 * 1024;
/// Inflated output is reserved from the byte budget in steps of this size.
const INFLATE_STEP: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct ListenOptions {
    pub tls: Option<TlsAcceptor>,
    /// Expect a PROXY protocol header on every connection (behind an NLB / HAProxy).
    pub proxy_protocol: bool,
    /// Overrides [`BODY_IDLE_TIMEOUT`] and [`BODY_TIMEOUT`] (tests).
    pub body_timeouts: Option<(Duration, Duration)>,
    /// Overrides [`DRAIN_TIMEOUT`] (tests).
    pub drain_timeout: Option<Duration>,
}

/// How long a graceful stop lets requests in flight finish before cancelling the node's tasks
/// (searches, by-query runs); they then get as long again to answer.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Triggers a graceful stop of [`serve_until`].
#[derive(Clone)]
pub struct Shutdown(tokio::sync::watch::Sender<bool>);

impl Default for Shutdown {
    fn default() -> Self {
        Shutdown(tokio::sync::watch::channel(false).0)
    }
}

impl Shutdown {
    pub fn new() -> Shutdown {
        Shutdown::default()
    }

    pub fn trigger(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once [`Shutdown::trigger`] ran.
    pub async fn wait(&self) {
        let mut rx = self.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// Accepts connections until the listener fails.
pub async fn serve(listener: TcpListener, node: Arc<Node>, opts: ListenOptions) -> anyhow::Result<()> {
    serve_until(listener, node, opts, Shutdown::new()).await
}

/// Counts open connections; the last one to close wakes the waiting server.
struct Conns {
    open: std::sync::atomic::AtomicUsize,
    idle: tokio::sync::Notify,
}

struct ConnGuard(Arc<Conns>);

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if self.0.open.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Accepts connections until `shutdown` triggers, then waits for the open ones to finish
/// their requests and close.
pub async fn serve_until(listener: TcpListener, node: Arc<Node>, opts: ListenOptions, shutdown: Shutdown) -> anyhow::Result<()> {
    let conns = Arc::new(Conns { open: Default::default(), idle: tokio::sync::Notify::new() });
    let mut stop = shutdown.subscribe();
    loop {
        let stopped = *stop.borrow();
        if stopped {
            break;
        }
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = stop.changed() => break,
        };
        let (mut sock, _peer) = match accepted {
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
        conns.open.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = ConnGuard(conns.clone());
        let stop = shutdown.subscribe();
        tokio::spawn(async move {
            let _guard = guard;
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
                    serve_connection(tls, node, sni, opts.body_timeouts, stop).await;
                }
                None => serve_connection(sock, node, None, opts.body_timeouts, stop).await,
            }
        });
    }
    drop(listener);
    // Requests in flight finish; a search still running after the drain period is cancelled
    // (it answers with its cancellation), and after as long again the stop goes ahead.
    let drain = opts.drain_timeout.unwrap_or(DRAIN_TIMEOUT);
    if !wait_closed(&conns, drain).await {
        let n = node.cancel_tasks();
        tracing::info!(cancelled = n, "requests still running after the drain period: cancelling their tasks");
        if !wait_closed(&conns, drain).await {
            tracing::warn!(open = conns.open.load(std::sync::atomic::Ordering::SeqCst), "connections still open at shutdown");
        }
    }
    Ok(())
}

/// Waits until every connection has closed, at most `timeout`; whether they all did.
async fn wait_closed(conns: &Conns, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let idle = conns.idle.notified();
        tokio::pin!(idle);
        idle.as_mut().enable();
        if conns.open.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            return true;
        }
        if tokio::time::timeout_at(deadline, idle).await.is_err() {
            return conns.open.load(std::sync::atomic::Ordering::SeqCst) == 0;
        }
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

async fn serve_connection<S>(io: S, node: Arc<Node>, sni: Option<String>, body_timeouts: Option<(Duration, Duration)>, mut stop: tokio::sync::watch::Receiver<bool>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let sni = sni.map(Arc::<str>::from);
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let node = node.clone();
        let sni = sni.clone();
        let timeouts = body_timeouts.unwrap_or((BODY_IDLE_TIMEOUT, BODY_TIMEOUT));
        async move { respond(node, req, sni.as_deref(), timeouts).await }
    });
    let mut builder = hyper::server::conn::http1::Builder::new();
    // A client gets 30 s to send each request's headers (slowloris protection).
    builder
        .keep_alive(true)
        .pipeline_flush(true)
        .max_buf_size(1 << 20)
        // Neither Elasticsearch nor OpenSearch (Netty) sends a `Date` header; hyper would add one
        // to every response, so clients and differential checks would see an extra header.
        .auto_date_header(false)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30));
    let conn = builder.serve_connection(TokioIo::new(crate::http_head::FirstHead::new(io)), svc);
    tokio::pin!(conn);
    let stopped = *stop.borrow();
    let r = if stopped {
        conn.as_mut().graceful_shutdown();
        conn.await
    } else {
        tokio::select! {
            r = conn.as_mut() => r,
            _ = stop.changed() => {
                // Finish the request in flight, then close.
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        }
    };
    if let Err(e) = r {
        tracing::trace!(error = %e, "connection closed");
    }
}

fn head_of<B>(req: &Request<B>) -> EsRequest {
    // An absolute-form target (`GET http://host/ HTTP/1.1`) is routed as written, as Netty
    // hands it to Elasticsearch: no handler matches it.
    let uri = match req.uri().authority() {
        Some(_) => req.uri().to_string(),
        None => req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into()),
    };
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
    OverBudget(Rejected),
}

/// The service's error: hyper closes the connection without answering (Netty's behaviour when
/// a request body can't be decoded).
#[derive(Debug)]
pub struct DropConnection(String);

/// A request body [`respond`] can read: hyper's own, or another server's (axum's, on a fleet).
pub trait RequestBody: hyper::body::Body<Data = Bytes, Error: Into<Box<dyn std::error::Error + Send + Sync>>> + Unpin + Send {}

impl<B> RequestBody for B where B: hyper::body::Body<Data = Bytes, Error: Into<Box<dyn std::error::Error + Send + Sync>>> + Unpin + Send {}

impl std::fmt::Display for DropConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DropConnection {}

/// Elasticsearch's answer to a body over `http.max_content_length`: an empty
/// `413 Request Entity Too Large` (the old reason phrase), then the connection closes.
fn too_large() -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
    r.extensions_mut().insert(hyper::ext::ReasonPhrase::from_static(b"Request Entity Too Large"));
    r.headers_mut().insert(hyper::header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    r.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    r
}

fn closing(mut r: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
    r.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    r
}

/// Netty's `HttpObjectAggregator` answer to an `Expect` it can't meet: an empty
/// `417 Expectation Failed`, then the connection closes.
fn expectation_failed() -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = StatusCode::EXPECTATION_FAILED;
    r.headers_mut().insert(hyper::header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    r.headers_mut().insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
    r
}

/// Netty's limits on the request head (see the module docs), measured as the client sent it
/// (header lines as `name: value`).
fn head_too_long<B>(req: &Request<B>) -> Option<EsError> {
    let target = match req.uri().authority() {
        Some(_) => req.uri().to_string().len(),
        None => req.uri().path_and_query().map_or(1, |p| p.as_str().len()),
    };
    let version = 8; // HTTP/1.x
    if req.method().as_str().len() + 1 + target + 1 + version > MAX_INITIAL_LINE {
        return Some(EsError::new(400, "too_long_http_line_exception", format!("An HTTP line is larger than {MAX_INITIAL_LINE} bytes.")));
    }
    let headers: usize = req.headers().iter().map(|(k, v)| k.as_str().len() + 2 + v.len()).sum();
    if headers > MAX_HEADER_SIZE {
        return Some(EsError::new(400, "too_long_http_header_exception", format!("HTTP header is larger than {MAX_HEADER_SIZE} bytes.")));
    }
    None
}

/// Reads the whole body, at most `max` bytes, within the body timeouts. Without a declared
/// length every frame is reserved from the byte budget before it is kept (`reserve`).
async fn read_body<B: RequestBody>(
    body: B,
    max: usize,
    (idle, total): (Duration, Duration),
    declared: Option<u64>,
    reserve: &mut (dyn FnMut(usize) -> Result<(), Rejected> + Send),
) -> Result<Bytes, BodyError> {
    let mut body = Limited::new(body, max);
    let deadline = tokio::time::Instant::now() + total;
    // Never sized from the peer's word alone: a declared length only seeds the first step.
    let mut out = bytes::BytesMut::with_capacity(declared.map_or(0, |n| (n as usize).min(INLINE_CODEC_BYTES)));
    loop {
        let next = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + idle), body.frame()).await;
        match next {
            Err(_) => return Err(BodyError::TimedOut),
            Ok(None) => return Ok(out.freeze()),
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    if declared.is_none() {
                        reserve(data.len()).map_err(BodyError::OverBudget)?;
                    }
                    if out.is_empty() && declared.is_none() {
                        // Often the whole body comes in one frame: keep it without a copy.
                        out = bytes::BytesMut::from(data);
                    } else {
                        out.extend_from_slice(&data);
                    }
                }
            }
            Ok(Some(Err(e))) => {
                return Err(if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() { BodyError::TooLong } else { BodyError::Broken });
            }
        }
    }
}

/// Serves one request as this node's REST port does, for a request another server accepted
/// (a fleet node's front, which runs it on its cluster's owner): the same limits, decoding and
/// answers. `Err`: close the connection without answering.
pub async fn serve_request<B: RequestBody>(node: Arc<Node>, req: Request<B>) -> Result<Response<Full<Bytes>>, DropConnection> {
    respond(node, req, None, (BODY_IDLE_TIMEOUT, BODY_TIMEOUT)).await
}

async fn respond<B: RequestBody>(node: Arc<Node>, req: Request<B>, sni: Option<&str>, timeouts: (Duration, Duration)) -> Result<Response<Full<Bytes>>, DropConnection> {
    let mut es = head_of(&req);
    let gzip_ok = accepts_gzip(es.header("accept-encoding")) && es.method != "HEAD";
    if let Some(e) = head_too_long(&req) {
        return Ok(closing(to_hyper(search_es::rest::error_response(&e, &es, search_es::Dialect::Elasticsearch), gzip_ok).await));
    }
    // Only `100-continue` can be met (hyper sends the `100 Continue` when the body is read).
    if es.header("expect").is_some_and(|v| !v.trim().eq_ignore_ascii_case("100-continue")) {
        return Ok(expectation_failed());
    }
    let mut lease = match node.enter(&es, sni).await {
        Ok(l) => l,
        Err(r) => return Ok(to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok).await),
    };
    let max = lease.max_body_bytes(&node);
    let declared = es.header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|len| len > max as u64) {
        return Ok(too_large());
    }
    if let Some(len) = declared
        && let Err(r) = lease.reserve_body(&es, len as usize)
    {
        return Ok(to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok).await);
    }
    let read = {
        let es = &es;
        let lease = &mut lease;
        read_body(req.into_body(), max, timeouts, declared, &mut |n| lease.reserve_more(es, n)).await
    };
    let body = match read {
        Ok(b) => b,
        Err(BodyError::TooLong) => return Ok(too_large()),
        Err(BodyError::OverBudget(r)) => return Ok(closing(to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok).await)),
        Err(BodyError::TimedOut) => {
            let e = EsError::new(408, "timeout_exception", "timed out reading the request body");
            return Ok(closing(to_hyper(search_es::rest::error_response(&e, &es, lease.loaded.spec().dialect), gzip_ok).await));
        }
        // The client went away mid-body: nobody to answer.
        Err(BodyError::Broken) => return Err(DropConnection("the client went away mid-body".into())),
    };
    let body = match es.header("content-encoding").map(|v| v.trim().to_ascii_lowercase()) {
        Some(enc) if enc == "gzip" || enc == "x-gzip" || enc == "deflate" => {
            let gz = enc != "deflate";
            match inflate_reserved(&mut lease, body, gz, max).await {
                Ok(b) => b,
                Err(Inflate::TooLong) => return Ok(too_large()),
                Err(Inflate::OverBudget { used, bytes }) => {
                    let r = lease.over_budget(&es, used, bytes);
                    return Ok(closing(to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok).await));
                }
                Err(Inflate::Corrupt(msg)) => return Err(DropConnection(msg)),
            }
        }
        _ => body,
    };
    es.body = body;
    if let Err(r) = lease.acquire(&es).await {
        return Ok(to_hyper(search_es::rest::error_response(&r.error, &es, r.dialect), gzip_ok).await);
    }
    let resp = lease.handle(es).await;
    drop(lease);
    Ok(to_hyper(resp, gzip_ok).await)
}

#[derive(Debug)]
enum Inflate {
    TooLong,
    /// The inflated body passed the byte budget (`used` in flight, `bytes` this request's).
    OverBudget {
        used: usize,
        bytes: usize,
    },
    Corrupt(String),
}

/// Inflates a request body, reserving its output from the cluster's byte budget as it grows
/// and giving the compressed bytes back afterwards; on the blocking pool when large.
async fn inflate_reserved(lease: &mut crate::node::Lease, body: Bytes, gz: bool, max: usize) -> Result<Bytes, Inflate> {
    let (mut res, budget) = lease.take_reservation();
    let compressed = body.len();
    let work = move || {
        let mut grow = |n: usize| budget.grow(&mut res, n).map_err(|used| (used, n));
        let r = inflate(&body, gz, max, &mut grow);
        drop(body);
        // The compressed copy is gone: its share of the reservation goes back.
        res.shrink(compressed);
        (r, res)
    };
    let (r, res) = if compressed > INLINE_CODEC_BYTES {
        tokio::task::spawn_blocking(work).await.map_err(|e| Inflate::Corrupt(format!("inflating the request body failed: {e}")))?
    } else {
        work()
    };
    lease.put_reservation(res);
    r
}

/// Decompresses a request body, refusing to expand past `max` bytes. `grow` reserves each
/// step of output (and fails with the bytes in use). A truncated stream yields what inflated
/// so far, as Netty's decoder passes it on; anything else that isn't valid is `Corrupt`.
fn inflate(body: &[u8], gz: bool, max: usize, grow: &mut dyn FnMut(usize) -> Result<(), (usize, usize)>) -> Result<Bytes, Inflate> {
    let mut reader: Box<dyn Read + '_> = if gz {
        Box::new(flate2::read::GzDecoder::new(body))
    } else if body.len() >= 2 && body[0] & 0x0f == 8 && (u16::from(body[0]) << 8 | u16::from(body[1])) % 31 == 0 {
        Box::new(flate2::read::ZlibDecoder::new(body))
    } else {
        // Raw deflate, as Netty's ZLIB_OR_NONE accepts.
        Box::new(flate2::read::DeflateDecoder::new(body))
    };
    let mut out: Vec<u8> = Vec::new();
    let mut reserved = 0usize;
    loop {
        // Reserve each step of output before inflating into it.
        let step = INFLATE_STEP.min(max + 1 - out.len());
        grow(step).map_err(|(used, n)| Inflate::OverBudget { used, bytes: reserved + n })?;
        reserved += step;
        match (&mut reader).take(step as u64).read_to_end(&mut out) {
            Ok(n) if n < step => break,
            Ok(_) => {}
            // Truncated: what inflated so far (already in `out`) is the body.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Inflate::Corrupt(format!("failed to decompress the request body: {e}"))),
        }
        if out.len() > max {
            return Err(Inflate::TooLong);
        }
    }
    if out.len() > max {
        return Err(Inflate::TooLong);
    }
    Ok(Bytes::from(out))
}

/// Elasticsearch's default `http.compression_level` is 3, which in stock zlib (Netty's) is
/// `deflate_fast`. zlib-rs follows zlib-ng, whose level 3 is the slower `deflate_medium`; its
/// level 2 is `deflate_fast` and matches stock zlib's level 3 in ratio and speed (geonames pages:
/// 4.96 vs 5.01, 3.2 vs 3.3 ms per 400 KB; zlib-rs level 3 is 5.25 at 4.3 ms).
const GZIP_LEVEL: u32 = 2;
/// Responses at least twice this size are deflated in chunks of it on the search pool.
const GZIP_CHUNK: usize = 128 * 1024;
/// Most threads one response's chunks spread over.
const GZIP_MAX_THREADS: usize = 4;

fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    if body.len() >= 2 * GZIP_CHUNK {
        let threads = search_index::search::pool::cores().min(GZIP_MAX_THREADS);
        if threads > 1 && !search_index::search::pool::in_worker() {
            return gzip_chunked(body, GZIP_CHUNK, threads - 1);
        }
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 3 + 32), flate2::Compression::new(GZIP_LEVEL));
    enc.write_all(body).ok()?;
    enc.finish().ok()
}

/// One gzip member whose deflate stream is the concatenation of independently compressed
/// chunks (pigz's independent mode): each chunk but the last ends on a sync flush, which closes
/// its blocks on a byte boundary without `BFINAL`, so the next chunk's fresh compressor can
/// start right after it. Back-references never cross a chunk, which costs about 1% of ratio at
/// 128 KiB chunks and lets a large page compress on several threads.
fn gzip_chunked(body: &[u8], chunk: usize, helpers: usize) -> Option<Vec<u8>> {
    let parts: Vec<&[u8]> = body.chunks(chunk).collect();
    type Part = parking_lot::Mutex<Option<(Vec<u8>, flate2::Crc)>>;
    let outs: Vec<Part> = (0..parts.len()).map(|_| parking_lot::Mutex::new(None)).collect();
    search_index::search::pool::for_each_index(parts.len(), helpers.min(parts.len() - 1), &|i| {
        let deflated = deflate_part(parts[i], i + 1 == parts.len());
        let mut crc = flate2::Crc::new();
        crc.update(parts[i]);
        *outs[i].lock() = deflated.map(|d| (d, crc));
    });
    let mut out = Vec::with_capacity(body.len() / 3 + 32);
    // ID1 ID2 CM=deflate FLG=0 MTIME=0 XFL=0 OS=unknown, as `GzEncoder` writes it.
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    let mut crc = flate2::Crc::new();
    for o in outs {
        let (d, c) = o.into_inner()?;
        out.extend_from_slice(&d);
        crc.combine(&c);
    }
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    Some(out)
}

/// Raw deflate of one chunk: ends with `BFINAL` when `last`, else on a sync flush.
fn deflate_part(data: &[u8], last: bool) -> Option<Vec<u8>> {
    use flate2::{Compress, Compression, FlushCompress, Status};
    let mut c = Compress::new(Compression::new(GZIP_LEVEL), false);
    let mut out = Vec::with_capacity(data.len() / 3 + 64);
    let flush = if last { FlushCompress::Finish } else { FlushCompress::Sync };
    loop {
        let st = c.compress_vec(&data[c.total_in() as usize..], &mut out, flush).ok()?;
        let consumed = c.total_in() as usize == data.len();
        // A flush is complete once it returns with output space left (zlib's `avail_out != 0`).
        match st {
            Status::StreamEnd => return Some(out),
            _ if !last && consumed && out.len() < out.capacity() => return Some(out),
            _ => out.reserve(out.capacity().max(4096)),
        }
    }
}

/// The HTTP response; `accepts_gzip` (never for `HEAD`, whose length is the identity body's,
/// as Elasticsearch answers) compresses the body, on the blocking pool when it is large.
async fn to_hyper(resp: EsResponse, accepts_gzip: bool) -> Response<Full<Bytes>> {
    let EsResponse { status, headers, body, head } = resp;
    let (body, gzipped) = if accepts_gzip && !head && !body.is_empty() {
        let z = if body.len() > INLINE_CODEC_BYTES {
            let b = body.clone();
            tokio::task::spawn_blocking(move || gzip(&b)).await.ok().flatten()
        } else {
            gzip(&body)
        };
        match z {
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

    fn unlimited(_: usize) -> Result<(), (usize, usize)> {
        Ok(())
    }

    fn deflate(body: &[u8], zlib: bool) -> Vec<u8> {
        if zlib {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(body).unwrap();
            e.finish().unwrap()
        } else {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(body).unwrap();
            e.finish().unwrap()
        }
    }

    #[test]
    fn gzip_and_deflate_bodies() {
        let z = gzip(b"{}").unwrap();
        assert_eq!(&inflate(&z, true, 100, &mut unlimited).unwrap()[..], b"{}");
        // `deflate` is zlib-wrapped or raw (Netty's ZLIB_OR_NONE).
        assert_eq!(&inflate(&deflate(b"{\"a\":1}", true), false, 100, &mut unlimited).unwrap()[..], b"{\"a\":1}");
        assert_eq!(&inflate(&deflate(b"{\"a\":1}", false), false, 100, &mut unlimited).unwrap()[..], b"{\"a\":1}");
        // Trailing bytes after a complete gzip member are ignored, as Elasticsearch does.
        let mut trailing = gzip(b"{}").unwrap();
        trailing.extend_from_slice(b"garbage");
        assert_eq!(&inflate(&trailing, true, 100, &mut unlimited).unwrap()[..], b"{}");
    }

    #[test]
    fn chunked_gzip_is_one_valid_member() {
        // JSON-ish text (compressible) and xorshift noise (incompressible: the output outgrows
        // its first reservation), at sizes around chunk boundaries.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut noise = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        };
        let row = b"{\"name\":\"geo\",\"population\":12345,\"tz\":\"Europe/Berlin\"},";
        let text: Vec<u8> = (0..700_000).map(|i| row[i % row.len()]).collect();
        let rand: Vec<u8> = (0..700_000).map(|_| noise()).collect();
        for data in [&text, &rand] {
            for (len, chunk) in [(1, 1000), (999, 1000), (1000, 1000), (1001, 1000), (65_537, 4096), (262_144, GZIP_CHUNK), (262_145, GZIP_CHUNK), (700_000, GZIP_CHUNK)] {
                for helpers in [0, 3] {
                    let z = gzip_chunked(&data[..len], chunk, helpers).unwrap();
                    let mut d = flate2::read::GzDecoder::new(&z[..]);
                    let mut out = Vec::new();
                    // GzDecoder checks the CRC and length trailer.
                    d.read_to_end(&mut out).unwrap();
                    assert!(out == data[..len], "len {len} chunk {chunk}");
                    assert_eq!(&inflate(&z, true, len, &mut unlimited).unwrap()[..], &data[..len]);
                }
            }
        }
        // Large bodies take the chunked path through `gzip` and stay close to one stream's ratio.
        let z = gzip(&text).unwrap();
        let mut serial = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        serial.write_all(&text).unwrap();
        assert!(z.len() <= serial.finish().unwrap().len() * 11 / 10 + 64);
        assert_eq!(&inflate(&z, true, text.len(), &mut unlimited).unwrap()[..], &text[..]);
    }

    #[test]
    fn inflate_is_bounded() {
        let big = vec![b'a'; 300_000];
        let z = gzip(&big).unwrap();
        assert_eq!(inflate(&z, true, 300_000, &mut unlimited).unwrap().len(), 300_000);
        assert!(matches!(inflate(&z, true, 299_999, &mut unlimited), Err(Inflate::TooLong)));
        assert!(matches!(inflate(&z, true, 100, &mut unlimited), Err(Inflate::TooLong)));
        // Output is reserved step by step, never more than a step ahead of what inflated.
        let mut steps = Vec::new();
        let mut track = |n: usize| {
            steps.push(n);
            Ok(())
        };
        inflate(&z, true, 1 << 20, &mut track).unwrap();
        assert_eq!(steps.iter().sum::<usize>(), 5 * INFLATE_STEP);
        // A refusal stops inflating at once.
        let mut budget = 100_000usize;
        let mut limited = |n: usize| {
            if n > budget {
                return Err((7, n));
            }
            budget -= n;
            Ok(())
        };
        match inflate(&z, true, 1 << 20, &mut limited) {
            Err(Inflate::OverBudget { used: 7, bytes }) => assert_eq!(bytes, 2 * INFLATE_STEP),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn broken_compressed_bodies() {
        let body = br#"{"query":{"match_all":{}}}"#;
        let z = gzip(body).unwrap();
        // Truncated: what inflated so far, as Netty passes it on (the header alone: nothing).
        assert_eq!(&inflate(&z[..10], true, 100, &mut unlimited).unwrap()[..], b"");
        assert_eq!(&inflate(&z[..3], true, 100, &mut unlimited).unwrap()[..], b"");
        let part = inflate(&z[..z.len() - 8], true, 100, &mut unlimited).unwrap();
        assert!(body.starts_with(&part), "{part:?}");
        // Not compressed at all, or a damaged checksum: corrupt (the connection is dropped).
        assert!(matches!(inflate(b"not gzip at all", true, 100, &mut unlimited), Err(Inflate::Corrupt(_))));
        let mut bad = z.clone();
        let n = bad.len();
        bad[n - 5] ^= 0xff;
        assert!(matches!(inflate(&bad, true, 100, &mut unlimited), Err(Inflate::Corrupt(_))));
        assert!(matches!(inflate(b"not gzip at all", false, 100, &mut unlimited), Err(Inflate::Corrupt(_))));
    }
}
