//! roll search inside rolld: the Elasticsearch REST listener and the `/v1/search` control API.
//!
//! The node keeps segments, the WAL and cluster metadata in the same object storage as the rest
//! of rolld (`--s3-bucket`, or a directory under `--home`); local disk under
//! `<home>/search/cache` is only a cache.

use clap::Args;
use pg_storage::StorageConfig;
use search_control::{Control, SearchConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[derive(Args, Clone, Debug)]
pub struct SearchArgs {
    /// Disable roll search.
    #[arg(long = "no-search", env = "ROLL_SEARCH_DISABLED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_disabled: bool,
    /// Elasticsearch REST (HTTP/HTTPS) listener.
    #[arg(long = "search-http", env = "ROLL_SEARCH_HTTP", default_value = "127.0.0.1:59200")]
    pub search_http: SocketAddr,
    /// Serve HTTPS with this PEM certificate chain (needs --search-tls-key). The TLS server name
    /// can select the cluster (`<cluster>.<--search-domain>`).
    #[arg(long = "search-tls-cert", env = "ROLL_SEARCH_TLS_CERT", requires = "search_tls_key")]
    pub search_tls_cert: Option<PathBuf>,
    #[arg(long = "search-tls-key", env = "ROLL_SEARCH_TLS_KEY", requires = "search_tls_cert")]
    pub search_tls_key: Option<PathBuf>,
    /// Clusters are reached at `<cluster>.<domain>` (e.g. `search.example.com`): SNI or `Host`
    /// routing, and the host in cluster URLs.
    #[arg(long = "search-domain", env = "ROLL_SEARCH_DOMAIN")]
    pub search_domain: Option<String>,
    /// Unload clusters idle this long, in seconds (a cluster's own idle timeout wins).
    #[arg(long = "search-idle-unload", env = "ROLL_SEARCH_IDLE_UNLOAD", default_value_t = 300)]
    pub search_idle_unload_s: u64,
    /// Local disk cache for segments, in bytes.
    #[arg(long = "search-cache-bytes", env = "ROLL_SEARCH_CACHE_BYTES", default_value_t = 10 << 30)]
    pub search_cache_bytes: u64,
    /// Acknowledge writes once the WAL is in object storage (for nodes with ephemeral disks).
    #[arg(long = "search-sync-objects", env = "ROLL_SEARCH_SYNC_OBJECTS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_sync_objects: bool,
    /// Expect PROXY protocol headers on the REST port (behind an AWS NLB or HAProxy).
    #[arg(long = "search-proxy-protocol", env = "ROLL_SEARCH_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_proxy_protocol: bool,
    /// Point-in-time branching window, in seconds (0 disables it).
    #[arg(long = "search-history", env = "ROLL_SEARCH_HISTORY", default_value_t = 86_400)]
    pub search_history_s: u64,
    /// Collect unreferenced segments and WAL files every this many seconds (0: never). Run it on
    /// one node per bucket.
    #[arg(long = "search-gc-interval", env = "ROLL_SEARCH_GC_INTERVAL", default_value_t = 3600)]
    pub search_gc_interval_s: u64,
    /// Publish Elasticsearch's test cluster features (to run the YAML REST suites against
    /// clusters created through the API). Test deployments only.
    #[arg(long = "search-test-features", env = "ROLL_SEARCH_TEST_FEATURES", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_test_features: bool,
}

pub struct Search {
    pub control: Arc<Control>,
    listener: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
    opts: search_server::ListenOptions,
}

impl Search {
    pub async fn start(home: &Path, args: &SearchArgs, storage: &StorageConfig, public_host: &str, node: &str) -> anyhow::Result<Self> {
        let mut cfg = SearchConfig::local(home.join("search"));
        cfg.node_name = node.to_string();
        cfg.public_host = public_host.to_string();
        cfg.http_port = args.search_http.port();
        cfg.domain = args.search_domain.clone().filter(|d| !d.is_empty());
        cfg.idle_unload = Duration::from_secs(args.search_idle_unload_s.max(1));
        cfg.defaults.idle_timeout_s = args.search_idle_unload_s as i64;
        cfg.cache_bytes = args.search_cache_bytes;
        cfg.sync_objects = args.search_sync_objects;
        cfg.history_retention = Duration::from_secs(args.search_history_s);
        cfg.test_features = args.search_test_features;
        cfg.gc_interval = (args.search_gc_interval_s > 0).then(|| Duration::from_secs(args.search_gc_interval_s));
        let mut opts = search_server::ListenOptions { proxy_protocol: args.search_proxy_protocol, ..Default::default() };
        if let (Some(c), Some(k)) = (&args.search_tls_cert, &args.search_tls_key) {
            opts.tls = Some(search_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?);
        }
        cfg.tls = opts.tls.is_some();
        let control = Control::new(cfg, search_store::Objects::new(storage)?).await?;
        let listener = tokio::net::TcpListener::bind(args.search_http).await.map_err(|e| anyhow::anyhow!("bind {}: {e}", args.search_http))?;
        info!(addr = %args.search_http, tls = control.cfg.tls, domain = ?control.cfg.domain, storage = %control.objects().description, "search listening");
        Ok(Search { control, listener: tokio::sync::Mutex::new(Some(listener)), opts })
    }

    pub fn router(&self) -> axum::Router {
        search_control::api::router(self.control.clone())
    }

    /// Serves the REST listener until it fails.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = self.listener.lock().await.take().ok_or_else(|| anyhow::anyhow!("search listener already serving"))?;
        search_server::serve(listener, self.control.node.clone(), self.opts.clone()).await
    }

    /// Unloads every cluster (accounting usage) and ships the WAL to object storage.
    pub async fn shutdown(&self) {
        if let Err(e) = self.control.shutdown().await {
            tracing::warn!(error = %e, "search shutdown");
        }
    }
}
