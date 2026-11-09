//! Metadata store on Postgres (roll postgres in production). Writes are serialised through a
//! single version row, so the change log is gap-free and every node can follow it in order.
//!
//! TLS follows the URL's `sslmode`: `disable` never, `prefer` (the default) when the server
//! offers it, `require` always. Certificates are always verified, against the system roots plus
//! the CA bundle given to [`PgMeta::connect_with`] (RDS needs its own bundle). A lost
//! connection (a Postgres restart, a Multi-AZ failover) is re-established on the next call.

use async_trait::async_trait;
use mq_core::meta::{Change, MetaStore};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tracing::{info, warn};

pub struct PgMeta {
    client: Mutex<Client>,
    config: tokio_postgres::Config,
    tls: Option<MakeRustlsConnect>,
    url: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mq_meta (kind TEXT NOT NULL, key TEXT NOT NULL, value JSONB NOT NULL, version BIGINT NOT NULL, PRIMARY KEY (kind, key));
CREATE INDEX IF NOT EXISTS mq_meta_vhost ON mq_meta (kind, split_part(key, E'\\x1f', 1));
CREATE TABLE IF NOT EXISTS mq_meta_log (version BIGINT PRIMARY KEY, kind TEXT NOT NULL, key TEXT NOT NULL, value JSONB);
CREATE TABLE IF NOT EXISTS mq_meta_version (id INT PRIMARY KEY CHECK (id = 1), version BIGINT NOT NULL);
INSERT INTO mq_meta_version (id, version) VALUES (1, 0) ON CONFLICT DO NOTHING;
";

impl PgMeta {
    /// Connect (e.g. `postgres://user:pass@host:5432/db`) and create the tables if needed.
    pub async fn connect(url: &str) -> anyhow::Result<Arc<Self>> {
        Self::connect_with(url, None).await
    }

    /// [`PgMeta::connect`], also trusting the CA certificates in `ca` (PEM).
    pub async fn connect_with(url: &str, ca: Option<&Path>) -> anyhow::Result<Arc<Self>> {
        let config: tokio_postgres::Config = url.parse()?;
        let tls = match config.get_ssl_mode() {
            SslMode::Disable => None,
            _ => Some(MakeRustlsConnect::new(tls_config(ca)?)),
        };
        let client = open(&config, tls.as_ref()).await?;
        // One statement per transaction. As one batch (a single transaction) the index's table
        // lock (taken even when the index exists) would be held while the version row waits for
        // another node's running write, and that write's insert into the table would deadlock
        // with it.
        for statement in SCHEMA.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            client.batch_execute(statement).await?;
        }
        Ok(Arc::new(PgMeta { client: Mutex::new(client), config, tls, url: redact(url) }))
    }

    /// The connection, re-established if it was lost.
    async fn client(&self) -> anyhow::Result<MutexGuard<'_, Client>> {
        let mut c = self.client.lock().await;
        if c.is_closed() {
            *c = open(&self.config, self.tls.as_ref()).await?;
            info!(url = %self.url, "metadata connection re-established");
        }
        Ok(c)
    }

    /// Write `value` (or delete) inside a transaction that takes the next version.
    async fn write(&self, kind: &str, key: &str, value: Option<&Value>, expected: Option<Option<i64>>) -> anyhow::Result<Option<i64>> {
        let mut c = self.client().await?;
        let tx = c.transaction().await?;
        if let Some(exp) = expected {
            let cur: Option<i64> = tx.query_opt("SELECT version FROM mq_meta WHERE kind = $1 AND key = $2 FOR UPDATE", &[&kind, &key]).await?.map(|r| r.get(0));
            if cur != exp {
                tx.rollback().await?;
                return Ok(None);
            }
        } else if value.is_none() {
            let exists = tx.query_opt("SELECT 1 FROM mq_meta WHERE kind = $1 AND key = $2", &[&kind, &key]).await?.is_some();
            if !exists {
                tx.rollback().await?;
                return Ok(None);
            }
        }
        let v: i64 = tx.query_one("UPDATE mq_meta_version SET version = version + 1 WHERE id = 1 RETURNING version", &[]).await?.get(0);
        match value {
            Some(val) => {
                tx.execute(
                    "INSERT INTO mq_meta (kind, key, value, version) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (kind, key) DO UPDATE SET value = EXCLUDED.value, version = EXCLUDED.version",
                    &[&kind, &key, val, &v],
                )
                .await?;
            }
            None => {
                tx.execute("DELETE FROM mq_meta WHERE kind = $1 AND key = $2", &[&kind, &key]).await?;
            }
        }
        tx.execute("INSERT INTO mq_meta_log (version, kind, key, value) VALUES ($1, $2, $3, $4)", &[&v, &kind, &key, &value]).await?;
        if v % 10_000 == 0 {
            tx.execute("DELETE FROM mq_meta_log WHERE version < $1", &[&(v - 200_000)]).await?;
        }
        tx.commit().await?;
        Ok(Some(v))
    }
}

