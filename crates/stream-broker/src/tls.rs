//! TLS for the Kafka listener (`SASL_SSL` / `SSL` in client terms).

use crate::config::TlsFiles;
use anyhow::Context;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub fn acceptor(files: &TlsFiles) -> anyhow::Result<TlsAcceptor> {
    let certs = load_certs(&std::fs::read(&files.cert).with_context(|| format!("reading {}", files.cert.display()))?)?;
    let key = load_key(&std::fs::read(&files.key).with_context(|| format!("reading {}", files.key.display()))?)?;
    acceptor_from(certs, key)
}

pub fn load_certs(pem: &[u8]) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut &pem[..]).collect::<Result<_, _>>()?;
    anyhow::ensure!(!certs.is_empty(), "no certificates in PEM");
    Ok(certs)
}

pub fn load_key(pem: &[u8]) -> anyhow::Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut &pem[..])?.ok_or_else(|| anyhow::anyhow!("no private key in PEM"))
}

pub fn acceptor_from(certs: Vec<CertificateDer<'static>>, key: PrivateKeyDer<'static>) -> anyhow::Result<TlsAcceptor> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
