//! A TLS-intercepting egress step for code that holds roll secrets' placeholders instead of keys
//! (docs/secrets.md, "Using a key without seeing it: the proxy's contract"). Product-neutral:
//! roll previews' egress proxy hands it connections (docs/previews.md, "Keys a preview never
//! holds"), and roll apps' closed environments can do the same. What decides each swap is an
//! [`Interception`] (rolld's, over roll secrets).
//!
//! A fork's preview gets `rlph_` placeholders where the values of keys bound to hosts would be.
//! Its egress proxy (`egress.js`) still decides first whether a host may be reached at all
//! (`egress_hosts`); for a host the preview's interception CA covers, it hands the client's
//! connection here instead of tunnelling it, and [`serve`]:
//!
//! 1. checks the CA is still live (on every connection, never cached);
//! 2. terminates TLS with a certificate for exactly that host, signed by the CA the preview's
//!    containers trust (and nothing else does);
//! 3. runs each HTTP/1.1 request's headers and query through the filter, which swaps each
//!    placeholder for its value, or refuses: a refused request is answered 403 here and nothing of
//!    it is sent;
//! 4. sends the rest on over its own TLS connection to the same host, verified against real roots
//!    (never the interception CA), so the preview can't make it trust a server of its own.
//!
//! Nothing here holds a value or a placeholder past one request, and nothing logs one. Whoever
//! implements [`Interception`] (rolld, over roll secrets) decides every swap. No upgrades
//! (WebSockets) through an intercepted host: a bound key goes in a request's headers or query.
//! HTTP/1.1 only (ALPN `http/1.1`): a client that offers nothing but h2 (gRPC) can't connect.
//!
//! A preview's code chooses how often it connects, so each preview is bounded: at most
//! [`MAX_CONNECTIONS`] at once (more are closed at once), a TLS handshake within
//! [`HANDSHAKE`], a request's headers within [`HEADERS`], and each host's leaf made once for the
//! CA's life (only `live` is asked every time).

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls;

/// One preview's interception, as rolld gives it (roll secrets' `InterceptCa` and its filter).
/// Its calls may block (a database): [`serve`] makes them off the async workers.
pub trait Interception: Send + Sync {
    /// Whether the CA still stands (the consumer not ended, not expired), asked on every connection.
    fn live(&self) -> anyhow::Result<bool>;
    /// Whether TLS to `host` is terminated here: a host one of the preview's keys is bound to.
    fn covers(&self, host: &str) -> bool;
    /// A certificate for exactly `host`, signed by the CA.
    fn leaf(&self, host: &str) -> anyhow::Result<Leaf>;
    /// Swap every placeholder in the request's headers and query, or none: how many were swapped
    /// and what its response is checked against, or why the request mustn't go (never holding a
    /// value or a placeholder).
    fn filter(&self, req: Outgoing<'_>) -> Result<Filtered, String>;
}

/// A request [`Interception::filter`] let go.
#[derive(Default)]
pub struct Filtered {
    pub swapped: usize,
    /// With a key swapped in: its response's check (roll secrets' scrub).
    pub scrub: Option<Box<dyn ResponseScrub>>,
}

/// The check of a response to a request a key was swapped into (roll secrets' scrub, docs/secrets.md
/// "the proxy's contract"): its headers, then its body as it arrives, for the values swapped in
/// (in any form it looks for). It holds them itself; [`serve`] never sees one.
pub trait ResponseScrub: Send + Sync {
    /// The response's headers, or its trailers: why not, when they echo a value.
    fn headers(&mut self, headers: &hyper::HeaderMap) -> Result<(), String>;
    /// The body's next bytes (a value split between two is found): why not, when they echo one.
    fn chunk(&mut self, bytes: &[u8]) -> Result<(), String>;
    /// After an echo: report it, which ends the placeholder. Blocking.
    fn echoed(&self);
}

/// A leaf certificate and its key, PEM.
#[derive(Clone)]
pub struct Leaf {
    pub cert_pem: String,
    /// The leaf's private key: wiped from memory when the last copy goes (the [`Leaves`] cache
    /// keeps one per host for the CA's life).
    pub key_pem: zeroize::Zeroizing<String>,
}

/// A request as the filter sees it: `host` is the one connected to (the name TLS is verified for
/// both ways), never the request's own `Host` header; `query` is raw, without `?`.
pub struct Outgoing<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub headers: &'a mut Vec<(String, String)>,
    pub query: Option<&'a mut String>,
}

