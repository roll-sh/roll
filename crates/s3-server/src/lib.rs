//! roll storage: the S3-compatible HTTP API.
//!
//! [`Server`] answers S3 requests over [`s3_store::Store`]: SigV4, SigV4a and SigV2 auth with
//! roll access keys, SSE-C and SSE-KMS passed through to the backing object store,
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
mod append;
mod attributes;
mod auth;
mod body;
mod bucket;
mod config;
mod cors;
mod ctx;
mod directory;
mod error;
mod inventory;
mod lifecycle;
mod logging;
mod multipart;
mod notification;
pub mod notify;
mod object;
mod owner;
mod policy;
mod post;
mod rename;
mod replication;
mod response;
mod select;
mod sse;
mod sts;
mod temporary;
mod torrent;
mod upload;
mod versioning;
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
pub use notify::{Delivery, EventSink, EventStats, Notifier, NotifyConfig, NotifyError, RollDestination};
pub use sts::oidc::{HttpFetch, JwksFetch};
pub use upload::put_object;

/// Check an IAM identity policy (a role's inline policy, or a session policy) of at most `max`
/// bytes, and return it minified. `Err` says what's wrong.
pub fn validate_identity_policy(doc: &str, max: usize) -> Result<String, String> {
    policy::Policy::parse_identity(doc.as_bytes(), max).map_err(|e| e.message)?;
    serde_json::from_str::<serde_json::Value>(doc).map(|v| v.to_string()).map_err(|e| e.to_string())
}

/// Check a bucket policy for `bucket` as PutBucketPolicy does. `Ok`: whether it's public (grants
/// anonymous access); `Err` says what's wrong (`MalformedPolicy`'s message).
pub fn validate_bucket_policy(doc: &str, bucket: &str) -> Result<bool, String> {
    if doc.len() > policy::MAX_POLICY_BYTES {
        return Err("Policy exceeds the maximum allowed document size.".into());
    }
    policy::Policy::parse(doc.as_bytes(), bucket).map(|p| p.is_public()).map_err(|e| e.message)
}

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
    /// Event notifications, replication and inventory reports.
    pub notify: NotifyConfig,
    /// Fetch OIDC providers' keys (`AssumeRoleWithWebIdentity`) over plain HTTP too. Tests
    /// only: in production the keys must come over HTTPS.
    pub oidc_allow_http: bool,
    /// The tracker GetObjectTorrent's files announce to (`ROLL_S3_TORRENT_ANNOUNCE`); `None`:
    /// none (trackerless).
    pub torrent_announce: Option<String>,
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
            notify: NotifyConfig::default(),
            oidc_allow_http: false,
            torrent_announce: None,
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
    /// Delivers events and validates notification destinations.
    pub notifier: Arc<Notifier>,
    /// Verifies web identity tokens (STS `AssumeRoleWithWebIdentity`).
    pub(crate) oidc: sts::oidc::Oidc,
    /// RenameObject's recent client tokens (idempotent retries).
    pub(crate) renames: rename::Tokens,
}

impl Server {
    pub fn new(store: Arc<Store>, cfg: Config) -> Arc<Self> {
        let notifier = Notifier::new(store.clone(), cfg.notify.clone(), cfg.region.clone());
        notifier.spawn();
        let oidc = sts::oidc::Oidc::new(Arc::new(HttpFetch::new(cfg.oidc_allow_http)));
        let server = Arc::new(Self { store, cfg, logs: Default::default(), notifier, oidc, renames: Default::default() });
        if let (Some(every), Ok(rt)) = (server.cfg.notify.inventory_interval, tokio::runtime::Handle::try_current()) {
            let weak = Arc::downgrade(&server);
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(s) = weak.upgrade() else { return };
                    inventory::run(&s, false).await;
                }
            });
        }
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

    /// Write the inventory reports that are due now (all enabled ones, with `force`). They're
    /// otherwise checked every `notify.inventory_interval`. Returns how many were written.
    pub async fn run_inventory(&self, force: bool) -> usize {
        inventory::run(self, force).await
    }

    /// Fetch OIDC providers' keys with `fetch` from now on (tests serve their own keys).
    pub fn set_jwks_fetcher(&self, fetch: Arc<dyn JwksFetch>) {
        self.oidc.set_fetch(fetch);
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
