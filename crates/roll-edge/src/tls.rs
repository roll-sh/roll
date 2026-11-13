//! Certificates and rustls: a store of certificates by hostname (wildcards included), consulted
//! on every handshake, so installing a renewed certificate takes effect on the next connection.

use anyhow::Context as _;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{self, ServerConfig};
use tracing::{info, warn};

/// A certificate chain and its private key, PEM.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CertPem {
    pub chain: String,
    pub key: String,
}

impl std::fmt::Debug for CertPem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CertPem")
    }
}

/// A parsed certificate, ready to serve.
#[derive(Clone)]
pub struct Parsed {
    pub key: Arc<CertifiedKey>,
    /// The DNS names it covers (`*.pg.us-east-1.roll.cloud` for a wildcard), lowercase.
    pub names: Vec<String>,
    /// Unix seconds.
    pub not_after: i64,
    pub serial: String,
}

impl Parsed {
    /// Whether it covers every name in `names`.
    pub fn covers(&self, names: &[String]) -> bool {
        names.iter().all(|n| self.names.iter().any(|m| m.eq_ignore_ascii_case(n)))
    }
}

impl CertPem {
    pub fn parse(&self) -> anyhow::Result<Parsed> {
        let chain = CertificateDer::pem_slice_iter(self.chain.as_bytes()).collect::<Result<Vec<_>, _>>().context("certificate chain")?;
        let leaf = chain.first().context("no certificate in the chain")?.clone();
        let key = PrivateKeyDer::from_pem_slice(self.key.as_bytes()).context("private key")?;
        let signer = rustls::crypto::ring::sign::any_supported_type(&key).context("private key type")?;
        let (_, x) = x509_parser::parse_x509_certificate(&leaf).map_err(|e| anyhow::anyhow!("certificate: {e}"))?;
        let mut names: Vec<String> = match x.subject_alternative_name() {
            Ok(Some(san)) => san
                .value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    x509_parser::extensions::GeneralName::DNSName(d) => Some(d.to_ascii_lowercase()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        if names.is_empty() {
            names.extend(x.subject().iter_common_name().filter_map(|c| c.as_str().ok()).map(str::to_ascii_lowercase));
        }
        let parsed = Parsed { names, not_after: x.validity().not_after.timestamp(), serial: x.raw_serial_as_string(), key: Arc::new(CertifiedKey::new(chain, signer)) };
        Ok(parsed)
    }
}

/// A self-signed certificate for `names` (dev mode and tests).
pub fn self_signed(names: &[String]) -> anyhow::Result<CertPem> {
    let ck = rcgen::generate_simple_self_signed(names.to_vec())?;
    Ok(CertPem { chain: ck.cert.pem(), key: ck.signing_key.serialize_pem() })
}

#[derive(Default)]
struct Inner {
    by_name: HashMap<String, Arc<CertifiedKey>>,
    latest: Option<Arc<CertifiedKey>>,
}

/// Certificates by the names they cover. Handshakes ask it for each connection's SNI: an exact
/// name first, then the wildcard one label up. Clients that send no SNI (an IP address) get the
/// certificate for [`CertStore::set_default_name`], else the one installed last.
#[derive(Default)]
pub struct CertStore {
    inner: RwLock<Inner>,
    default_name: RwLock<Option<String>>,
    /// The name whose certificate a server name with none of its own gets (unset: none, and the
    /// handshake fails).
    unknown_name: RwLock<Option<String>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore").field("names", &self.inner.read().unwrap().by_name.keys().collect::<Vec<_>>()).finish()
    }
}

impl CertStore {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Serve `cert` for every name it covers, from the next handshake on.
    pub fn install(&self, cert: &Parsed) {
        let mut inner = self.inner.write().unwrap();
        for n in &cert.names {
            inner.by_name.insert(n.clone(), cert.key.clone());
        }
        inner.latest = Some(cert.key.clone());
    }

    /// Stop serving the certificate installed for exactly `name` (a custom domain's, when the
    /// domain goes). Names it shares a certificate with keep it.
    pub fn remove(&self, name: &str) {
        let mut inner = self.inner.write().unwrap();
        if let Some(key) = inner.by_name.remove(&name.to_ascii_lowercase())
            && inner.latest.as_ref().is_some_and(|l| Arc::ptr_eq(l, &key))
        {
            inner.latest = None;
        }
    }

    /// The name whose certificate clients without SNI get.
    pub fn set_default_name(&self, name: &str) {
        *self.default_name.write().unwrap() = Some(name.to_ascii_lowercase());
    }

    /// The name whose certificate a server name without one of its own gets: behind Cloudflare
    /// for SaaS, a custom hostname's connections from Cloudflare carry the custom hostname, and
    /// get the fallback origin's certificate.
    pub fn set_unknown_name(&self, name: &str) {
        *self.unknown_name.write().unwrap() = Some(name.to_ascii_lowercase());
    }

