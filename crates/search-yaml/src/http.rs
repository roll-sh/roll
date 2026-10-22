//! A small HTTP/1.1 client: one keep-alive connection per worker, raw request targets.
//!
//! The runner talks to one host and sends requests one at a time per worker, so a pooled client
//! buys nothing; a dedicated hyper connection gives keep-alive, exact control over the request
//! target (no URL normalization: an index named `..` or `%2E` must reach the server as written)
//! and repeated response headers (`Warning`). A connection the server closed is replaced
//! transparently before the next request, but a request is resent only when the server cannot
//! have processed it: a response dropped after the request went out fails the test (as with
//! Elasticsearch's RestClient) instead of being hidden by a second, possibly non-idempotent, try.
//!
//! `https://` endpoints use rustls: the platform's trust store by default, a PEM CA file instead
//! (Elasticsearch's generated `http_ca.crt`), or no verification at all (`--insecure`), set once
//! per process with [`set_tls_options`].

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};

/// How `https://` endpoints are verified.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Trust only the certificates in this PEM file (instead of the platform's trust store).
    pub ca_file: Option<PathBuf>,
    /// Don't verify the server's certificate at all.
    pub insecure: bool,
}

static TLS_OPTIONS: OnceLock<TlsOptions> = OnceLock::new();
static DEFAULT_CONNECTOR: OnceLock<Result<TlsConnector, String>> = OnceLock::new();

/// Sets the process-wide TLS options; `false` if they were already set (or already used).
pub fn set_tls_options(options: TlsOptions) -> bool {
    TLS_OPTIONS.set(options).is_ok()
}

fn default_connector() -> anyhow::Result<TlsConnector> {
    DEFAULT_CONNECTOR
        .get_or_init(|| tls_connector(TLS_OPTIONS.get_or_init(TlsOptions::default)).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(|e| anyhow!(e))
}

/// Builds a connector for the options.
pub fn tls_connector(options: &TlsOptions) -> anyhow::Result<TlsConnector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions()?;
    let config = if options.insecure {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider))).with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        match &options.ca_file {
            Some(path) => {
                let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                let mut n = 0;
                for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                    roots.add(cert.with_context(|| format!("parsing {}", path.display()))?)?;
                    n += 1;
                }
                if n == 0 {
                    bail!("no certificates in {}", path.display());
                }
            }
            None => {
                let native = rustls_native_certs::load_native_certs();
                roots.add_parsable_certificates(native.certs);
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

/// `--insecure`: any certificate is accepted, but handshake signatures are still checked.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The largest response body accepted (a misbehaving target must not exhaust memory).
const MAX_RESPONSE_BYTES: usize = 512 << 20;

/// Where to send requests and how to authenticate.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// `https://`.
    pub tls: bool,
    pub host: String,
    pub port: u16,
    /// Path prefix of the base URL (no trailing slash), prepended to every request target.
    pub prefix: String,
    /// Headers sent with every request (e.g. `Authorization`).
    pub headers: Vec<(String, String)>,
}

impl Endpoint {
    /// Parses `http[s]://[user:pass@]host[:port][/prefix]`.
    pub fn parse(url: &str) -> anyhow::Result<Endpoint> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else {
            bail!("expected an http:// or https:// URL, got {url}");
        };
        let default_port = if tls { 443 } else { 80 };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        let mut headers = Vec::new();
        let hostport = match authority.rsplit_once('@') {
            Some((userinfo, hp)) => {
                let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
                headers.push(("Authorization".to_owned(), basic_auth(&percent_decode(user), &percent_decode(pass))));
                hp
            }
            None => authority,
        };
        let (host, port) = if let Some(h) = hostport.strip_prefix('[') {
            let (h, p) = h.split_once(']').ok_or_else(|| anyhow!("bad IPv6 host in {url}"))?;
            (h.to_owned(), p.strip_prefix(':').map(str::parse).transpose()?.unwrap_or(default_port))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), p.parse().with_context(|| format!("bad port in {url}"))?),
                None => (hostport.to_owned(), default_port),
            }
        };
        if host.is_empty() {
            bail!("missing host in {url}");
        }
        Ok(Endpoint { tls, host, port, prefix: path.trim_end_matches('/').to_owned(), headers })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `Basic base64(user:password)`.
