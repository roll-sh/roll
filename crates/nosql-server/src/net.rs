//! Listeners: plain TCP, TLS (with SNI for database routing), the PROXY protocol (v1 and v2,
//! which AWS Network Load Balancers use to pass the client's address) and ScyllaDB's
//! shard-aware port. Follows `redis_server::net`.
//!
//! TLS may verify client certificates ([`ClientAuth`], `client_encryption_options`'
//! `require_client_auth` and truststore): the leaf of a verified chain goes to the connection
//! as a [`ClientCertificate`] for the certificate authenticators. With
//! [`ListenOptions::tls_optional`] (Cassandra's `client_encryption_options.optional`) a port
//! serves TLS and plain text alike, told apart by the first byte (a TLS handshake record is
//! 0x16; a CQL frame starts with its version).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::conn::{self, Accepted};
use crate::directory::Directory;

#[derive(Clone, Default)]
pub struct ListenOptions {
    pub tls: Option<TlsAcceptor>,
    /// Expect a PROXY protocol header on every connection (behind an NLB / HAProxy).
    pub proxy_protocol: bool,
    /// ScyllaDB's shard-aware port: a connection's shard is its source port modulo the shard
    /// count (the client's port from the PROXY header when there is one).
    pub shard_aware: bool,
    /// Accept plain-text connections on a TLS port too (Cassandra's
    /// `client_encryption_options.optional`).
    pub tls_optional: bool,
}

/// Whether TLS asks clients for a certificate (`require_client_auth`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientAuth {
    /// No client certificates (`false`).
    #[default]
    None,
    /// Asked for and verified when sent; clients without one connect too (ScyllaDB's
    /// `optional`).
    Optional,
    /// Required: the handshake fails without a certificate the client CA verifies (`true`).
    Required,
}

impl ClientAuth {
    /// `require_client_auth` as the configuration files write it: `true`/`false`/`optional`.
    pub fn parse(s: &str) -> Option<ClientAuth> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "false" | "no" | "0" | "none" => Some(ClientAuth::None),
            "optional" => Some(ClientAuth::Optional),
            "true" | "yes" | "1" | "required" | "require" => Some(ClientAuth::Required),
            _ => None,
        }
    }
}

pub use nosql_engine::auth::certificate::ClientCertificate;

/// A TLS server config from a PEM certificate chain and private key.
pub fn tls_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<ServerConfig>> {
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(certs, key)?;
    Ok(Arc::new(cfg))
}

pub fn tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<TlsAcceptor> {
    Ok(TlsAcceptor::from(tls_config(cert_pem, key_pem)?))
}

/// A TLS server config that verifies client certificates against the CA certificates of
/// `client_ca_pem` (`auth` other than [`ClientAuth::None`] needs them).
pub fn tls_config_with_clients(cert_pem: &[u8], key_pem: &[u8], client_ca_pem: Option<&[u8]>, auth: ClientAuth) -> anyhow::Result<Arc<ServerConfig>> {
    if auth == ClientAuth::None {
        return tls_config(cert_pem, key_pem);
    }
    let ca = client_ca_pem.ok_or_else(|| anyhow::anyhow!("verifying client certificates needs a client CA (truststore)"))?;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(ca) {
        roots.add(c?)?;
    }
    anyhow::ensure!(!roots.is_empty(), "the client CA file holds no certificate");
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone());
    if auth == ClientAuth::Optional {
        verifier = verifier.allow_unauthenticated();
    }
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let cfg = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_client_cert_verifier(verifier.build()?).with_single_cert(certs, key)?;
    Ok(Arc::new(cfg))
}

pub fn tls_acceptor_with_clients(cert_pem: &[u8], key_pem: &[u8], client_ca_pem: Option<&[u8]>, auth: ClientAuth) -> anyhow::Result<TlsAcceptor> {
    Ok(TlsAcceptor::from(tls_config_with_clients(cert_pem, key_pem, client_ca_pem, auth)?))
}

/// What the certificate authenticators read of a (verified) client certificate: the subject in
/// RFC 4514 form (most specific RDN first, `CN=alice,O=MyOrg`) and the subject alternative
/// names (`EMAIL`, `DNS`, `URI`, `IP`). `None` when it doesn't parse.
pub fn client_certificate(der: &[u8]) -> Option<ClientCertificate> {
    use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let mut rdns = Vec::new();
    for rdn in cert.subject().iter_rdn() {
        let parts: Vec<String> = rdn.iter().map(|a| format!("{}={}", attribute_name(&a.attr_type().to_id_string()), attribute_value(a))).collect();
        rdns.push(parts.join("+"));
    }
    rdns.reverse();
    let mut alt_names = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for n in &san.value.general_names {
            let (t, v) = match n {
                GeneralName::RFC822Name(v) => ("EMAIL", v.to_string()),
                GeneralName::DNSName(v) => ("DNS", v.to_string()),
                GeneralName::URI(v) => ("URI", v.to_string()),
                GeneralName::IPAddress(b) => match b.len() {
                    4 => ("IP", std::net::Ipv4Addr::from(<[u8; 4]>::try_from(*b).ok()?).to_string()),
                    16 => ("IP", std::net::Ipv6Addr::from(<[u8; 16]>::try_from(*b).ok()?).to_string()),
                    _ => continue,
                },
                _ => continue,
            };
            alt_names.push((t.to_string(), v));
        }
    }
    Some(ClientCertificate { subject: rdns.join(","), alt_names })
}

