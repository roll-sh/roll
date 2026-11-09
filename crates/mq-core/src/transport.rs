//! Internal transports (broker ↔ keeper, broker ↔ broker): optional TLS, then a mutual
//! challenge-response handshake on a shared secret.
//!
//! The handshake always runs, so both sides must agree on the secret (none is the empty
//! secret) and a peer from another cluster or a stray scanner is turned away before it can send
//! a request. Each side proves it knows the secret without sending it, over fresh nonces, and
//! the purpose (`keeper`, `cluster`) is mixed in so a secret shared by two services can't be
//! replayed across them:
//!
//! ```text
//! client → server  "RLLT" [version u8] [purpose len u8][purpose] [client nonce 32]
//! server → client  [server nonce 32] [hmac(secret, "server" purpose cn sn) 32]
//! client → server  [hmac(secret, "client" purpose sn cn) 32]
//! server → client  [0 = accepted]
//! ```
//!
//! TLS (rustls) encrypts and authenticates the server by certificate; the secret
//! authenticates the client. Inside one VPC, Nitro instances already encrypt traffic between
//! them, so TLS is optional and the secret is what keeps other workloads out.

use anyhow::{Context, bail};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

const MAGIC: &[u8; 4] = b"RLLT";
const VERSION: u8 = 1;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How an internal endpoint secures its connections. Everything is optional; the default is
/// plain TCP with the empty secret.
#[derive(Clone, Debug, Default)]
pub struct TransportConfig {
    /// Shared secret both sides must know.
    pub secret: Option<String>,
    /// Server: certificate chain and private key (PEM). Both enable TLS on accept.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Client: connect with TLS.
    pub tls: bool,
    /// Client: trust these CA certificates (PEM) instead of the system roots.
    pub tls_ca: Option<PathBuf>,
    /// Client: name to verify the server certificate against (defaults to the peer's IP).
    pub tls_server_name: Option<String>,
}

/// A loaded [`TransportConfig`] for one purpose.
pub struct Transport {
    purpose: &'static str,
    secret: Vec<u8>,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
    connector: Option<(tokio_rustls::TlsConnector, Option<ServerName<'static>>)>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transport")
            .field("purpose", &self.purpose)
            .field("secret", &!self.secret.is_empty())
            .field("tls_server", &self.acceptor.is_some())
            .field("tls_client", &self.connector.is_some())
            .finish()
    }
}

impl Transport {
    /// Plain TCP, empty secret.
    pub fn plain(purpose: &'static str) -> Arc<Transport> {
        Arc::new(Transport { purpose, secret: Vec::new(), acceptor: None, connector: None })
    }