/// How [`serve`] reaches the host upstream (rolld: resolved, public addresses only).
pub type Dial = Arc<dyn Fn(String, u16) -> futures::future::BoxFuture<'static, std::io::Result<tokio::net::TcpStream>> + Send + Sync>;

/// What one connection did, for the preview's activity: counts and why requests were refused.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Served {
    pub requests: usize,
    pub swapped: usize,
    pub blocked: Vec<String>,
}

/// TLS to the host upstream, verified against `roots`, HTTP/1.1.
pub fn upstream_config(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut c = rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().expect("ring's TLS versions").with_root_certificates(roots).with_no_client_auth();
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(c)
}

/// [`upstream_config`] with the system's roots.
pub fn system_upstream() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    upstream_config(roots)
}

/// The system's roots plus the one at `root` (PEM): local development's TLS echo host
/// (`ROLL_PREVIEWS_INTERCEPT_ALLOW_PRIVATE`), for intercepted connections only.
pub fn with_root(root: &std::path::Path) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    for c in rustls_pemfile::certs(&mut std::fs::read(root)?.as_slice()) {
        roots.add(c?)?;
    }
    Ok(upstream_config(roots))
}

/// A [`Dial`] to any address the name resolves to: local development only (with [`with_root`]).
pub fn any_dial() -> Dial {
    Arc::new(|host: String, port: u16| Box::pin(async move { tokio::net::TcpStream::connect((host.as_str(), port)).await }))
}

/// [`Dial`] as rolld dials: the host resolved here, and only a public address connected to (roll-net's
/// `private_ip`), so a bound name pointed at the host, the fleet or the cloud's metadata reaches
/// nothing.
pub fn public_dial() -> Dial {
    Arc::new(|host: String, port: u16| {
        Box::pin(async move {
            let addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
            let mut last = std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("{host} has no public address"));
            for a in addrs.filter(|a| !roll_net::private_ip(a.ip())) {
                match tokio::net::TcpStream::connect(a).await {
                    Ok(s) => return Ok(s),
                    Err(e) => last = e,
                }
            }
            Err(last)
        })
    })
}

/// The longest handoff line `egress.js` sends (`host:port\n`).
const HANDOFF_MAX: usize = 300;

/// What a preview's egress proxy hands over on its socket: the `CONNECT` target, as `host:port\n`,
/// then the client's bytes. Bounded; anything else is refused.
pub async fn read_handoff<S: AsyncRead + Unpin>(s: &mut S) -> anyhow::Result<(String, u16)> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::with_capacity(64);
    let mut b = [0u8; 1];
    loop {
        anyhow::ensure!(s.read(&mut b).await? == 1, "the handoff ended early");
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        anyhow::ensure!(line.len() <= HANDOFF_MAX, "a handoff line too long");
    }
    let line = String::from_utf8(line)?;
    let (host, port) = line.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("a handoff without a port"))?;
    anyhow::ensure!(!host.is_empty() && host.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-'), "a handoff's host isn't a name");
    Ok((host.to_string(), port.parse()?))
}

