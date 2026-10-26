//! Proxy-to-node TLS, with each node's certificate pinned through its fleet record.
//!
//! A node serving its REST listener over TLS publishes the SHA-256 of its certificate (DER) in
//! its heartbeat ([`crate::fleet::Beat::tls`]). The proxy then connects to that address with
//! TLS and accepts exactly that certificate: the handshake signature is checked against the
//! pinned certificate's key, and nothing else about it matters (issuer, names, dates). The
//! trust comes from the metadata store, which only the nodes and proxies can write and which is
//! itself reached over verified TLS. This needs no certificate authority and no names: a node
//! on AWS makes a throwaway self-signed certificate at boot ([`self_signed`]); a node with an
//! operator's certificate publishes that one's fingerprint the same way.
//!
//! The proxy connects by IP address, so no SNI goes out: the node routes by the `Host` header
//! the proxy forwards (which carries the cluster an SNI name chose at the proxy). Connections
//! are pooled per address and scheme like plain ones; a node that restarts at the same address
//! with a new certificate is a new process, so its old pooled connections are already dead and
//! the new pin applies to the next connect. A pin that doesn't match fails the connect, which
//! the proxy treats like an unreachable node (nothing was sent, the request goes elsewhere).
//!
//! Records without a fingerprint (single-host setups, older nodes) keep plain HTTP.
//!
//! The same connector carries the Lambda function's hop to the proxies (through the stack's
//! internal NLB, so a host name rather than one address): there every address takes one pin
//! ([`Pins::any`]), the certificate the proxies make from a secret the stack gives both sides
//! ([`seeded`]). An Ed25519 certificate is fully determined by its key and fields, so every
//! proxy and the function arrive at the same one without exchanging anything.

use hyper::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::{Connected, Connection, HttpConnector};
use hyper_util::rt::TokioIo;
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};

/// SHA-256 of a certificate's DER encoding.
pub type CertPin = [u8; 32];

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The pin of a DER certificate, as published (lowercase hex).
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// The pin of the first (leaf) certificate of a PEM chain.
pub fn pem_fingerprint(pem: &[u8]) -> anyhow::Result<String> {
    let leaf = CertificateDer::pem_slice_iter(pem).next().ok_or_else(|| anyhow::anyhow!("no certificate in the PEM"))??;
    Ok(fingerprint(leaf.as_ref()))
}

/// A published pin back to bytes (`None` if it isn't 64 hex digits).
pub fn parse_pin(s: &str) -> Option<CertPin> {
    let mut pin = [0u8; 32];
    hex::decode_to_slice(s.trim(), &mut pin).ok()?;
    Some(pin)
}

/// A certificate made at boot, never written anywhere: its pin is the only thing that leaves.
pub struct SelfSigned {
    pub cert_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
}