    /// Load certificates and keys. Fails on unreadable files, so a misconfigured node doesn't start.
    pub fn new(purpose: &'static str, config: &TransportConfig) -> anyhow::Result<Arc<Transport>> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let acceptor = match (&config.tls_cert, &config.tls_key) {
            (Some(cert), Some(key)) => {
                let chain = CertificateDer::pem_file_iter(cert)
                    .with_context(|| format!("{}: reading certificates", cert.display()))?
                    .collect::<Result<Vec<_>, _>>()
                    .with_context(|| format!("{}: bad certificate", cert.display()))?;
                if chain.is_empty() {
                    bail!("{}: no certificates", cert.display());
                }
                let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("{}: reading private key", key.display()))?;
                let server = rustls::ServerConfig::builder_with_provider(provider.clone())
                    .with_safe_default_protocol_versions()?
                    .with_no_client_auth()
                    .with_single_cert(chain, key)
                    .context("certificate and key don't match")?;
                Some(tokio_rustls::TlsAcceptor::from(Arc::new(server)))
            }
            (None, None) => None,
            _ => bail!("{purpose} TLS needs both a certificate and a private key"),
        };
        let connector = if config.tls || config.tls_ca.is_some() {
            let mut roots = rustls::RootCertStore::empty();
            match &config.tls_ca {
                Some(ca) => {
                    for c in CertificateDer::pem_file_iter(ca).with_context(|| format!("{}: reading CA certificates", ca.display()))? {
                        roots.add(c.with_context(|| format!("{}: bad CA certificate", ca.display()))?)?;
                    }
                }
                None => {
                    for c in rustls_native_certs::load_native_certs().certs {
                        let _ = roots.add(c);
                    }
                }
            }
            if roots.is_empty() {
                bail!("{purpose} TLS: no trusted CA certificates");
            }
            let client = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = match &config.tls_server_name {
                Some(n) => Some(ServerName::try_from(n.clone()).with_context(|| format!("bad TLS server name {n}"))?),
                None => None,
            };
            Some((tokio_rustls::TlsConnector::from(Arc::new(client)), name))
        } else {
            None
        };
        Ok(Arc::new(Transport { purpose, secret: config.secret.clone().unwrap_or_default().into_bytes(), acceptor, connector }))
    }

    pub fn is_tls_server(&self) -> bool {
        self.acceptor.is_some()
    }

    /// Server side: TLS (when configured), then the handshake.
    pub async fn accept(&self, sock: TcpStream) -> anyhow::Result<Stream> {
        let _ = sock.set_nodelay(true);
        tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let mut s = match &self.acceptor {
                Some(a) => Stream::ServerTls(Box::new(a.accept(sock).await.context("TLS handshake")?)),
                None => Stream::Plain(sock),
            };
            self.server_handshake(&mut s).await?;
            Ok(s)
        })
        .await
        .context("handshake timed out")?
    }

    /// Client side: connect, TLS (when configured), then the handshake.
    pub async fn connect(&self, addr: SocketAddr, timeout: Duration) -> anyhow::Result<Stream> {
        tokio::time::timeout(timeout.max(HANDSHAKE_TIMEOUT), async {
            let sock = tokio::time::timeout(timeout, TcpStream::connect(addr)).await.context("connect timed out")??;
            let _ = sock.set_nodelay(true);
            let mut s = match &self.connector {
                Some((c, name)) => {
                    let name = name.clone().unwrap_or_else(|| ServerName::IpAddress(addr.ip().into()));
                    Stream::ClientTls(Box::new(c.connect(name, sock).await.context("TLS handshake")?))
                }
                None => Stream::Plain(sock),
            };
            self.client_handshake(&mut s).await?;
            Ok(s)
        })
        .await
        .context("handshake timed out")?
    }

    fn mac(&self, role: &[u8], a: &[u8], b: &[u8]) -> Hmac<Sha256> {
        let mut m = Hmac::<Sha256>::new_from_slice(&self.secret).expect("hmac takes any key length");
        for part in [role, self.purpose.as_bytes(), a, b] {
            m.update(&(part.len() as u32).to_be_bytes());
            m.update(part);
        }
        m
    }

    async fn client_handshake(&self, s: &mut Stream) -> anyhow::Result<()> {
        let cn = nonce();
        let mut hello = Vec::with_capacity(40 + self.purpose.len());
        hello.extend_from_slice(MAGIC);
        hello.push(VERSION);
        hello.push(self.purpose.len() as u8);
        hello.extend_from_slice(self.purpose.as_bytes());
        hello.extend_from_slice(&cn);
        s.write_all(&hello).await?;
        s.flush().await?;
        let mut reply = [0u8; 64];
        s.read_exact(&mut reply).await.context("peer closed during the handshake (is it the right service?)")?;
        let (sn, proof) = reply.split_at(32);
        self.mac(b"server", &cn, sn).verify_slice(proof).map_err(|_| anyhow::anyhow!("{} peer doesn't know the shared secret", self.purpose))?;
        s.write_all(&self.mac(b"client", sn, &cn).finalize().into_bytes()).await?;
        s.flush().await?;
        let mut ok = [0u8; 1];
        s.read_exact(&mut ok).await.with_context(|| format!("{} peer refused our secret", self.purpose))?;
        if ok[0] != 0 {
            bail!("{} peer refused the handshake", self.purpose);
        }
        Ok(())
    }

    async fn server_handshake(&self, s: &mut Stream) -> anyhow::Result<()> {
        let mut head = [0u8; 6];
        s.read_exact(&mut head).await?;
        if &head[..4] != MAGIC {
            bail!("not a roll {} client", self.purpose);
        }
        if head[4] != VERSION {
            bail!("unsupported handshake version {}", head[4]);
        }
        let mut purpose = vec![0u8; head[5] as usize];
        s.read_exact(&mut purpose).await?;
        if purpose != self.purpose.as_bytes() {
            bail!("client wants {}, this is {}", String::from_utf8_lossy(&purpose), self.purpose);
        }
        let mut cn = [0u8; 32];
        s.read_exact(&mut cn).await?;
        let sn = nonce();
        let mut reply = Vec::with_capacity(64);
        reply.extend_from_slice(&sn);
        reply.extend_from_slice(&self.mac(b"server", &cn, &sn).finalize().into_bytes());
        s.write_all(&reply).await?;
        s.flush().await?;
        let mut proof = [0u8; 32];
        s.read_exact(&mut proof).await?;
        if self.mac(b"client", &sn, &cn).verify_slice(&proof).is_err() {
            bail!("client doesn't know the shared secret");
        }
        s.write_all(&[0]).await?;
        s.flush().await?;
        Ok(())
    }
}

fn nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n).expect("system randomness");
    n
}