/// Serve one preview's handoffs on `listener` (its socket, mounted into its egress container),
/// each as [`serve`], until the listener ends. `done` hears each connection's [`Served`] (for the
/// activity), or why it was refused.
pub async fn listen(listener: tokio::net::UnixListener, i: Arc<dyn Interception>, upstream: Arc<rustls::ClientConfig>, dial: Dial, done: Arc<dyn Fn(String, anyhow::Result<Served>) + Send + Sync>) {
    let i: Arc<dyn Interception> = Arc::new(Leaves { inner: i, made: parking_lot::Mutex::new(std::collections::HashMap::new()) });
    let room = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    while let Ok((mut s, _)) = listener.accept().await {
        // Over the bound: closed at once, nothing done for it.
        let Ok(permit) = room.clone().try_acquire_owned() else { continue };
        let (i, upstream, dial, done) = (i.clone(), upstream.clone(), dial.clone(), done.clone());
        tokio::spawn(async move {
            let _permit = permit;
            match read_handoff(&mut s).await {
                Ok((host, port)) => {
                    let out = serve(s, &host, port, i, upstream, dial).await;
                    done(host, out);
                }
                Err(e) => done(String::new(), Err(e)),
            }
        });
    }
}

/// The most of one connection's refusals kept for the activity.
const MAX_BLOCKED: usize = 20;

/// The most connections one preview has intercepted at once.
pub const MAX_CONNECTIONS: usize = 16;
/// How long a client has for its TLS handshake.
pub const HANDSHAKE: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a client has for a request's headers.
pub const HEADERS: std::time::Duration = std::time::Duration::from_secs(30);

/// Each host's leaf made once (key generation and a signature), for the CA's life: a listener
/// serves one CA, and a new CA comes with a new listener.
struct Leaves {
    inner: Arc<dyn Interception>,
    made: parking_lot::Mutex<std::collections::HashMap<String, Leaf>>,
}

impl Interception for Leaves {
    fn live(&self) -> anyhow::Result<bool> {
        self.inner.live()
    }
    fn covers(&self, host: &str) -> bool {
        self.inner.covers(host)
    }
    fn leaf(&self, host: &str) -> anyhow::Result<Leaf> {
        if let Some(l) = self.made.lock().get(host) {
            return Ok(l.clone());
        }
        let l = self.inner.leaf(host)?;
        self.made.lock().insert(host.to_string(), l.clone());
        Ok(l)
    }
    fn filter(&self, req: Outgoing<'_>) -> Result<Filtered, String> {
        self.inner.filter(req)
    }
}

/// The connection to the host, made again when the host closed it (an idle one).
struct Upstream {
    cfg: Arc<rustls::ClientConfig>,
    dial: Dial,
    host: String,
    port: u16,
    sender: tokio::sync::Mutex<Option<hyper::client::conn::http1::SendRequest<Incoming>>>,
}

impl Upstream {
    async fn connect(&self) -> anyhow::Result<hyper::client::conn::http1::SendRequest<Incoming>> {
        let name = rustls::pki_types::ServerName::try_from(self.host.clone())?;
        let up = tokio_rustls::TlsConnector::from(self.cfg.clone()).connect(name, (self.dial)(self.host.clone(), self.port).await?).await?;
        let (sender, conn) = hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(up)).await?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }
}

/// Serve one connection from the preview to `host:port` (the `CONNECT` target its egress proxy
/// passed on), as the module doc says. Ends when either side closes.
pub async fn serve<S>(client: S, host: &str, port: u16, i: Arc<dyn Interception>, upstream: Arc<rustls::ClientConfig>, dial: Dial) -> anyhow::Result<Served>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    anyhow::ensure!(i.covers(&host), "{host} isn't a host this preview's keys are bound to");
    let (i2, h2) = (i.clone(), host.clone());
    let (live, leaf) = tokio::task::spawn_blocking(move || -> anyhow::Result<(bool, Leaf)> { Ok((i2.live()?, i2.leaf(&h2)?)) }).await??;
    anyhow::ensure!(live, "the preview's interception CA has ended");
    let tls = tokio::time::timeout(HANDSHAKE, tokio_rustls::TlsAcceptor::from(server_config(&leaf)?).accept(client)).await.map_err(|_| anyhow::anyhow!("the client's TLS handshake took too long"))??;
    let up = Upstream { cfg: upstream, dial, host: host.clone(), port, sender: tokio::sync::Mutex::new(None) };
    // The first connection at once: a host the roots don't vouch for gets nothing.
    let first = up.connect().await?;
    *up.sender.lock().await = Some(first);
    let sender = Arc::new(up);
    let served = Arc::new(parking_lot::Mutex::new(Served::default()));
    let authority = if port == 443 { host.clone() } else { format!("{host}:{port}") };
    let svc = {
        let served = served.clone();
        hyper::service::service_fn(move |req: Request<Incoming>| {
            let (i, host, authority, sender, served) = (i.clone(), host.clone(), authority.clone(), sender.clone(), served.clone());
            async move { Ok::<_, Infallible>(one(req, i, host, authority, sender, served).await) }
        })
    };
    // A client that just goes away ends the connection with an error: what it sent is counted.
    let _ = hyper::server::conn::http1::Builder::new().timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(HEADERS).serve_connection(TokioIo::new(tls), svc).await;
    let out = served.lock().clone();
    Ok(out)
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = BoxBody<Bytes, BoxError>;