    /// The certificate for `name` (`None`: no SNI).
    pub fn lookup(&self, name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let inner = self.inner.read().unwrap();
        let find = |name: &str| {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            inner.by_name.get(&name).cloned().or_else(|| name.split_once('.').and_then(|(_, up)| inner.by_name.get(&format!("*.{up}")).cloned()))
        };
        match name {
            Some(n) => find(n).or_else(|| self.unknown_name.read().unwrap().as_deref().and_then(find)),
            None => self.default_name.read().unwrap().as_deref().and_then(find).or_else(|| inner.latest.clone()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().by_name.is_empty()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.lookup(hello.server_name())
    }
}

/// A server config that picks certificates from `store`. `alpn`: the protocols to offer (a
/// client offering others is refused; none offered is fine).
pub fn server_config(store: Arc<CertStore>, alpn: &[&[u8]]) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default versions")
        .with_no_client_auth()
        .with_cert_resolver(store);
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(cfg)
}

/// [`server_config`], requiring a client certificate that chains to one of `client_roots`.
pub fn server_config_verifying_clients(store: Arc<CertStore>, alpn: &[&[u8]], client_roots: &[CertificateDer<'static>]) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for r in client_roots {
        roots.add(r.clone())?;
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let mut cfg = ServerConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_client_cert_verifier(verifier).with_cert_resolver(store);
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(Arc::new(cfg))
}

/// The certificates in a PEM bundle.
pub fn pem_certificates(pem: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem.as_bytes()).collect::<Result<Vec<_>, _>>().context("PEM certificates")?;
    anyhow::ensure!(!certs.is_empty(), "no certificate in the PEM bundle");
    Ok(certs)
}

/// A directory of `<name>.crt` + `<name>.key` pairs (PEM): the dev mode's certificates, or
/// certificates an operator provides. Pairs are reloaded when their files change.
pub struct CertDir {
    dir: PathBuf,
    seen: HashMap<PathBuf, (SystemTime, SystemTime)>,
}

impl CertDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), seen: HashMap::new() }
    }

    /// Write a self-signed certificate for `names` as `edge.crt`/`edge.key` when the directory
    /// has no certificate yet.
    pub fn ensure_self_signed(&self, names: &[String]) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir).with_context(|| format!("create {}", self.dir.display()))?;
        if self.pairs()?.next().is_some() {
            return Ok(());
        }
        let pem = self_signed(names)?;
        std::fs::write(self.dir.join("edge.crt"), &pem.chain)?;
        write_private(&self.dir.join("edge.key"), pem.key.as_bytes())?;
        info!(dir = %self.dir.display(), ?names, "edge: wrote a self-signed certificate");
        Ok(())
    }

    fn pairs(&self) -> anyhow::Result<impl Iterator<Item = (PathBuf, PathBuf)>> {
        let entries = std::fs::read_dir(&self.dir).with_context(|| format!("read {}", self.dir.display()))?;
        Ok(entries.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "crt")).filter_map(|crt| {
            let key = crt.with_extension("key");
            key.exists().then_some((crt, key))
        }))
    }

    /// Install every pair that is new or changed since the last call. Returns how many.
    pub fn reload(&mut self, store: &CertStore) -> anyhow::Result<usize> {
        let mut n = 0;
        let pairs: Vec<_> = self.pairs()?.collect();
        for (crt, key) in pairs {
            let stamp = (mtime(&crt), mtime(&key));
            if self.seen.get(&crt) == Some(&stamp) {
                continue;
            }
            let pem = CertPem { chain: std::fs::read_to_string(&crt)?, key: std::fs::read_to_string(&key)? };
            match pem.parse() {
                Ok(p) => {
                    store.install(&p);
                    info!(file = %crt.display(), names = ?p.names, "edge: certificate loaded");
                    n += 1;
                }
                Err(e) => warn!(file = %crt.display(), error = %e, "edge: certificate not loaded"),
            }
            self.seen.insert(crt, stamp);
        }
        Ok(n)
    }

    /// Reload every `every` forever.
    pub async fn watch(mut self, store: Arc<CertStore>, every: Duration) {
        loop {
            tokio::time::sleep(every).await;
            if let Err(e) = self.reload(&store) {
                warn!(error = %e, "edge: reading the certificate directory");
            }
        }
    }
}

fn mtime(p: &Path) -> SystemTime {
    std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)?.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_exact_wildcard_and_default() {
        let store = CertStore::new();
        let a = self_signed(&["roll.test".into(), "*.roll.test".into()]).unwrap().parse().unwrap();
        let b = self_signed(&["*.pg.r1.roll.test".into()]).unwrap().parse().unwrap();
        store.install(&a);
        store.install(&b);
        let is = |got: Option<Arc<CertifiedKey>>, want: &Parsed| got.is_some_and(|g| g.cert == want.key.cert);
        assert!(is(store.lookup(Some("ROLL.test.")), &a));
        assert!(is(store.lookup(Some("api.roll.test")), &a));
        assert!(is(store.lookup(Some("ep1.pg.r1.roll.test")), &b));
        assert!(store.lookup(Some("a.b.roll.test")).is_none(), "a wildcard covers one label");
        assert!(store.lookup(Some("other.test")).is_none());
        assert!(is(store.lookup(None), &b), "no SNI: the last installed");
        store.set_default_name("api.roll.test");
        assert!(is(store.lookup(None), &a));
        assert!(a.covers(&["*.roll.test".into()]) && !a.covers(&["*.pg.r1.roll.test".into()]));
        let c = self_signed(&["shop.example.com".into()]).unwrap().parse().unwrap();
        store.install(&c);
        assert!(is(store.lookup(Some("shop.example.com")), &c));
        store.remove("shop.example.com");
        assert!(store.lookup(Some("shop.example.com")).is_none(), "a removed domain's certificate isn't served");
        store.set_unknown_name("up.roll.test");
        assert!(is(store.lookup(Some("other.example")), &a), "an unknown name gets the fallback's");
    }
}
