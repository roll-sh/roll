//! ACME's challenge checks (roll-secrets' acme module, docs/secrets.md "ACME"): `http-01` through a
//! guarded client (`roll_net::PublicOnly`, the literal-host check, no redirects, no proxy, a
//! timeout and a bounded read), `dns-01` through public DNS (Cloudflare's resolvers, not the
//! fleet's own, so a private zone can't answer for a public name), with a timeout and bounded
//! answers, and `tls-alpn-01` (RFC 8737) as one `acme-tls/1` handshake with an address the same
//! guard resolved, port 443 only, within the timeout. A deployment that allows private addresses
//! (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`, local development) reaches them and resolves through the
//! system's resolver.

use hickory_resolver::proto::rr::{RData, RecordType};
use roll_secrets::acme::AlpnFailure;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_rustls::rustls;

const TIMEOUT: Duration = Duration::from_secs(10);
/// TXT records read at one name, and bytes kept of one.
const TXT_MAX: usize = 32;
const TXT_BYTES: usize = 1024;
/// tls-alpn-01's protocol, and the extension its certificate carries (id-pe-acmeIdentifier).
const ACME_TLS: &[u8] = b"acme-tls/1";
const ACME_IDENTIFIER: &str = "1.3.6.1.5.5.7.1.31";
/// The largest challenge certificate taken (it's self-signed with one name: a few hundred bytes).
const CERT_MAX: usize = 16 * 1024;

pub(super) struct Checks {
    client: reqwest::Client,
    allow_private: bool,
}

impl Checks {
    pub(super) fn new(allow_private: bool) -> Checks {
        let mut b = reqwest::Client::builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).user_agent("roll-secrets-acme");
        if !allow_private {
            b = b.dns_resolver(roll_net::PublicOnly).no_proxy();
        }
        Checks { client: b.build().expect("an HTTP client"), allow_private }
    }

    /// A resolver per lookup: nothing cached, so a record a client just added is seen.
    fn resolver(&self) -> Result<hickory_resolver::TokioResolver, String> {
        use hickory_resolver::config::{CLOUDFLARE, ResolverConfig};
        let mut b = if self.allow_private {
            hickory_resolver::TokioResolver::builder_tokio().map_err(|e| e.to_string())?
        } else {
            hickory_resolver::Resolver::builder_with_config(ResolverConfig::udp_and_tcp(&CLOUDFLARE), Default::default())
        };
        b.options_mut().timeout = Duration::from_secs(5);
        b.options_mut().attempts = 2;
        b.build().map_err(|e| e.to_string())
    }
}

impl Checks {
    /// tls-alpn-01 at `name`: its first address the guard allows, port 443.
    async fn alpn(&self, name: &str, digest: &[u8; 32]) -> Result<(), AlpnFailure> {
        let tls = AlpnFailure::Tls;
        let u = url::Url::parse(&format!("https://{name}/")).map_err(|e| tls(e.to_string()))?;
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err(tls(format!("{name} is a private address")));
        }
        let resolved = async {
            let addrs: Vec<SocketAddr> = if self.allow_private {
                tokio::net::lookup_host((name, 443)).await.map_err(|e| e.to_string())?.collect()
            } else {
                use reqwest::dns::Resolve;
                let n: reqwest::dns::Name = name.parse().map_err(|_| format!("{name} isn't a name"))?;
                roll_net::PublicOnly.resolve(n).await.map_err(|e| e.to_string())?.collect()
            };
            addrs.into_iter().next().ok_or_else(|| format!("{name} has no address"))
        };
        let mut addr = tokio::time::timeout(TIMEOUT, resolved).await.map_err(|_| tls("resolving timed out".into()))?.map_err(tls)?;
        addr.set_port(443);
        alpn_at(addr, name, digest).await
    }
}