/// A response body checked frame by frame before each goes on to the client: data through the
/// scrub, trailers as headers. An echo ends it with an error, so the client's connection is cut
/// (its status went already) and nothing of the value reaches it; the echo is reported.
struct Scrubbed {
    inner: BoxBody<Bytes, hyper::Error>,
    scrub: Option<Box<dyn ResponseScrub>>,
    served: Arc<parking_lot::Mutex<Served>>,
}

impl hyper::body::Body for Scrubbed {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, BoxError>>> {
        use std::task::Poll;
        let Self { inner, scrub, served } = &mut *self;
        let Some(check) = scrub.as_mut() else { return Poll::Ready(Some(Err("its response echoed a key".into()))) };
        let frame = match std::pin::Pin::new(inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(f))) => f,
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e.into()))),
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => return Poll::Pending,
        };
        let checked = match (frame.data_ref(), frame.trailers_ref()) {
            (Some(d), _) => check.chunk(d),
            (None, Some(t)) => check.headers(t),
            (None, None) => Ok(()),
        };
        match checked {
            Ok(()) => Poll::Ready(Some(Ok(frame))),
            Err(why) => {
                let taken = scrub.take().expect("checked above");
                echoed(served, taken, &why);
                Poll::Ready(Some(Err(why.into())))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.scrub.is_some() && self.inner.is_end_stream()
    }
}

/// An echo: counted with the refusals, and reported (off the async threads).
fn echoed(served: &parking_lot::Mutex<Served>, scrub: Box<dyn ResponseScrub>, why: &str) {
    {
        let mut s = served.lock();
        if s.blocked.len() < MAX_BLOCKED {
            s.blocked.push(format!("{why}: stopped, and its placeholder ended"));
        }
    }
    tokio::task::spawn_blocking(move || scrub.echoed());
}

fn said(status: StatusCode, why: &str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(format!("roll's egress didn't send this request: {why}\n"))).map_err(|never| match never {}).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

/// Headers that are this hop's, not the request's.
const HOP: &[&str] = &["connection", "proxy-connection", "keep-alive", "proxy-authorization", "te", "trailer", "upgrade"];