/// A fresh self-signed certificate (ECDSA P-256) for `names` (host names or IP addresses; the
/// proxy ignores them, other clients may not).
pub fn self_signed(names: &[String]) -> anyhow::Result<SelfSigned> {
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "roll search node");
    // Pinned, so expiry is meaningless; a long validity keeps strict clients that look at it
    // from failing on a long-running node.
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    let cert = params.self_signed(&key)?;
    Ok(SelfSigned { fingerprint: fingerprint(cert.der()), cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

/// The proxies' certificate for the Lambda hop, made from `secret` (shared by the proxies and
/// the function through the stack): the same key, fields and signature everywhere, so
/// [`SelfSigned::fingerprint`] is a pin both sides know. Ed25519 because its signatures are
/// deterministic (ECDSA's aren't, so the certificate's bytes would differ per proxy).
pub fn seeded(secret: &[u8]) -> anyhow::Result<SelfSigned> {
    anyhow::ensure!(secret.len() >= 16, "the TLS seed must be at least 16 bytes long");
    let seed = Sha256::new().chain_update(b"roll search proxy tls v1\0").chain_update(secret).finalize();
    // PKCS#8 v1 for an Ed25519 private key (RFC 8410): a fixed header, then the 32-byte seed.
    let mut pkcs8 = Vec::with_capacity(48);
    pkcs8.extend_from_slice(&[0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);
    pkcs8.extend_from_slice(&seed);
    let key = rcgen::KeyPair::try_from(pkcs8.as_slice())?;
    let mut params = rcgen::CertificateParams::new(vec!["roll-search-proxy".to_string()])?;
    params.distinguished_name.push(rcgen::DnType::CommonName, "roll search proxy");
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2125, 1, 1);
    params.serial_number = Some(rcgen::SerialNumber::from(1u64));
    let cert = params.self_signed(&key)?;
    Ok(SelfSigned { fingerprint: fingerprint(cert.der()), cert_pem: cert.pem(), key_pem: key.serialize_pem() })
}

/// Node address → the certificate it must present. Kept by the directory from node records.
#[derive(Default, Debug)]
pub struct Pins {
    by_addr: RwLock<HashMap<SocketAddr, CertPin>>,
    /// For every address without its own pin, and for upstreams named by host name.
    any: Option<CertPin>,
}

impl Pins {
    /// One certificate for every address (the Lambda function's hop to the proxies).
    pub fn any(pin: CertPin) -> Pins {
        Pins { by_addr: RwLock::default(), any: Some(pin) }
    }

    pub fn get(&self, addr: &SocketAddr) -> Option<CertPin> {
        self.by_addr.read().get(addr).copied().or(self.any)
    }

    pub fn contains(&self, addr: &SocketAddr) -> bool {
        self.get(addr).is_some()
    }

    /// Replaces every per-address pin (after a membership refresh).
    pub fn replace(&self, next: HashMap<SocketAddr, CertPin>) {
        let mut cur = self.by_addr.write();
        if *cur != next {
            *cur = next;
        }
    }
}

/// Accepts exactly one certificate.
#[derive(Debug)]
struct PinnedVerifier {
    pin: CertPin,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Client configurations kept per pin (each holds its session cache, so reconnects resume);
/// past this many the cache starts over.
const CONFIGS: usize = 1024;

struct Inner {
    pins: Arc<Pins>,
    provider: Arc<CryptoProvider>,
    configs: Mutex<HashMap<CertPin, Arc<ClientConfig>>>,
    handshake_timeout: Duration,
}

impl Inner {
    fn config(&self, pin: CertPin) -> Result<Arc<ClientConfig>, BoxError> {
        let mut configs = self.configs.lock();
        if let Some(c) = configs.get(&pin) {
            return Ok(c.clone());
        }
        let verifier = Arc::new(PinnedVerifier { pin, algs: self.provider.signature_verification_algorithms });
        let mut cfg = ClientConfig::builder_with_provider(self.provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        // Nodes and proxies route by the forwarded `Host`; an SNI name (the NLB's, for the
        // function) would only be mistaken for a cluster's.
        cfg.enable_sni = false;
        let cfg = Arc::new(cfg);
        if configs.len() >= CONFIGS {
            configs.clear();
        }
        configs.insert(pin, cfg.clone());
        Ok(cfg)
    }
}

/// The proxy's connector: plain TCP for `http://` URIs, TLS against the node's pin for
/// `https://` ones.
#[derive(Clone)]
pub struct Connector {
    http: HttpConnector,
    inner: Arc<Inner>,
}

impl Connector {
    /// `http` must not enforce the `http` scheme (it is set here).
    pub fn new(mut http: HttpConnector, pins: Arc<Pins>, handshake_timeout: Duration) -> Connector {
        http.enforce_http(false);
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Connector { http, inner: Arc::new(Inner { pins, provider, configs: Mutex::default(), handshake_timeout }) }
    }
}

impl tower::Service<Uri> for Connector {
    type Response = Stream;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Stream, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        tower::Service::<Uri>::poll_ready(&mut self.http, cx).map_err(Into::into)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let mut http = self.http.clone();
        let inner = self.inner.clone();
        Box::pin(async move {
            let https = uri.scheme() == Some(&hyper::http::uri::Scheme::HTTPS);
            // The pin is looked up before connecting: no TCP connection for a node that can't
            // be verified.
            let pin = match https {
                false => None,
                true => {
                    let authority = uri.authority().ok_or_else(|| format!("no host in {uri}"))?;
                    let (name, pin) = match authority.as_str().parse::<SocketAddr>() {
                        Ok(addr) => (ServerName::IpAddress(addr.ip().into()), inner.pins.get(&addr)),
                        // A host name (the function's upstream): only a pin for any address fits.
                        Err(_) => (ServerName::try_from(authority.host().to_string()).map_err(|e| format!("bad host in {uri}: {e}"))?, inner.pins.any),
                    };
                    Some((authority.to_string(), name, pin.ok_or_else(|| format!("no certificate pin for search node {authority}"))?))
                }
            };
            let tcp = tower::Service::call(&mut http, uri).await?;
            let Some((addr, name, pin)) = pin else { return Ok(Stream::Plain(tcp)) };
            let tls = tokio_rustls::TlsConnector::from(inner.config(pin)?);
            let stream = tokio::time::timeout(inner.handshake_timeout, tls.connect(name, tcp.into_inner()))
                .await
                .map_err(|_| format!("TLS handshake with search node {addr} timed out"))??;
            Ok(Stream::Tls(Box::new(TokioIo::new(stream))))
        })
    }
}

/// A connection to a node.
pub enum Stream {
    Plain(TokioIo<TcpStream>),
    Tls(Box<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl Connection for Stream {
    fn connected(&self) -> Connected {
        match self {
            Stream::Plain(s) => s.connected(),
            Stream::Tls(s) => s.inner().get_ref().0.connected(),
        }
    }
}

impl Read for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(&mut **s).poll_read(cx, buf),
        }
    }
}

impl Write for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(&mut **s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(&mut **s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(&mut **s).poll_shutdown(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
        }
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Stream::Tls(s) => Pin::new(&mut **s).poll_write_vectored(cx, bufs),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn pins_round_trip() {
        let c = self_signed(&[std::net::Ipv4Addr::LOCALHOST.to_string()]).unwrap();
        assert_eq!(c.fingerprint.len(), 64);
        assert_eq!(pem_fingerprint(c.cert_pem.as_bytes()).unwrap(), c.fingerprint);
        let pin = parse_pin(&c.fingerprint).unwrap();
        assert_eq!(hex::encode(pin), c.fingerprint);
        assert_eq!(parse_pin(&c.fingerprint.to_uppercase()), Some(pin));
        assert_eq!(parse_pin("abc"), None);
        assert_eq!(parse_pin(&"zz".repeat(32)), None);
        assert!(pem_fingerprint(b"nothing").is_err());
        // Every boot makes a new one.
        assert_ne!(self_signed(&[]).unwrap().fingerprint, c.fingerprint);
    }

    #[test]
    fn seeded_certificates_are_the_same_for_the_same_secret() {
        let a = seeded(b"0123456789abcdef-stack-secret").unwrap();
        let b = seeded(b"0123456789abcdef-stack-secret").unwrap();
        assert_eq!((a.fingerprint.as_str(), a.cert_pem.as_str(), a.key_pem.as_str()), (b.fingerprint.as_str(), b.cert_pem.as_str(), b.key_pem.as_str()));
        assert_eq!(pem_fingerprint(a.cert_pem.as_bytes()).unwrap(), a.fingerprint);
        assert_ne!(seeded(b"0123456789abcdef-other-secret").unwrap().fingerprint, a.fingerprint);
        assert!(seeded(b"short").is_err());
        // Its key loads as a rustls server key.
        crate::proxy::tls_acceptor(a.cert_pem.as_bytes(), a.key_pem.as_bytes()).unwrap();
    }

    /// A TLS echo-ish server: answers one HTTP request with `200 tls` over the given cert.
    async fn tls_server(cert: &SelfSigned) -> SocketAddr {
        tls_server_on(cert, (std::net::Ipv4Addr::LOCALHOST, 0)).await
    }

    async fn tls_server_on(cert: &SelfSigned, bind: impl tokio::net::ToSocketAddrs) -> SocketAddr {
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::PrivateKeyDer;
        let certs = vec![CertificateDer::from_pem_slice(cert.cert_pem.as_bytes()).unwrap()];
        let key = PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(certs, key).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
        let l = tokio::net::TcpListener::bind(bind).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = l.accept().await else { return };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut s) = acceptor.accept(s).await else { return };
                    let sni = s.get_ref().1.server_name().map(str::to_string);
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let body = format!("tls sni={sni:?}");
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()).as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        addr
    }

    fn client(pins: Arc<Pins>) -> hyper_util::client::legacy::Client<Connector, http_body_util::Empty<Bytes>> {
        let conn = Connector::new(HttpConnector::new(), pins, Duration::from_secs(2));
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(conn)
    }

    async fn get(c: &hyper_util::client::legacy::Client<Connector, http_body_util::Empty<Bytes>>, uri: String) -> Result<String, String> {
        use http_body_util::BodyExt;
        let r = c.get(uri.parse().unwrap()).await.map_err(|e| format!("{e:?}"))?;
        Ok(String::from_utf8(r.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
    }

    #[tokio::test]
    async fn connects_only_to_the_pinned_certificate() {
        let good = self_signed(&["node".into()]).unwrap();
        let addr = tls_server(&good).await;
        let pins = Arc::new(Pins::default());
        let c = client(pins.clone());
        // No pin for the address: no connection at all.
        let e = get(&c, format!("https://{addr}/")).await.unwrap_err();
        assert!(e.contains("no certificate pin"), "{e}");
        // The right pin: served, and no SNI went out (the node routes by Host).
        pins.replace(HashMap::from([(addr, parse_pin(&good.fingerprint).unwrap())]));
        assert_eq!(get(&c, format!("https://{addr}/")).await.unwrap(), "tls sni=None");
        // Another certificate's pin (an impostor at the address, or a stale record): refused
        // as a connect error, so the proxy never sends the request.
        let other = self_signed(&["node".into()]).unwrap();
        let addr2 = tls_server(&good).await;
        pins.replace(HashMap::from([(addr2, parse_pin(&other.fingerprint).unwrap())]));
        let c2 = client(pins.clone());
        let e = c2.get(format!("https://{addr2}/").parse().unwrap()).await.unwrap_err();
        assert!(e.is_connect(), "{e:?}");
    }

    /// The function's hop: the proxies behind a host name, every address taking the seeded pin;
    /// no SNI goes out (the proxies would read it as a cluster name).
    #[tokio::test]
    async fn host_names_take_the_pin_for_any_address() {
        let cert = seeded(b"a secret of the stack, 32 bytes!").unwrap();
        let addr = tls_server(&cert).await;
        let c = client(Arc::new(Pins::any(parse_pin(&cert.fingerprint).unwrap())));
        assert_eq!(get(&c, format!("https://localhost:{}/", addr.port())).await.unwrap(), "tls sni=None");
        assert_eq!(get(&c, format!("https://{addr}/")).await.unwrap(), "tls sni=None");
        // Another secret's certificate is refused; without any pin a host name isn't reached.
        let other = seeded(b"another secret of the stack, 32!").unwrap();
        let c = client(Arc::new(Pins::any(parse_pin(&other.fingerprint).unwrap())));
        assert!(get(&c, format!("https://localhost:{}/", addr.port())).await.is_err());
        let e = get(&client(Arc::default()), format!("https://localhost:{}/", addr.port())).await.unwrap_err();
        assert!(e.contains("no certificate pin"), "{e}");
    }

    #[tokio::test]
    async fn plain_uris_stay_plain() {
        let l = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nplain").await;
        });
        assert_eq!(get(&client(Arc::default()), format!("http://{addr}/")).await.unwrap(), "plain");
    }
}
