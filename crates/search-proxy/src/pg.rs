//! One Postgres connection for the fleet's metadata (node records, the cluster catalog),
//! re-established when it drops.
//!
//! TLS follows the URL's `sslmode` the way libpq does for the modes that matter: `disable`
//! never, `prefer` (the default) when the server offers it, `require` and stricter always. The
//! server certificate is always verified, against the system roots plus an optional PEM bundle
//! (the RDS CA bundle ships in the image), so `require` is never "encrypted but unverified".
//!
//! A password can come separately from the URL (`with_password`): ECS injects secrets as whole
//! environment variables, and RDS-managed master secrets hold the password alone.
//!
//! The connection task sleeps on its socket, so an idle fleet costs no timer here.

use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};

pub struct Pg {
    client: Mutex<tokio_postgres::Client>,
    config: tokio_postgres::Config,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
}

impl Pg {
    /// Connects to `url`; `ca` adds trusted CA certificates (PEM), `password` overrides the
    /// URL's.
    pub async fn connect(url: &str, ca: Option<&Path>, password: Option<&str>) -> anyhow::Result<Arc<Pg>> {
        let mut config: tokio_postgres::Config = libpq_url(url).parse()?;
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            config.password(p);
        }
        let tls = match config.get_ssl_mode() {
            tokio_postgres::config::SslMode::Disable => None,
            _ => Some(tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(ca)?)),
        };
        let client = open(&config, tls.as_ref()).await?;
        Ok(Arc::new(Pg { client: Mutex::new(client), config, tls }))
    }

    /// The client, reconnected first if the connection closed.
    pub async fn client(&self) -> anyhow::Result<MutexGuard<'_, tokio_postgres::Client>> {
        let mut c = self.client.lock().await;
        if c.is_closed() {
            *c = open(&self.config, self.tls.as_ref()).await?;
        }
        Ok(c)
    }
}

async fn open(config: &tokio_postgres::Config, tls: Option<&tokio_postgres_rustls::MakeRustlsConnect>) -> anyhow::Result<tokio_postgres::Client> {
    fn watch<F: std::future::Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static>(conn: F) {
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "fleet metadata connection closed");
            }
        });
    }
    Ok(match tls {
        Some(tls) => {
            let (client, conn) = config.connect(tls.clone()).await?;
            watch(conn);
            client
        }
        None => {
            let (client, conn) = config.connect(tokio_postgres::NoTls).await?;
            watch(conn);
            client
        }
    })
}

/// `url` with libpq's `sslmode=verify-ca` / `verify-full` spelled `require`, which
/// tokio-postgres doesn't parse. The meaning is kept: with TLS this module always verifies the
/// certificate chain and the host name, so `require` here is libpq's `verify-full`. RDS guides
/// (and secrets written by hand) use `verify-full`.
pub fn libpq_url(url: &str) -> std::borrow::Cow<'_, str> {
    let Some((base, query)) = url.split_once('?') else { return url.into() };
    if !query.split('&').any(|kv| matches!(kv, "sslmode=verify-ca" | "sslmode=verify-full")) {
        return url.into();
    }
    let query: Vec<&str> = query.split('&').map(|kv| if matches!(kv, "sslmode=verify-ca" | "sslmode=verify-full") { "sslmode=require" } else { kv }).collect();
    format!("{base}?{}", query.join("&")).into()
}

/// A rustls client configuration trusting the system roots and `ca`.
pub fn tls_config(ca: Option<&Path>) -> anyhow::Result<tokio_rustls::rustls::ClientConfig> {
    use anyhow::Context;
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(ca) = ca {
        for c in CertificateDer::pem_file_iter(ca).with_context(|| format!("{}: reading CA certificates", ca.display()))? {
            roots.add(c.with_context(|| format!("{}: bad CA certificate", ca.display()))?)?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_root_certificates(roots).with_no_client_auth())
}

/// Where fleet metadata lives: `postgres://…` (a fleet) or a SQLite file path (one host).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetaUrl {
    Postgres(String),
    Sqlite(std::path::PathBuf),
}

impl MetaUrl {
    pub fn parse(s: &str) -> MetaUrl {
        if s.starts_with("postgres://") || s.starts_with("postgresql://") {
            MetaUrl::Postgres(s.to_string())
        } else {
            MetaUrl::Sqlite(s.strip_prefix("sqlite://").or_else(|| s.strip_prefix("sqlite:")).unwrap_or(s).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_urls() {
        assert_eq!(MetaUrl::parse("postgres://u@h/db?sslmode=require"), MetaUrl::Postgres("postgres://u@h/db?sslmode=require".into()));
        assert_eq!(MetaUrl::parse("postgresql://h/db"), MetaUrl::Postgres("postgresql://h/db".into()));
        assert_eq!(MetaUrl::parse("/var/lib/roll/fleet.db"), MetaUrl::Sqlite("/var/lib/roll/fleet.db".into()));
        assert_eq!(MetaUrl::parse("sqlite:///tmp/f.db"), MetaUrl::Sqlite("/tmp/f.db".into()));
        assert_eq!(MetaUrl::parse("sqlite:f.db"), MetaUrl::Sqlite("f.db".into()));
    }

    #[test]
    fn libpq_ssl_modes() {
        assert_eq!(libpq_url("postgres://u@h/db"), "postgres://u@h/db");
        assert_eq!(libpq_url("postgres://u@h/db?sslmode=require"), "postgres://u@h/db?sslmode=require");
        assert_eq!(libpq_url("postgres://u@h/db?sslmode=verify-full"), "postgres://u@h/db?sslmode=require");
        assert_eq!(libpq_url("postgres://u@h/db?application_name=x&sslmode=verify-ca&connect_timeout=5"), "postgres://u@h/db?application_name=x&sslmode=require&connect_timeout=5");
        for u in ["postgres://u@h/db?sslmode=verify-full", "postgres://u@h/db?sslmode=verify-ca"] {
            assert!(u.parse::<tokio_postgres::Config>().is_err(), "tokio-postgres learned {u}: drop the rewrite");
            let c: tokio_postgres::Config = libpq_url(u).parse().unwrap();
            assert_eq!(c.get_ssl_mode(), tokio_postgres::config::SslMode::Require);
        }
    }

    #[test]
    fn tls_config_builds_and_rejects_bad_bundles() {
        assert!(tls_config(None).is_ok());
        let dir = std::env::temp_dir().join(format!("search-proxy-pg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.pem");
        std::fs::write(&bad, "-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n").unwrap();
        assert!(tls_config(Some(&bad)).is_err());
        assert!(tls_config(Some(&dir.join("missing.pem"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        // The image's CA bundle (Amazon RDS's global bundle, deploy/search/Dockerfile) must load:
        // every certificate in it is added, and one rustls can't take fails the start.
        if let Ok(bundle) = std::env::var("ROLL_TEST_CA_BUNDLE") {
            tls_config(Some(Path::new(&bundle))).expect("the CA bundle loads");
        }
    }
}