async fn one(req: Request<Incoming>, i: Arc<dyn Interception>, host: String, authority: String, up: Arc<Upstream>, served: Arc<parking_lot::Mutex<Served>>) -> Response<Body> {
    served.lock().requests += 1;
    let block = |why: String| {
        let mut s = served.lock();
        if s.blocked.len() < MAX_BLOCKED {
            s.blocked.push(why.clone());
        }
        said(StatusCode::FORBIDDEN, &why)
    };
    if req.headers().contains_key(hyper::header::UPGRADE) {
        return block("an upgrade (a WebSocket) to a host a key is bound to isn't intercepted".into());
    }
    let (mut parts, body) = req.into_parts();
    let mut headers = Vec::with_capacity(parts.headers.len());
    for (n, v) in &parts.headers {
        if HOP.contains(&n.as_str()) || n == hyper::header::HOST {
            continue;
        }
        let Ok(v) = v.to_str() else { return block(format!("the header {n} isn't text")) };
        headers.push((n.as_str().to_string(), v.to_string()));
    }
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(String::from);
    let filtered = tokio::task::spawn_blocking(move || {
        let (mut headers, mut query) = (headers, query);
        let n = i.filter(Outgoing { host: &host, path: &path, headers: &mut headers, query: query.as_mut() });
        (n, headers, query, path)
    })
    .await;
    let (headers, query, path, scrub) = match filtered {
        Ok((Ok(f), headers, query, path)) => {
            served.lock().swapped += f.swapped;
            (headers, query, path, f.scrub.filter(|_| f.swapped > 0))
        }
        Ok((Err(why), ..)) => return block(why),
        Err(e) => return said(StatusCode::INTERNAL_SERVER_ERROR, &format!("checking it failed: {e}")),
    };
    let mut map = hyper::HeaderMap::with_capacity(headers.len() + 1);
    for (n, v) in headers {
        match (hyper::header::HeaderName::from_bytes(n.as_bytes()), hyper::header::HeaderValue::from_str(&v)) {
            (Ok(n), Ok(v)) => {
                map.append(n, v);
            }
            _ => return said(StatusCode::INTERNAL_SERVER_ERROR, "a swapped header isn't valid"),
        }
    }
    // The host it's connected to, whatever the request said.
    map.insert(hyper::header::HOST, hyper::header::HeaderValue::from_str(&authority).expect("a checked host name"));
    let target = match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    let Ok(uri) = target.parse::<hyper::Uri>() else { return said(StatusCode::BAD_REQUEST, "its path and query") };
    parts.uri = uri;
    parts.headers = map;
    parts.version = hyper::Version::HTTP_11;
    let mut slot = up.sender.lock().await;
    let ready = match slot.as_mut() {
        Some(s) => s.ready().await.is_ok(),
        None => false,
    };
    // The host closed it (an idle connection it timed out): a new one, verified as the first.
    if !ready {
        match up.connect().await {
            Ok(s) => *slot = Some(s),
            Err(e) => return said(StatusCode::BAD_GATEWAY, &format!("the host: {e}")),
        }
    }
    let s = slot.as_mut().expect("connected above");
    match s.send_request(Request::from_parts(parts, body)).await {
        Ok(mut resp) => {
            // Its hop's own headers stay with it: the host closing its connection doesn't close the
            // client's.
            for h in HOP {
                resp.headers_mut().remove(*h);
            }
            let Some(mut scrub) = scrub else { return resp.map(|b| b.map_err(BoxError::from).boxed()) };
            // A key went in: what comes back is checked before it reaches the preview. Asked for
            // without encoding (the filter's `Accept-Encoding: identity`), so it can be read.
            if let Some(enc) = resp.headers().get(hyper::header::CONTENT_ENCODING).filter(|e| !e.as_bytes().eq_ignore_ascii_case(b"identity")) {
                let why = format!("the host answered a request a key was swapped into with a {} body, which can't be checked", String::from_utf8_lossy(enc.as_bytes()));
                let mut s = served.lock();
                if s.blocked.len() < MAX_BLOCKED {
                    s.blocked.push(why.clone());
                }
                return said(StatusCode::BAD_GATEWAY, &why);
            }
            if let Err(why) = scrub.headers(resp.headers()) {
                echoed(&served, scrub, &why);
                return said(StatusCode::BAD_GATEWAY, &format!("{why}: stopped"));
            }
            resp.map(|b| Scrubbed { inner: b.boxed(), scrub: Some(scrub), served: served.clone() }.boxed())
        }
        Err(e) => said(StatusCode::BAD_GATEWAY, &format!("the host: {e}")),
    }
}