pub fn basic_auth(user: &str, password: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether an error comes from a connection reset or a write to a closed connection.
fn is_reset(e: &hyper::Error) -> bool {
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return matches!(io.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe);
        }
        source = s.source();
    }
    false
}

/// A received response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    /// Header names lowercased, in received order, repeated headers kept.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn headers_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// One keep-alive connection to an endpoint.
pub struct Client {
    endpoint: Endpoint,
    sender: Option<SendRequest<Full<Bytes>>>,
    timeout: Duration,
    /// For `https://`: this connector instead of the process-wide one.
    tls: Option<TlsConnector>,
}

async fn handshake<T>(io: T) -> anyhow::Result<SendRequest<Full<Bytes>>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http1::Builder::new().title_case_headers(true).handshake(TokioIo::new(io)).await?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(sender)
}

impl Client {
    pub fn new(endpoint: Endpoint, timeout: Duration) -> Client {
        Client { endpoint, sender: None, timeout, tls: None }
    }

    /// Uses `connector` for `https://` instead of the process-wide options.
    pub fn with_tls(mut self, connector: TlsConnector) -> Client {
        self.tls = Some(connector);
        self
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Drops the connection; the next request opens a new one.
    pub fn disconnect(&mut self) {
        self.sender = None;
    }

    async fn connect(&mut self) -> anyhow::Result<()> {
        let addr = (self.endpoint.host.as_str(), self.endpoint.port);
        let stream = tokio::time::timeout(self.timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("timed out connecting to {}:{}", self.endpoint.host, self.endpoint.port))?
            .with_context(|| format!("connecting to {}:{}", self.endpoint.host, self.endpoint.port))?;
        stream.set_nodelay(true)?;
        let sender = if self.endpoint.tls {
            let connector = match &self.tls {
                Some(c) => c.clone(),
                None => default_connector()?,
            };
            let name = ServerName::try_from(self.endpoint.host.clone()).with_context(|| format!("invalid TLS server name {}", self.endpoint.host))?;
            let tls = tokio::time::timeout(self.timeout, connector.connect(name, stream))
                .await
                .map_err(|_| anyhow!("timed out in the TLS handshake with {}:{}", self.endpoint.host, self.endpoint.port))?
                .with_context(|| format!("TLS handshake with {}:{}", self.endpoint.host, self.endpoint.port))?;
            handshake(tls).await?
        } else {
            handshake(stream).await?
        };
        self.sender = Some(sender);
        Ok(())
    }

    /// Sends a request. `target` is the encoded path and query (without the endpoint prefix).
    pub async fn send(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: Option<(Bytes, String)>,
    ) -> anyhow::Result<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let reused = self.sender.as_ref().is_some_and(|s| !s.is_closed());
            if !reused {
                self.connect().await?;
            }
            let req = self.build(method, target, headers, body.clone())?;
            let sender = self.sender.as_mut().expect("connected");
            // `(error, unsent)`: `unsent` when hyper hands the request back, i.e. no byte of it
            // reached the connection.
            let fut = async {
                if let Err(e) = sender.ready().await {
                    return Err((e, true));
                }
                sender.try_send_request(req).await.map_err(|mut e| {
                    let unsent = e.take_message().is_some();
                    (e.into_error(), unsent)
                })
            };
            match tokio::time::timeout(self.timeout, fut).await {
                Err(_) => {
                    self.sender = None;
                    bail!("timed out after {:?} waiting for {method} {target}", self.timeout);
                }
                Ok(Err((e, unsent))) => {
                    self.sender = None;
                    // Retry only when the server cannot have acted on the request, like
                    // Elasticsearch's RestClient, which never resends: a request hyper never
                    // wrote, or a reset on a reused keep-alive connection (a server that closes
                    // an idle connection with our request unread in its buffer answers with a
                    // reset). A connection that closes after the request went out, before any
                    // response, means the server received it and may have acted on it (a crash
                    // or a handler bug): resending would hide that and repeat side effects.
                    if attempt == 1 && (unsent || (reused && is_reset(&e))) {
                        continue;
                    }
                    return Err(anyhow!(e).context(format!("{method} {target}")));
                }
                Ok(Ok(res)) => {
                    let status = res.status();
                    let mut out_headers = Vec::with_capacity(res.headers().len());
                    for (k, v) in res.headers() {
                        out_headers.push((k.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned()));
                    }
                    let body = tokio::time::timeout(self.timeout, Limited::new(res.into_body(), MAX_RESPONSE_BYTES).collect())
                        .await
                        .map_err(|_| anyhow!("timed out reading the response to {method} {target}"))?
                        .map_err(|e| anyhow!("reading the response to {method} {target}: {e}"))?
                        .to_bytes();
                    return Ok(Response {
                        status: status.as_u16(),
                        reason: status.canonical_reason().unwrap_or("").to_owned(),
                        headers: out_headers,
                        body,
                    });
                }
            }
        }
    }

    fn build(
        &self,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: Option<(Bytes, String)>,
    ) -> anyhow::Result<hyper::Request<Full<Bytes>>> {
        let full_target = format!("{}{}", self.endpoint.prefix, target);
        let mut builder = hyper::Request::builder()
            .method(hyper::Method::from_bytes(method.as_bytes())?)
            .uri(full_target)
            .header("Host", self.endpoint.host_header());
        let mut has_ct = false;
        for (k, v) in self.endpoint.headers.iter().filter(|(k, _)| !headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k))) {
            builder = builder.header(k.as_str(), v.as_str());
        }
        for (k, v) in headers {
            has_ct |= k.eq_ignore_ascii_case("content-type");
            builder = builder.header(k.as_str(), v.as_str());
        }
        let body = match body {
            Some((bytes, content_type)) => {
                if !has_ct {
                    builder = builder.header("Content-Type", content_type);
                }
                Full::new(bytes)
            }
            None => Full::new(Bytes::new()),
        };
        Ok(builder.body(body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        let e = Endpoint::parse("http://127.0.0.1:9200").unwrap();
        assert_eq!((e.host.as_str(), e.port, e.prefix.as_str()), ("127.0.0.1", 9200, ""));
        assert!(e.headers.is_empty());
        let e = Endpoint::parse("http://elastic:p%40ss@localhost:19202/base/").unwrap();
        assert_eq!((e.host.as_str(), e.port, e.prefix.as_str()), ("localhost", 19202, "/base"));
        assert_eq!(e.headers[0].1, basic_auth("elastic", "p@ss"));
        let e = Endpoint::parse("http://[::1]:9200").unwrap();
        assert_eq!((e.host.as_str(), e.port), ("::1", 9200));
        assert_eq!(e.host_header(), "[::1]:9200");
        assert_eq!(Endpoint::parse("http://example.com").unwrap().port, 80);
        let e = Endpoint::parse("https://x").unwrap();
        assert_eq!((e.tls, e.port), (true, 443));
        assert!(!Endpoint::parse("http://x").unwrap().tls);
        assert!(Endpoint::parse("ftp://x").is_err());
    }

    /// A one-request-per-connection HTTPS server for `localhost` with a fresh self-signed
    /// certificate; returns its port and the certificate PEM.
    async fn tls_server() -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_pem = ck.cert.pem();
        let key = rustls::pki_types::PrivateKeyDer::try_from(ck.signing_key.serialize_der()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![ck.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(sock).await else { return };
                    let mut buf = vec![0u8; 4096];
                    let mut seen = Vec::new();
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = tls.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        seen.extend_from_slice(&buf[..n]);
                    }
                    let body = br#"{"tls":true}"#;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = tls.write_all(head.as_bytes()).await;
                    let _ = tls.write_all(body).await;
                    let _ = tls.shutdown().await;
                });
            }
        });
        (port, cert_pem)
    }

    #[tokio::test]
    async fn https_with_a_ca_file_or_insecure() {
        let (port, pem) = tls_server().await;
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, &pem).unwrap();
        let ep = Endpoint::parse(&format!("https://localhost:{port}")).unwrap();
        let timeout = Duration::from_secs(5);

        let trusted = tls_connector(&TlsOptions { ca_file: Some(ca), insecure: false }).unwrap();
        let mut c = Client::new(ep.clone(), timeout).with_tls(trusted);
        let r = c.send("GET", "/", &[], None).await.unwrap();
        assert_eq!(&r.body[..], br#"{"tls":true}"#);
        // The server closed the connection: the next request reconnects (and re-handshakes).
        let r = c.send("GET", "/again", &[], None).await.unwrap();
        assert_eq!(r.status, 200);

        let insecure = tls_connector(&TlsOptions { ca_file: None, insecure: true }).unwrap();
        let r = Client::new(ep.clone(), timeout).with_tls(insecure).send("GET", "/", &[], None).await.unwrap();
        assert_eq!(r.status, 200);

        // A self-signed certificate isn't in the platform's trust store.
        let platform = tls_connector(&TlsOptions::default()).unwrap();
        let err = Client::new(ep, timeout).with_tls(platform).send("GET", "/", &[], None).await.unwrap_err();
        assert!(format!("{err:#}").contains("TLS handshake"), "{err:#}");

        let empty = dir.path().join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        assert!(tls_connector(&TlsOptions { ca_file: Some(empty), insecure: false }).is_err());
    }

    #[test]
    fn basic_auth_header() {
        assert_eq!(basic_auth("user", "password"), "Basic dXNlcjpwYXNzd29yZA==");
    }

    #[tokio::test]
    async fn round_trip_against_a_local_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // Two requests on one connection, then the server closes it; the third request
            // must transparently reconnect.
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                let mut answered = 0;
                while answered < 2 {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    while let Some(end) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&seen[..end]).into_owned();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        if seen.len() < end + 4 + len {
                            break;
                        }
                        let target = head.split(' ').nth(1).unwrap().to_owned();
                        seen.drain(..end + 4 + len);
                        let body = format!("{{\"target\":\"{target}\",\"len\":{len}}}");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nwarning: 299 a \"x\"\r\nwarning: 299 b \"y\"\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        sock.write_all(resp.as_bytes()).await.unwrap();
                        answered += 1;
                    }
                }
            }
        });
        let mut c = Client::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), Duration::from_secs(5));
        let r = c.send("GET", "/a/..%2Fb", &[], None).await.unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(&r.body[..], br#"{"target":"/a/..%2Fb","len":0}"#);
        assert_eq!(r.headers_named("Warning").count(), 2);
        let r = c.send("POST", "/x", &[], Some((Bytes::from_static(b"{}"), "application/json".into()))).await.unwrap();
        assert_eq!(&r.body[..], br#"{"target":"/x","len":2}"#);
        let r = c.send("GET", "/again", &[], None).await.unwrap();
        assert_eq!(&r.body[..], br#"{"target":"/again","len":0}"#);
    }

    /// A server that reads a whole request and drops the connection without answering received
    /// it: the client must fail, not resend (POST _doc with an auto id, _bulk, ...).
    #[tokio::test]
    async fn dropped_response_is_not_retried() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head and the 8-byte body, then close.
                while !got.windows(4).any(|w| w == b"\r\n\r\n") || !got.ends_with(b"{\"f\": 1}") {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(sock);
            }
        });
        let mut c = Client::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), Duration::from_secs(5));
        let body = Some((Bytes::from_static(b"{\"f\": 1}"), "application/json".into()));
        let err = c.send("POST", "/flaky/_doc", &[], body).await.unwrap_err();
        assert!(format!("{err:#}").contains("POST /flaky/_doc"), "{err:#}");
        // Let a (wrong) second attempt reach the server before counting.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The same on a reused keep-alive connection: the server answers one request, then reads
    /// the next completely and closes (a FIN, nothing unread): no resend.
    #[tokio::test]
    async fn dropped_response_on_a_reused_connection_is_not_retried() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                let mut answered = false;
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        got.drain(..end + 4);
                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if answered {
                            break;
                        }
                        let body = "{}";
                        let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len());
                        sock.write_all(resp.as_bytes()).await.unwrap();
                        answered = true;
                    }
                }
                drop(sock);
            }
        });
        let mut c = Client::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), Duration::from_secs(5));
        assert_eq!(c.send("GET", "/first", &[], None).await.unwrap().status, 200);
        assert!(c.send("DELETE", "/second", &[], None).await.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