async fn open(config: &tokio_postgres::Config, tls: Option<&MakeRustlsConnect>) -> anyhow::Result<Client> {
    fn watch<F: std::future::Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static>(conn: F) {
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                warn!(error = %e, "metadata connection lost");
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
            let (client, conn) = config.connect(NoTls).await?;
            watch(conn);
            client
        }
    })
}

fn tls_config(ca: Option<&Path>) -> anyhow::Result<rustls::ClientConfig> {
    use anyhow::Context;
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(ca) = ca {
        for c in CertificateDer::pem_file_iter(ca).with_context(|| format!("{}: reading CA certificates", ca.display()))? {
            roots.add(c.with_context(|| format!("{}: bad CA certificate", ca.display()))?)?;
        }
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_root_certificates(roots).with_no_client_auth())
}

fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(s), Some(at)) if at > s => format!("{}://…@{}", &url[..s], &url[at + 1..]),
        _ => url.to_string(),
    }
}

#[async_trait]
impl MetaStore for PgMeta {
    async fn get(&self, kind: &str, key: &str) -> anyhow::Result<Option<Value>> {
        let c = self.client().await?;
        Ok(c.query_opt("SELECT value FROM mq_meta WHERE kind = $1 AND key = $2", &[&kind, &key]).await?.map(|r| r.get(0)))
    }

    async fn put(&self, kind: &str, key: &str, value: &Value) -> anyhow::Result<i64> {
        Ok(self.write(kind, key, Some(value), None).await?.unwrap_or(0))
    }

    async fn delete(&self, kind: &str, key: &str) -> anyhow::Result<()> {
        self.write(kind, key, None, None).await?;
        Ok(())
    }