fn server_config(leaf: &Leaf) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs = rustls_pemfile::certs(&mut leaf.cert_pem.as_bytes()).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut leaf.key_pem.as_bytes())?.ok_or_else(|| anyhow::anyhow!("a leaf without its key"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut c = rustls::ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(certs, key)?;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ca(name: &str) -> (rcgen::Issuer<'static, rcgen::KeyPair>, String) {
        let mut p = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        p.distinguished_name.push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        let pem = p.self_signed(&key).unwrap().pem();
        (rcgen::Issuer::new(p, key), pem)
    }

    fn leaf_of(issuer: &rcgen::Issuer<'static, rcgen::KeyPair>, host: &str) -> Leaf {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap().signed_by(&key, issuer).unwrap();
        Leaf { cert_pem: cert.pem(), key_pem: key.serialize_pem().into() }
    }

    fn roots(pem: &str) -> rustls::RootCertStore {
        let mut r = rustls::RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut pem.as_bytes()) {
            r.add(c.unwrap()).unwrap();
        }
        r
    }

    /// The preview's CA, covering one host; `rlph_good` swaps for `sk_real`, anything else `rlph_` refuses.
    struct Fake {
        issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
        live: bool,
        echoes: Arc<AtomicUsize>,
    }

    impl Interception for Fake {
        fn live(&self) -> anyhow::Result<bool> {
            Ok(self.live)
        }
        fn covers(&self, host: &str) -> bool {
            host == "api.example.test"
        }
        fn leaf(&self, host: &str) -> anyhow::Result<Leaf> {
            Ok(leaf_of(&self.issuer, host))
        }
        fn filter(&self, req: Outgoing<'_>) -> Result<Filtered, String> {
            let mut n = 0;
            for (_, v) in req.headers.iter_mut() {
                if v.contains("rlph_bad") {
                    return Err("not a live placeholder".into());
                }
                if v.contains("rlph_good") {
                    *v = v.replace("rlph_good", "sk_real");
                    n += 1;
                }
            }
            Ok(Filtered { swapped: n, scrub: Some(Box::new(FakeScrub(self.echoes.clone()))) })
        }
    }

    /// roll secrets' scrub as the fake has it: `sk_real` anywhere in a header or a chunk.
    struct FakeScrub(Arc<AtomicUsize>);

    impl ResponseScrub for FakeScrub {
        fn headers(&mut self, headers: &hyper::HeaderMap) -> Result<(), String> {
            if headers.values().any(|v| v.as_bytes().windows(7).any(|w| w == b"sk_real")) { Err("echoed STRIPE_KEY".into()) } else { Ok(()) }
        }
        fn chunk(&mut self, bytes: &[u8]) -> Result<(), String> {
            if bytes.windows(7).any(|w| w == b"sk_real") { Err("echoed STRIPE_KEY".into()) } else { Ok(()) }
        }
        fn echoed(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The host upstream: says whether it got the real key, and counts what reached it.
    async fn upstream(issuer: &rcgen::Issuer<'static, rcgen::KeyPair>, hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&leaf_of(issuer, "api.example.test")).unwrap());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, hits) = (acceptor.clone(), hits.clone());
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(tcp).await else { return };
                    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                        let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                        // It closes each connection after answering (as an idle timeout would).
                        async move {
                            let mut r = match req.uri().path() {
                                "/echo" => Response::new(Full::new(Bytes::from(format!("you sent {auth}")))),
                                "/echo-header" => {
                                    let mut r = Response::new(Full::new(Bytes::from("ok")));
                                    r.headers_mut().insert("x-echo", hyper::header::HeaderValue::from_str(&auth).unwrap());
                                    r
                                }
                                "/gzip" => {
                                    let mut r = Response::new(Full::new(Bytes::from_static(b"\x1f\x8b")));
                                    r.headers_mut().insert(hyper::header::CONTENT_ENCODING, hyper::header::HeaderValue::from_static("gzip"));
                                    r
                                }
                                _ => Response::new(Full::new(Bytes::from(format!("real={} host={host} q={}", auth == "Bearer sk_real", req.uri().query().unwrap_or_default())))),
                            };
                            r.headers_mut().insert(hyper::header::CONNECTION, hyper::header::HeaderValue::from_static("close"));
                            Ok::<_, Infallible>(r)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await;
                });
            }
        });
        addr
    }

    fn dial_to(addr: std::net::SocketAddr) -> Dial {
        Arc::new(move |_, _| Box::pin(tokio::net::TcpStream::connect(addr)))
    }

    /// The preview's side: TLS trusting only the interception CA, then requests.
    async fn client(ca_pem: &str, side: tokio::io::DuplexStream) -> hyper::client::conn::http1::SendRequest<Full<Bytes>> {
        let name = rustls::pki_types::ServerName::try_from("api.example.test").unwrap();
        let tls = tokio_rustls::TlsConnector::from(upstream_config(roots(ca_pem))).connect(name, side).await.unwrap();
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.unwrap();
        tokio::spawn(conn);
        sender
    }

    async fn get(s: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>, auth: &str) -> (u16, String) {
        let (status, body) = get_at(s, "/v1/charges?limit=1", auth).await;
        (status, body.expect("the whole body"))
    }

    /// The status, and the body unless the connection was cut while it came.
    async fn get_at(s: &mut hyper::client::conn::http1::SendRequest<Full<Bytes>>, path: &str, auth: &str) -> (u16, Option<String>) {
        let req = Request::get(path).header("host", "evil.test").header("authorization", auth).body(Full::new(Bytes::new())).unwrap();
        // Cut before its status line went: no status (0).
        let Ok(resp) = s.send_request(req).await else { return (0, None) };
        let status = resp.status().as_u16();
        (status, resp.into_body().collect().await.ok().map(|b| String::from_utf8(b.to_bytes().to_vec()).unwrap()))
    }

    #[tokio::test]
    async fn a_handoff_names_one_host_and_port_and_nothing_more() {
        let read = |b: &'static [u8]| async move { read_handoff(&mut &b[..]).await.map_err(|e| e.to_string()) };
        assert_eq!(read(b"api.example.test:443\nrest").await, Ok(("api.example.test".into(), 443)));
        for bad in [&b"api.example.test\n"[..], b"a b:443\n", b":443\n", b"api.example.test:443"] {
            assert!(read(bad).await.is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(read(Box::leak(vec![b'a'; 400].into_boxed_slice())).await.is_err(), "bounded");
    }

    #[tokio::test]
    async fn a_placeholder_becomes_its_key_only_on_the_way_to_its_host() {
        let (preview_ca, preview_pem) = ca("preview");
        let (real_ca, real_pem) = ca("a public root");
        let hits = Arc::new(AtomicUsize::new(0));
        let addr = upstream(&real_ca, hits.clone()).await;
        let i: Arc<dyn Interception> = Arc::new(Fake { issuer: preview_ca, live: true, echoes: Arc::new(AtomicUsize::new(0)) });

        // In through the preview's socket, as its egress proxy hands a connection over.
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::UnixListener::bind(dir.path().join("intercept.sock")).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = parking_lot::Mutex::new(Some(tx));
        let done: Arc<dyn Fn(String, anyhow::Result<Served>) + Send + Sync> = Arc::new(move |host, out| {
            if let Some(tx) = tx.lock().take() {
                let _ = tx.send((host, out.map_err(|e| e.to_string())));
            }
        });
        tokio::spawn(listen(listener, i.clone(), upstream_config(roots(&real_pem)), dial_to(addr), done));
        let mut sock = tokio::net::UnixStream::connect(dir.path().join("intercept.sock")).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut sock, b"api.example.test:443\n").await.unwrap();
        let (mine, theirs) = tokio::io::duplex(64 << 10);
        tokio::spawn(async move {
            let (mut a, mut b) = (theirs, sock);
            let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
        });
        let served = tokio::spawn(async move { rx.await.unwrap() });
        let mut s = client(&preview_pem, mine).await;
        // Swapped on the way out, the host's own name sent whatever the request said.
        assert_eq!(get(&mut s, "Bearer rlph_good").await, (200, "real=true host=api.example.test q=limit=1".into()));
        // A placeholder that doesn't swap: refused here, nothing reaches the host.
        let (status, body) = get(&mut s, "Bearer rlph_bad").await;
        assert_eq!((status, body.contains("not a live placeholder"), hits.load(Ordering::SeqCst)), (403, true, 1));
        // The host closed its side after answering: the next request gets a new connection to it.
        assert_eq!(get(&mut s, "Bearer rlph_good").await.0, 200);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        drop(s);
        let (host, out) = tokio::time::timeout(std::time::Duration::from_secs(10), served).await.expect("the connection ends").unwrap();
        assert_eq!((host.as_str(), out), ("api.example.test", Ok(Served { requests: 3, swapped: 2, blocked: vec!["not a live placeholder".into()] })));

        // A host the CA doesn't cover, or a CA that ended: nothing is terminated or dialled.
        for (host, live) in [("evil.test", true), ("api.example.test", false)] {
            let i: Arc<dyn Interception> = Arc::new(Fake { issuer: ca("preview").0, live, echoes: Arc::new(AtomicUsize::new(0)) });
            let (_mine, theirs) = tokio::io::duplex(1024);
            assert!(serve(theirs, host, 443, i, upstream_config(roots(&real_pem)), dial_to(addr)).await.is_err(), "{host} live={live}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Upstream is verified for real: a host the roots don't vouch for gets no request (the
        // preview can't point the proxy at a server of its own).
        let (mine, theirs) = tokio::io::duplex(64 << 10);
        let served = tokio::spawn(serve(theirs, "api.example.test", 443, i, upstream_config(roots(&preview_pem)), dial_to(addr)));
        let pem = preview_pem.clone();
        tokio::spawn(async move {
            let name = rustls::pki_types::ServerName::try_from("api.example.test").unwrap();
            let _tls = tokio_rustls::TlsConnector::from(upstream_config(roots(&pem))).connect(name, mine).await;
        });
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), served).await.expect("serve ends");
        assert!(out.unwrap().is_err());
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    /// A response that echoes a key swapped into its request never brings it back: in a header,
    /// a 502; in the body, the connection is cut before those bytes. Either way it's reported
    /// (which ends the placeholder). An encoded body, which can't be checked, is a 502 too.
    #[tokio::test]
    async fn a_response_that_echoes_a_swapped_key_is_stopped() {
        let (preview_ca, preview_pem) = ca("preview");
        let (real_ca, real_pem) = ca("a public root");
        let addr = upstream(&real_ca, Arc::new(AtomicUsize::new(0))).await;
        let echoes = Arc::new(AtomicUsize::new(0));
        let i: Arc<dyn Interception> = Arc::new(Fake { issuer: preview_ca, live: true, echoes: echoes.clone() });
        let mut outs = Vec::new();
        for (path, auth) in [("/echo-header", "Bearer rlph_good"), ("/gzip", "Bearer rlph_good"), ("/echo", "Bearer rlph_good"), ("/echo", "Bearer not-a-placeholder")] {
            let (mine, theirs) = tokio::io::duplex(64 << 10);
            let served = tokio::spawn(serve(theirs, "api.example.test", 443, i.clone(), upstream_config(roots(&real_pem)), dial_to(addr)));
            let mut s = client(&preview_pem, mine).await;
            let got = get_at(&mut s, path, auth).await;
            drop(s);
            let out = tokio::time::timeout(std::time::Duration::from_secs(10), served).await.expect("serve ends").unwrap().unwrap();
            outs.push((got, out.blocked.len()));
        }
        let [(header, h_blocked), (gzip, g_blocked), (body, b_blocked), (plain, p_blocked)] = outs.try_into().unwrap();
        assert_eq!((header.0, header.1.as_deref().is_some_and(|b| !b.contains("sk_real")), h_blocked), (502, true, 1), "a header echo: {header:?}");
        assert_eq!((gzip.0, g_blocked), (502, 1), "an encoded body: {gzip:?}");
        assert!(matches!(body, (0 | 200, None)) && b_blocked == 1, "a body echo: the connection cut before its bytes: {body:?}");
        assert_eq!((plain.0, plain.1.as_deref(), p_blocked), (200, Some("you sent Bearer not-a-placeholder"), 0), "nothing swapped, nothing checked");
        // Reported once per echo (off the async threads), never for the encoded body.
        for _ in 0..100 {
            if echoes.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(echoes.load(Ordering::SeqCst), 2);
    }
}
