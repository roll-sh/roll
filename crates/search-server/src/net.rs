//! TLS and the PROXY protocol (v1 and v2), which AWS Network Load Balancers use to pass the
//! client's address. The same helpers as `redis-server`'s listener, kept local so the search
//! node doesn't depend on the Redis crates.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// A TLS server config from a PEM certificate chain and private key, offering HTTP/1.1.
pub fn tls_config(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<Arc<ServerConfig>> {
    let certs = CertificateDer::pem_slice_iter(cert_pem).collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg =
        ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_no_client_auth().with_single_cert(certs, key)?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

pub fn tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> anyhow::Result<TlsAcceptor> {
    Ok(TlsAcceptor::from(tls_config(cert_pem, key_pem)?))
}

const V2_SIG: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// Reads a PROXY protocol header without consuming anything after it. Returns the client
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
    r.read_exact(&mut head[5..]).await?;
    if &head[..12] != V2_SIG {
        anyhow::bail!("expected a PROXY protocol header");
    }
    let ver_cmd = head[12];
    let fam = head[13];
    // At most 65535 bytes, bounded by the header's own u16 length.
    let len = u16::from_be_bytes([head[14], head[15]]) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    if ver_cmd >> 4 != 2 {
        anyhow::bail!("unsupported PROXY protocol version");
    }
    if ver_cmd & 0xf == 0 {
        return Ok(None);
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
    async fn proxy_v1_and_v2() {
        let mut r: &[u8] = b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 9200\r\nGET / HTTP/1.1\r\n";
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), Some("203.0.113.7:51234".parse().unwrap()));
        assert_eq!(r, b"GET / HTTP/1.1\r\n");
        let mut h = V2_SIG.to_vec();
        h.extend_from_slice(&[0x21, 0x11, 0, 12, 198, 51, 100, 9, 10, 0, 0, 1, 0x1f, 0x90, 0x23, 0xf0]);
        let mut r: &[u8] = &h;
        assert_eq!(read_proxy_header(&mut r).await.unwrap(), Some("198.51.100.9:8080".parse().unwrap()));
        let mut r: &[u8] = b"GET / HTTP/1.1\r\n";
        assert!(read_proxy_header(&mut r).await.is_err());
    }
}
