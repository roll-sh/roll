//! roll storage: the S3-compatible HTTP API.
//!
//! [`Server`] answers S3 requests over [`s3_store::Store`]: SigV4 auth with roll access keys,
//! path- and virtual-host-style addressing, streaming bodies both ways (bounded memory),
//! checksums verified before anything is committed, CORS, public buckets and metering. The wire
//! format (signatures, `aws-chunked`, XML, error codes) is `s3-proto`'s.
//!
//! ```ignore
//! let server = Server::new(store, Config::default());
//! s3_server::serve(tokio::net::TcpListener::bind("127.0.0.1:59000").await?, server).await?;
//! ```

mod access;
mod acl;
mod auth;
mod body;
mod bucket;
mod config;
mod cors;
mod ctx;
mod error;
mod logging;
mod multipart;
mod object;
mod policy;
mod post;
mod response;
mod upload;
mod website;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use ipnet::IpNet;
use s3_store::Store;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

pub use error::store_error;
pub use upload::put_object;

#[derive(Clone, Debug)]
pub struct Config {
    /// `<bucket>.<domain>` addresses a bucket (virtual-host style). Path style always works.
    pub virtual_host_domain: Option<String>,
    /// Peers in these ranges are inside roll: their egress is free.
    pub internal_cidrs: Vec<IpNet>,
    /// Region reported by `GetBucketLocation` and `x-amz-bucket-region` (any region is accepted
    /// in signatures).
    pub region: String,
    /// Where browser form uploads (POST Object, length unknown up front) are spooled.
    pub spool_dir: PathBuf,
    /// `<bucket>.<domain>` is the bucket's static website endpoint (S3 website semantics).
    pub website_domain: Option<String>,
    /// How often buffered server access logs are written to their target buckets; `None` =
    /// only by [`Server::flush_access_logs`].
    pub access_log_interval: Option<std::time::Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            virtual_host_domain: None,
            internal_cidrs: default_internal_cidrs(),
            region: "us-east-1".into(),
            spool_dir: std::env::temp_dir(),
            website_domain: None,
            access_log_interval: Some(std::time::Duration::from_secs(300)),
        }
    }
}

/// Loopback, the private IPv4 ranges (VPCs, the docker bridge) and IPv6 ULAs.
pub fn default_internal_cidrs() -> Vec<IpNet> {
    ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"].iter().map(|s| s.parse().expect("valid CIDR")).collect()
}

/// Parse a comma-separated CIDR list (`ROLL_S3_INTERNAL_CIDRS`).
pub fn parse_cidrs(s: &str) -> anyhow::Result<Vec<IpNet>> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(|p| p.parse::<IpNet>().map_err(|e| anyhow::anyhow!("{p}: {e}"))).collect()
}

pub struct Server {
    pub store: Arc<Store>,
    pub cfg: Config,
    logs: logging::Logs,
}

impl Server {
    pub fn new(store: Arc<Store>, cfg: Config) -> Arc<Self> {
        let server = Arc::new(Self { store, cfg, logs: Default::default() });
        if let (Some(every), Ok(rt)) = (server.cfg.access_log_interval, tokio::runtime::Handle::try_current()) {
            let weak = Arc::downgrade(&server);
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(s) = weak.upgrade() else { return };
                    s.flush_access_logs().await;
                }
            });
        }
        server
    }

    /// Write the buffered server access logs to their target buckets now (they're otherwise
    /// written every `access_log_interval`). Returns how many log objects were written.
    pub async fn flush_access_logs(&self) -> usize {
        logging::flush(self).await
    }

    /// Access log lines dropped because the buffer was full.
    pub fn access_logs_dropped(&self) -> u64 {
        self.logs.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether traffic to `ip` is inside roll (free egress).
    pub fn is_internal(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        self.cfg.internal_cidrs.iter().any(|n| n.contains(&ip))
    }
}

/// The S3 API as an axum router. Serve it with connect info (see [`serve`]): egress is
/// classified by the peer address.
pub fn router(server: Arc<Server>) -> Router {
    Router::new().fallback(handle).with_state(server)
}

/// Serve the S3 API on `listener` until it fails.
pub async fn serve(listener: tokio::net::TcpListener, server: Arc<Server>) -> std::io::Result<()> {
    axum::serve(listener, router(server).into_make_service_with_connect_info::<SocketAddr>()).await
}

async fn handle(State(server): State<Arc<Server>>, req: Request) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    ctx::dispatch(server, peer, req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_lists() {
        assert_eq!(parse_cidrs(" 10.0.0.0/8, ::1/128 ,,").unwrap().len(), 2);
        assert!(parse_cidrs("").unwrap().is_empty());
        assert!(parse_cidrs("10.0.0.0/33").is_err());
        assert!(parse_cidrs("example.com").is_err());
    }

    #[tokio::test]
    async fn internal_peers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(s3_store::StoreConfig::new(dir.path().join("meta")).manual(), s3_store::test_backend("s3-server-lib").await).await.unwrap();
        let server = Server::new(store.clone(), Config::default());
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for inside in ["127.0.0.1", "10.1.2.3", "172.17.0.2", "172.31.255.255", "192.168.1.1", "::1", "fd00::1", "::ffff:10.1.2.3", "::ffff:127.0.0.1"] {
            assert!(server.is_internal(ip(inside)), "{inside}");
        }
        for outside in ["8.8.8.8", "172.32.0.1", "100.64.0.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(!server.is_internal(ip(outside)), "{outside}");
        }
        let none = Server::new(store, Config { internal_cidrs: Vec::new(), ..Config::default() });
        assert!(!none.is_internal(ip("127.0.0.1")));
    }
}