/// One `acme-tls/1` handshake with `addr` (SNI `name`) within the timeout, then the certificate it
/// presented checked against `name` and `digest`.
async fn alpn_at(addr: SocketAddr, name: &str, digest: &[u8; 32]) -> Result<(), AlpnFailure> {
    let tls = AlpnFailure::Tls;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(OnlyParses(provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![ACME_TLS.to_vec()];
    let server = rustls::pki_types::ServerName::try_from(name.to_string()).map_err(|_| tls(format!("{name} isn't a name")))?;
    let handshake = async {
        let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let conn = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(server, tcp).await.map_err(|e| e.to_string())?;
        let (_, session) = conn.get_ref();
        if session.alpn_protocol() != Some(ACME_TLS) {
            return Err("the server didn't choose acme-tls/1".to_string());
        }
        session.peer_certificates().and_then(|c| c.first()).map(|c| c.to_vec()).ok_or_else(|| "the server presented no certificate".to_string())
    };
    let cert = tokio::time::timeout(TIMEOUT, handshake).await.map_err(|_| tls("timed out".into()))?.map_err(tls)?;
    proves(&cert, name, digest).map_err(AlpnFailure::Incorrect)
}

/// Whether a challenge certificate proves `name` (RFC 8737 §3): its one subjectAltName is `name`,
/// and the critical acmeIdentifier extension is an OCTET STRING of `digest`.
fn proves(der: &[u8], name: &str, digest: &[u8; 32]) -> Result<(), String> {
    use x509_parser::extensions::GeneralName;
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|_| "the certificate doesn't parse".to_string())?;
    let san = cert.subject_alternative_name().map_err(|_| "the certificate's subjectAltName doesn't parse".to_string())?;
    match san.map(|s| s.value.general_names.as_slice()) {
        Some([GeneralName::DNSName(n)]) if n.eq_ignore_ascii_case(name) => {}
        _ => return Err(format!("the certificate's subjectAltName isn't exactly {name}")),
    }
    let ids: Vec<_> = cert.extensions().iter().filter(|e| e.oid.to_id_string() == ACME_IDENTIFIER).collect();
    let [id] = ids.as_slice() else { return Err("the certificate has no acmeIdentifier extension".into()) };
    if !id.critical {
        return Err("the certificate's acmeIdentifier extension isn't critical".into());
    }
    if id.value.len() != 34 || id.value[..2] != [0x04, 0x20] || id.value[2..] != digest[..] {
        return Err("the certificate's acmeIdentifier isn't the key authorization's digest".into());
    }
    Ok(())
}

/// tls-alpn-01's certificate is self-signed by design: any certificate is taken that parses and
/// fits [`CERT_MAX`], and what it proves is checked after the handshake. The handshake's own
/// signatures are still verified against the certificate's key, so the server holds it (with the
/// key itself: webpki refuses the critical extension the certificate must carry).
#[derive(Debug)]
struct OnlyParses(Arc<rustls::crypto::CryptoProvider>);

impl OnlyParses {
    /// `message` signed with `cert`'s key, under one of the algorithms the scheme maps to.
    fn signed_by(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        let (_, c) = x509_parser::parse_x509_certificate(cert).map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        let key = &c.public_key().subject_public_key.data;
        let algs = self.0.signature_verification_algorithms.mapping.iter().find(|(s, _)| *s == dss.scheme).map(|(_, a)| *a).unwrap_or_default();
        if algs.iter().any(|a| a.verify_signature(key, message, dss.signature()).is_ok()) {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature))
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for OnlyParses {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.len() > CERT_MAX || x509_parser::parse_x509_certificate(end_entity).is_err() {
            return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.signed_by(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &rustls::pki_types::CertificateDer<'_>, dss: &rustls::DigitallySignedStruct) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.signed_by(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn block_on<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Handle::current().block_on(f)
}

impl roll_secrets::acme::Checks for Checks {
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        let resolver = self.resolver()?;
        let found = block_on(tokio::time::timeout(TIMEOUT, resolver.lookup(format!("{name}."), RecordType::TXT))).map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
        Ok(found
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                RData::TXT(t) => Some(t.txt_data.iter().flat_map(|s| s.iter().copied()).take(TXT_BYTES).collect::<Vec<u8>>()),
                _ => None,
            })
            .take(TXT_MAX)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect())
    }

    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, String> {
        let u = url::Url::parse(url).map_err(|e| e.to_string())?;
        if u.scheme() != "http" || u.host_str().is_none() {
            return Err("an http-01 challenge is fetched over http://".into());
        }
        if !self.allow_private && roll_net::literal_private_host(&u) {
            return Err(format!("{} is a private address", u.host_str().unwrap_or_default()));
        }
        block_on(async {
            let mut resp = self.client.get(u).send().await.map_err(|e| e.without_url().to_string())?;
            if !resp.status().is_success() {
                return Err(format!("answered {}", resp.status()));
            }
            let mut out = Vec::new();
            while let Some(c) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
                if out.len() + c.len() > limit {
                    return Err(format!("an answer over {limit} bytes"));
                }
                out.extend_from_slice(&c);
            }
            Ok(out)
        })
    }

    fn tls_alpn(&self, name: &str, digest: &[u8; 32]) -> Result<(), AlpnFailure> {
        block_on(self.alpn(name, digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roll_secrets::acme::Checks as _;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    const NAME: &str = "challenge.roll.example";

    /// A TLS server on loopback presenting a certificate for [`NAME`] with `ext`, choosing
    /// `acme-tls/1` when `alpn`.
    async fn serve(ext: Option<rcgen::CustomExtension>, alpn: bool) -> SocketAddr {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![NAME.to_string()]).unwrap();
        params.custom_extensions = ext.into_iter().collect();
        let cert = params.self_signed(&key).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        // Served as is: with_single_cert would refuse the critical extension it doesn't know.
        let signer = provider.key_provider.load_private_key(PrivateKeyDer::Pkcs8(key.serialize_der().into())).unwrap();
        let certified = rustls::sign::CertifiedKey::new(vec![CertificateDer::from(cert.der().to_vec())], signer);
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
        if alpn {
            config.alpn_protocols = vec![ACME_TLS.to_vec()];
        }
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move { drop(acceptor.accept(tcp).await) });
            }
        });
        addr
    }

    #[tokio::test]
    async fn tls_alpn_01_takes_only_an_acme_tls_handshake_with_the_digest_in_a_critical_extension() {
        let digest = [7u8; 32];
        let good = serve(Some(rcgen::CustomExtension::new_acme_identifier(&digest)), true).await;
        alpn_at(good, NAME, &digest).await.expect("the challenge certificate proves the name");

        let wrong = alpn_at(good, NAME, &[8u8; 32]).await;
        assert!(matches!(&wrong, Err(AlpnFailure::Incorrect(m)) if m.contains("digest")), "{wrong:?}");

        let plain = serve(Some(rcgen::CustomExtension::new_acme_identifier(&digest)), false).await;
        let no_alpn = alpn_at(plain, NAME, &digest).await;
        assert!(matches!(&no_alpn, Err(AlpnFailure::Tls(m)) if m.contains("acme-tls/1")), "{no_alpn:?}");

        let mut ext = rcgen::CustomExtension::from_oid_content(&[1, 3, 6, 1, 5, 5, 7, 1, 31], [&[0x04, 0x20][..], &digest].concat());
        ext.set_criticality(false);
        let lax = serve(Some(ext), true).await;
        let not_critical = alpn_at(lax, NAME, &digest).await;
        assert!(matches!(&not_critical, Err(AlpnFailure::Incorrect(m)) if m.contains("critical")), "{not_critical:?}");

        // The guard: a private name is refused before anything is resolved or dialled.
        let private = Checks::new(false);
        let refused = tokio::task::spawn_blocking(move || private.tls_alpn("localhost", &digest)).await.unwrap();
        assert!(matches!(&refused, Err(AlpnFailure::Tls(m)) if m.contains("private")), "{refused:?}");
    }
}