    async fn list(&self, kind: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let c = self.client().await?;
        Ok(c.query("SELECT key, value FROM mq_meta WHERE kind = $1 ORDER BY key", &[&kind]).await?.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn list_vhost(&self, kind: &str, vhost: &str) -> anyhow::Result<Vec<(String, Value)>> {
        // Matches the `mq_meta_vhost` index; unlike a key range it doesn't depend on collation.
        let c = self.client().await?;
        let rows = c.query("SELECT key, value FROM mq_meta WHERE kind = $1 AND split_part(key, E'\\x1f', 1) = $2 ORDER BY key", &[&kind, &vhost]).await?;
        Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn cas(&self, kind: &str, key: &str, expected: Option<i64>, value: &Value) -> anyhow::Result<Option<i64>> {
        self.write(kind, key, Some(value), Some(expected)).await
    }

    async fn get_versioned(&self, kind: &str, key: &str) -> anyhow::Result<Option<(Value, i64)>> {
        let c = self.client().await?;
        Ok(c.query_opt("SELECT value, version FROM mq_meta WHERE kind = $1 AND key = $2", &[&kind, &key]).await?.map(|r| (r.get(0), r.get(1))))
    }

    async fn changes_since(&self, version: i64) -> anyhow::Result<(Vec<Change>, i64)> {
        let c = self.client().await?;
        let latest: i64 = c.query_one("SELECT version FROM mq_meta_version WHERE id = 1", &[]).await?.get(0);
        if version >= latest {
            return Ok((vec![], latest));
        }
        let rows = c.query("SELECT version, kind, key, value FROM mq_meta_log WHERE version > $1 ORDER BY version LIMIT 10000", &[&version]).await?;
        let changes: Vec<Change> = rows.into_iter().map(|r| Change { version: r.get(0), kind: r.get(1), key: r.get(2), value: r.get(3) }).collect();
        let upto = changes.last().map(|c| c.version).unwrap_or(latest);
        Ok((changes, upto))
    }

    fn describe(&self) -> String {
        format!("postgres {}", self.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A fake server that records how the client starts: the SSLRequest, then (over TLS when
    /// it said yes) the startup message.
    async fn fake_server(tls: Option<tokio_rustls::TlsAcceptor>) -> (u16, tokio::task::JoinHandle<(bool, Vec<u8>)>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut head = [0u8; 8];
            s.read_exact(&mut head).await.unwrap();
            let ssl_request = head == [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];
            if !ssl_request {
                return (false, head.to_vec());
            }
            match tls {
                Some(a) => {
                    s.write_all(b"S").await.unwrap();
                    let Ok(mut t) = a.accept(s).await else { return (true, Vec::new()) };
                    let mut startup = vec![0u8; 256];
                    let n = t.read(&mut startup).await.unwrap_or(0);
                    startup.truncate(n);
                    (true, startup)
                }
                None => {
                    s.write_all(b"N").await.unwrap();
                    let mut startup = vec![0u8; 256];
                    let n = s.read(&mut startup).await.unwrap_or(0);
                    startup.truncate(n);
                    (true, startup)
                }
            }
        });
        (port, task)
    }

    fn ca_and_acceptor(dir: &Path) -> (std::path::PathBuf, tokio_rustls::TlsAcceptor) {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(rcgen::CertificateParams::new(Vec::<String>::new()).unwrap(), &ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap().signed_by(&key, &issuer).unwrap();
        let ca_file = dir.join("ca.pem");
        std::fs::write(&ca_file, ca.pem()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap())
            .unwrap();
        (ca_file, tokio_rustls::TlsAcceptor::from(Arc::new(server)))
    }

    fn sent_user(startup: &[u8]) -> bool {
        startup.windows(14).any(|w| w == b"user\0rollmeta\0")
    }

    #[tokio::test]
    async fn tls_follows_sslmode_and_verifies_the_server() {
        let dir = std::env::temp_dir().join(format!("roll-pgmeta-{}", mq_core::rand_u64()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ca, acceptor) = ca_and_acceptor(&dir);
        let url = |port: u16, mode: &str| format!("postgres://rollmeta:pw@localhost:{port}/meta?sslmode={mode}&connect_timeout=2");

        // require + trusted CA: the startup message travels inside TLS.
        let (port, server) = fake_server(Some(acceptor.clone())).await;
        let _ = PgMeta::connect_with(&url(port, "require"), Some(&ca)).await;
        let (asked, startup) = server.await.unwrap();
        assert!(asked && sent_user(&startup), "startup over TLS: {startup:?}");

        // An untrusted server certificate is refused before anything is sent.
        let (port, server) = fake_server(Some(acceptor)).await;
        assert!(PgMeta::connect_with(&url(port, "require"), None).await.is_err());
        let (asked, startup) = server.await.unwrap();
        assert!(asked && startup.is_empty(), "nothing sent to an unverified server");

        // prefer falls back to plain text when the server has no TLS (roll postgres locally).
        let (port, server) = fake_server(None).await;
        let _ = PgMeta::connect_with(&url(port, "prefer"), Some(&ca)).await;
        let (asked, startup) = server.await.unwrap();
        assert!(asked && sent_user(&startup));

        // disable never asks.
        let (port, server) = fake_server(None).await;
        let _ = PgMeta::connect_with(&url(port, "disable"), None).await;
        let (asked, _) = server.await.unwrap();
        assert!(!asked);
        let _ = std::fs::remove_dir_all(dir);
    }
}
