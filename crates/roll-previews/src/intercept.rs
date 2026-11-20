//! Keys a preview's code never holds, on their way out (docs/previews.md, "Keys a preview never
//! holds"; roll secrets' docs/secrets.md, "Using a key without seeing it: the proxy's contract").
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
    /// Swap every placeholder in the request's headers and query, or none: how many were swapped,
    /// or why the request mustn't go (never holding a value or a placeholder).
    fn filter(&self, req: Outgoing<'_>) -> Result<usize, String>;
}

/// A leaf certificate and its key, PEM.
pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
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
    while let Ok((mut s, _)) = listener.accept().await {
        let (i, upstream, dial, done) = (i.clone(), upstream.clone(), dial.clone(), done.clone());
        tokio::spawn(async move {
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
    let tls = tokio_rustls::TlsAcceptor::from(server_config(&leaf)?).accept(client).await?;
    let name = rustls::pki_types::ServerName::try_from(host.clone())?;
    let up = tokio_rustls::TlsConnector::from(upstream).connect(name, dial(host.clone(), port).await?).await?;
    let (sender, conn) = hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(up)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let sender = Arc::new(tokio::sync::Mutex::new(sender));
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
    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tls), svc).await;
    let out = served.lock().clone();
    Ok(out)
}

type Body = BoxBody<Bytes, hyper::Error>;

fn said(status: StatusCode, why: &str) -> Response<Body> {
    let mut r = Response::new(Full::new(Bytes::from(format!("roll previews' egress didn't send this request: {why}\n"))).map_err(|never| match never {}).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

/// Headers that are this hop's, not the request's.
const HOP: &[&str] = &["connection", "proxy-connection", "keep-alive", "proxy-authorization", "te", "trailer", "upgrade"];

async fn one(req: Request<Incoming>, i: Arc<dyn Interception>, host: String, authority: String, sender: Arc<tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<Incoming>>>, served: Arc<parking_lot::Mutex<Served>>) -> Response<Body> {
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
    let (headers, query, path) = match filtered {
        Ok((Ok(n), headers, query, path)) => {
            served.lock().swapped += n;
            (headers, query, path)
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
    let mut s = sender.lock().await;
    if s.ready().await.is_err() {
        return said(StatusCode::BAD_GATEWAY, "the connection to the host closed");
    }
    match s.send_request(Request::from_parts(parts, body)).await {
        Ok(resp) => resp.map(BodyExt::boxed),
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
        Leaf { cert_pem: cert.pem(), key_pem: key.serialize_pem() }
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
        fn filter(&self, req: Outgoing<'_>) -> Result<usize, String> {
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
            Ok(n)
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
                        async move { Ok::<_, Infallible>(Response::new(Full::new(Bytes::from(format!("real={} host={host} q={}", auth == "Bearer sk_real", req.uri().query().unwrap_or_default()))))) }
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
        let req = Request::get("/v1/charges?limit=1").header("host", "evil.test").header("authorization", auth).body(Full::new(Bytes::new())).unwrap();
        let resp = s.send_request(req).await.unwrap();
        let status = resp.status().as_u16();
        (status, String::from_utf8(resp.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
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
        let i: Arc<dyn Interception> = Arc::new(Fake { issuer: preview_ca, live: true });

        let (mine, theirs) = tokio::io::duplex(64 << 10);
        let served = tokio::spawn(serve(theirs, "api.example.test", 443, i.clone(), upstream_config(roots(&real_pem)), dial_to(addr)));
        let mut s = client(&preview_pem, mine).await;
        // Swapped on the way out, the host's own name sent whatever the request said.
        assert_eq!(get(&mut s, "Bearer rlph_good").await, (200, "real=true host=api.example.test q=limit=1".into()));
        // A placeholder that doesn't swap: refused here, nothing reaches the host.
        let (status, body) = get(&mut s, "Bearer rlph_bad").await;
        assert_eq!((status, body.contains("not a live placeholder"), hits.load(Ordering::SeqCst)), (403, true, 1));
        drop(s);
        let out = served.await.unwrap().unwrap();
        assert_eq!(out, Served { requests: 2, swapped: 1, blocked: vec!["not a live placeholder".into()] });

        // A host the CA doesn't cover, or a CA that ended: nothing is terminated or dialled.
        for (host, live) in [("evil.test", true), ("api.example.test", false)] {
            let i: Arc<dyn Interception> = Arc::new(Fake { issuer: ca("preview").0, live });
            let (_mine, theirs) = tokio::io::duplex(1024);
            assert!(serve(theirs, host, 443, i, upstream_config(roots(&real_pem)), dial_to(addr)).await.is_err(), "{host} live={live}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);

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
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