/// RFC 4514's short names (and GnuTLS's `EMAIL`); other attributes print as their OID.
fn attribute_name(oid: &str) -> String {
    match oid {
        "2.5.4.3" => "CN",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "STREET",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "0.9.2342.19200300.100.1.25" => "DC",
        "0.9.2342.19200300.100.1.1" => "UID",
        "1.2.840.113549.1.9.1" => "EMAIL",
        other => return other.to_string(),
    }
    .to_string()
}

/// An attribute value escaped as RFC 4514 says; one that isn't a string is `#` and its hex.
fn attribute_value(a: &x509_parser::x509::AttributeTypeAndValue<'_>) -> String {
    let Ok(v) = a.as_str() else {
        return format!("#{}", a.attr_value().data.iter().map(|b| format!("{b:02x}")).collect::<String>());
    };
    let mut out = String::with_capacity(v.len());
    let n = v.chars().count();
    for (i, c) in v.chars().enumerate() {
        let special = matches!(c, ',' | '+' | '"' | '\\' | '<' | '>' | ';') || (i == 0 && (c == '#' || c == ' ')) || (i + 1 == n && c == ' ');
        if special {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether a connection on a [`ListenOptions::tls_optional`] port starts a TLS handshake.
async fn starts_tls(sock: &tokio::net::TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(tokio::time::timeout(Duration::from_secs(10), sock.peek(&mut b)).await, Ok(Ok(1)) if b[0] == 0x16)
}

/// Accept CQL connections forever.
pub async fn serve(listener: TcpListener, dir: Arc<dyn Directory>, opts: ListenOptions) -> anyhow::Result<()> {
    loop {
        let (mut sock, mut peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Out of descriptors and the like: back off instead of spinning.
                tracing::warn!(error = %e, "nosql accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let dir = dir.clone();
        let opts = opts.clone();
        tokio::spawn(async move {
            if opts.proxy_protocol {
                match tokio::time::timeout(Duration::from_secs(5), read_proxy_header(&mut sock)).await {
                    Ok(Ok(Some(src))) => peer = src,
                    Ok(Ok(None)) => {}
                    _ => return,
                }
            }
            let tls = match opts.tls {
                Some(a) if !opts.tls_optional || starts_tls(&sock).await => Some(a),
                _ => None,
            };
            match tls {
                Some(acceptor) => {
                    let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(sock)).await else { return };
                    let session = tls.get_ref().1;
                    let sni = session.server_name().map(str::to_string);
                    let details = tls_details(session);
                    // The verifier checked the chain; the leaf names the client.
                    let certificate = session.peer_certificates().and_then(|c| c.first()).and_then(|c| client_certificate(c.as_ref())).map(Arc::new);
                    conn::serve(Box::new(tls), Accepted { peer, sni, shard_aware: opts.shard_aware, tls: details, certificate }, dir).await;
                }
                None => conn::serve(Box::new(sock), Accepted { peer, sni: None, shard_aware: opts.shard_aware, tls: None, certificate: None }, dir).await,
            }
        });
    }
}

/// The negotiated protocol and cipher suite, named as Java (Cassandra's `system_views.clients`)
/// names them: `TLSv1.3`, `TLS_AES_256_GCM_SHA384`.
pub fn tls_details(c: &tokio_rustls::rustls::ServerConnection) -> Option<nosql_engine::system::clients::Tls> {
    let protocol = format!("{:?}", c.protocol_version()?).replace('_', ".");
    let suite = format!("{:?}", c.negotiated_cipher_suite()?.suite());
    let cipher_suite = suite.strip_prefix("TLS13_").map_or(suite.clone(), |s| format!("TLS_{s}"));
    Some(nosql_engine::system::clients::Tls { protocol, cipher_suite })
}

/// Serve on an ephemeral local port (tests, embedding). Returns the address.
pub async fn start(dir: Arc<dyn Directory>, opts: ListenOptions) -> anyhow::Result<SocketAddr> {
    start_at(dir, opts, SocketAddr::from(([127, 0, 0, 1], 0))).await
}

pub async fn start_at(dir: Arc<dyn Directory>, opts: ListenOptions, at: SocketAddr) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind(at).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { serve(listener, dir, opts).await });
    Ok(addr)
}

const V2_SIG: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// Read a PROXY protocol header without consuming anything after it. Returns the client
/// address (`None` for `LOCAL`/`UNKNOWN`, e.g. load balancer health checks).
pub async fn read_proxy_header<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<Option<SocketAddr>> {
    let mut head = [0u8; 16];
    r.read_exact(&mut head[..5]).await?;
    if &head[..5] == b"PROXY" {
        // v1: a text line of at most 107 bytes.
        let mut line = b"PROXY".to_vec();
        loop {
            let b = r.read_u8().await?;
            line.push(b);
            if line.ends_with(b"\r\n") {
                break;
            }
            if line.len() > 107 {
                anyhow::bail!("PROXY v1 header too long");
            }
        }
        let line = std::str::from_utf8(&line[..line.len() - 2])?;
        let parts: Vec<&str> = line.split(' ').collect();
        return match parts.as_slice() {
            ["PROXY", "TCP4" | "TCP6", src, _dst, sport, _dport] => Ok(Some(SocketAddr::new(src.parse()?, sport.parse()?))),
            ["PROXY", "UNKNOWN", ..] => Ok(None),
            _ => anyhow::bail!("bad PROXY v1 header"),
        };
    }
    if head[..5] != V2_SIG[..5] {
        // Neither header: don't wait for bytes a client that sent none won't send.
        anyhow::bail!("expected a PROXY protocol header");
    }
    r.read_exact(&mut head[5..]).await?;
    if &head[..12] != V2_SIG {
        anyhow::bail!("expected a PROXY protocol header");
    }
    let ver_cmd = head[12];
    let fam = head[13];
    // At most 64 KiB: bounded by the u16.
    let len = u16::from_be_bytes([head[14], head[15]]) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    if ver_cmd >> 4 != 2 {
        anyhow::bail!("unsupported PROXY protocol version");
    }
    if ver_cmd & 0xf == 0 {
        return Ok(None); // LOCAL
    }
    Ok(match fam >> 4 {
        1 if body.len() >= 12 => {
            let ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be_bytes([body[8], body[9]])))
        }
        2 if body.len() >= 36 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&body[..16]);
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), u16::from_be_bytes([body[32], body[33]])))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_v1() {
        let mut r: &[u8] = b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 9042\r\n\x04\x00";
        let a = read_proxy_header(&mut r).await.unwrap();
        assert_eq!(a, Some("203.0.113.7:51234".parse().unwrap()));
        assert_eq!(r, b"\x04\x00");
        let mut r: &[u8] = b"PROXY TCP6 2001:db8::1 2001:db8::2 4000 9042\r\n";
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), Some("[2001:db8::1]:4000".parse().unwrap()));
        let mut r: &[u8] = b"PROXY UNKNOWN\r\n";
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), None);
        let mut r: &[u8] = b"PROXY TCP4 x y z w\r\n";
        assert!(read_proxy_header(&mut r).await.is_err());
        let long = [b"PROXY ".as_slice(), &[b'a'; 200]].concat();
        let mut r: &[u8] = &long;
        assert!(read_proxy_header(&mut r).await.is_err());
    }

    #[tokio::test]
    async fn proxy_v2() {
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x21, 0x11, 0, 12, 198, 51, 100, 9, 10, 0, 0, 1, 0x1f, 0x90, 0x23, 0x82]);
        h.extend_from_slice(b"rest");
        let mut r: &[u8] = &h;
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), Some("198.51.100.9:8080".parse().unwrap()));
        assert_eq!(r, b"rest");
        // IPv6.
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x21, 0x21, 0, 36]);
        let mut body = [0u8; 36];
        body[15] = 1;
        body[32..34].copy_from_slice(&7000u16.to_be_bytes());
        h.extend_from_slice(&body);
        let mut r: &[u8] = &h;
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), Some("[::1]:7000".parse().unwrap()));
        // LOCAL (health check).
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x20, 0x00, 0, 0]);
        let mut r: &[u8] = &h;
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), None);
        // Not PROXY, or truncated.
        let mut r: &[u8] = b"\x04\x00\x00\x01\x05\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00";
        assert!(read_proxy_header(&mut r).await.is_err());
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x21, 0x11, 0, 12, 1]);
        let mut r: &[u8] = &h;
        assert!(read_proxy_header(&mut r).await.is_err());
        // Version 1 in a v2 header.
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x11, 0x11, 0, 0]);
        let mut r: &[u8] = &h;
        assert!(read_proxy_header(&mut r).await.is_err());
    }
}