/// A connection made by a [`Transport`].
pub enum Stream {
    Plain(TcpStream),
    ServerTls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
    ClientTls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

pub type ReadHalf = Box<dyn AsyncRead + Send + Unpin>;
pub type WriteHalf = Box<dyn AsyncWrite + Send + Unpin>;

impl Stream {
    /// Split for a reader and a writer task (plain TCP splits without a lock).
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        match self {
            Stream::Plain(s) => {
                let (r, w) = s.into_split();
                (Box::new(r), Box::new(w))
            }
            tls => {
                let (r, w) = tokio::io::split(tls);
                (Box::new(r), Box::new(w))
            }
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::ServerTls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
            Stream::ClientTls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::ServerTls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
            Stream::ClientTls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::ServerTls(s) => Pin::new(s.as_mut()).poll_flush(cx),
            Stream::ClientTls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::ServerTls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            Stream::ClientTls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn pair(server: Arc<Transport>, client: Arc<Transport>) -> (anyhow::Result<Stream>, anyhow::Result<Stream>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let s = tokio::spawn(async move { server.accept(l.accept().await.unwrap().0).await });
        let c = client.connect(addr, Duration::from_secs(2)).await;
        (s.await.unwrap(), c)
    }

    fn secret(purpose: &'static str, s: &str) -> Arc<Transport> {
        Transport::new(purpose, &TransportConfig { secret: Some(s.into()), ..Default::default() }).unwrap()
    }

    #[tokio::test]
    async fn matching_secrets_talk() {
        let (s, c) = pair(secret("keeper", "k1"), secret("keeper", "k1")).await;
        let (mut s, mut c) = (s.unwrap(), c.unwrap());
        c.write_all(b"ping").await.unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping");
        let (s, c) = pair(Transport::plain("keeper"), Transport::plain("keeper")).await;
        assert!(s.is_ok() && c.is_ok());
    }

    #[tokio::test]
    async fn wrong_or_missing_secret_is_refused() {
        for (server, client) in [("k1", "k2"), ("k1", ""), ("", "k1")] {
            let (s, c) = pair(secret("keeper", server), secret("keeper", client)).await;
            assert!(s.is_err() && c.is_err(), "{server:?} vs {client:?}");
        }
    }

    #[tokio::test]
    async fn purposes_dont_mix() {
        let (s, c) = pair(secret("keeper", "same"), secret("cluster", "same")).await;
        assert!(s.is_err() && c.is_err());
    }

    #[tokio::test]
    async fn junk_is_turned_away() {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = secret("keeper", "k");
        let s = tokio::spawn(async move { server.accept(l.accept().await.unwrap().0).await });
        let mut c = TcpStream::connect(addr).await.unwrap();
        c.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        assert!(s.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn tls_with_a_private_ca() {
        let dir = std::env::temp_dir().join(format!("roll-transport-{}", crate::rand_u64()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(rcgen::CertificateParams::new(Vec::<String>::new()).unwrap(), &ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["keeper.internal".into(), "127.0.0.1".into()]).unwrap().signed_by(&key, &issuer).unwrap();
        let file = |name: &str, pem: String| {
            let p = dir.join(name);
            std::fs::write(&p, pem).unwrap();
            Some(p)
        };
        let server = TransportConfig { secret: Some("k".into()), tls_cert: file("cert.pem", cert.pem()), tls_key: file("key.pem", key.serialize_pem()), ..Default::default() };
        let ca_file = file("ca.pem", ca.pem());
        let client = |name: Option<&str>| TransportConfig { secret: Some("k".into()), tls: true, tls_ca: ca_file.clone(), tls_server_name: name.map(String::from), ..Default::default() };
        let server = Transport::new("keeper", &server).unwrap();
        assert!(server.is_tls_server());

        // By IP (the default) and by name.
        for name in [None, Some("keeper.internal")] {
            let (s, c) = pair(server.clone(), Transport::new("keeper", &client(name)).unwrap()).await;
            let (mut s, mut c) = (s.unwrap(), c.unwrap());
            assert!(matches!(c, Stream::ClientTls(_)));
            c.write_all(b"over tls").await.unwrap();
            let mut b = [0u8; 8];
            s.read_exact(&mut b).await.unwrap();
            assert_eq!(&b, b"over tls");
        }
        // The wrong name, a client without TLS, and one that doesn't trust the CA all fail.
        let (s, c) = pair(server.clone(), Transport::new("keeper", &client(Some("other.internal"))).unwrap()).await;
        assert!(s.is_err() && c.is_err());
        let (s, c) = pair(server.clone(), secret("keeper", "k")).await;
        assert!(s.is_err() && c.is_err());
        let other_ca = rcgen::CertificateParams::new(vec!["x".into()]).unwrap().self_signed(&rcgen::KeyPair::generate().unwrap()).unwrap();
        let untrusting = TransportConfig { tls_ca: file("other.pem", other_ca.pem()), ..client(None) };
        let (s, c) = pair(server, Transport::new("keeper", &untrusting).unwrap()).await;
        assert!(s.is_err() && c.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn half_a_tls_config_is_an_error() {
        let e = Transport::new("keeper", &TransportConfig { tls_cert: Some("/nonexistent.pem".into()), ..Default::default() }).unwrap_err();
        assert!(e.to_string().contains("both"), "{e}");
    }
}
