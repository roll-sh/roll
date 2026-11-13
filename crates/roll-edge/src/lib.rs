//! roll cloud's edge (docs/cloud/README.md, "The edge"): every public listener of a `rolld` node.
//!
//! - [`proxy_protocol`]: the client's address from the NLB's PROXY protocol header (v1 and v2).
//! - [`tls`]: rustls with an SNI certificate resolver over a [`tls::CertStore`] that reloads
//!   without a restart, and the dev-mode certificates.
//! - [`hosts`]: which product and tenant a hostname names (the Hostnames table).
//! - [`listener`]: the accept loop every TCP listener shares (PROXY header, then TLS).
//! - [`pg`]: Postgres' SSLRequest and direct TLS (`sslnegotiation=direct`).
//! - [`http`]: the HTTPS listener, routing by Host to routers and reverse proxies.
//! - [`certs`] and [`acme`]: wildcard certificates over ACME DNS-01 (Cloudflare), sealed in
//!   object storage, issued by one node at a time and reloaded by all.
//! - [`domains`]: custom domains' certificates over ACME HTTP-01 on a node alone, for the
//!   hostnames a [`domains::CustomDomains`] names; [`saas`]: Cloudflare for SaaS custom
//!   hostnames on the fleet.
//! - [`splice`]: hand a client connection to the node that owns its tenant.
//! - [`drain`]: `GET /healthz` and the draining state a stopping node shares.

pub mod acme;
pub mod certs;
pub mod domains;
pub mod drain;
pub mod hosts;
pub mod http;
pub mod listener;
pub mod pg;
pub mod proxy_protocol;
pub mod saas;
pub mod seal;
pub mod splice;
pub mod stream;
pub mod tls;
pub mod trusted;

pub use drain::Drain;
pub use hosts::{Host, Hostnames};
pub use listener::Accepted;
pub use tls::CertStore;

/// A byte stream a product can serve: a TLS stream, a splice, a socket.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static> Io for T {}

/// Make ring the process's rustls provider, for clients built with `ClientConfig::builder()`
/// (the ACME client's). Harmless when one is installed already.
pub fn install_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}
