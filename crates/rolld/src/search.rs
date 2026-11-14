//! roll search inside rolld: the Elasticsearch REST listener and the `/v1/search` control API.
//!
//! The node keeps segments, the WAL and cluster metadata in the same object storage as the rest
//! of rolld (`--s3-bucket`, or a directory under `--home`); local disk under
//! `<home>/search/cache` is only a cache. Documents are durable: writes are acknowledged once
//! the WAL holds them (local disk, or object storage with `--search-sync-objects`), and a
//! cluster that unloads is flushed first.
//!
//! On a cluster (a roll-cluster backend other than memory) every search cluster is served by the
//! holder of its lease `search:cluster:<name>` (`search_control::cluster`): the catalog is in the
//! system database (`ROLL_SYSTEM_DATABASE`, schema `search`), writes are acknowledged once their
//! WAL batch is in object storage, and this node's REST port runs each request on its cluster's
//! owner. `/v1/search` is served on rolld's API port with every other product's routes, behind
//! its operator gate (auth.rs).

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
    /// Run only roll search (`ROLL_PRODUCTS=search`).
    #[arg(long = "search-only", env = "ROLL_SEARCH_ONLY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_only: bool,
    /// Elasticsearch REST (HTTP/HTTPS) listener.
    #[arg(long = "search-http", env = "ROLL_SEARCH_HTTP", default_value = "127.0.0.1:59200")]
    pub search_http: SocketAddr,
    /// Serve HTTPS with this PEM certificate chain (needs --search-tls-key). The TLS server name
    /// can select the cluster (`<cluster>.<--search-domain>`). A single node only: on a cluster
    /// TLS ends at the edge.
    #[arg(long = "search-tls-cert", env = "ROLL_SEARCH_TLS_CERT", requires = "search_tls_key")]
    pub search_tls_cert: Option<PathBuf>,
    #[arg(long = "search-tls-key", env = "ROLL_SEARCH_TLS_KEY", requires = "search_tls_cert")]
    pub search_tls_key: Option<PathBuf>,
    /// Clusters are reached at `<cluster>.<domain>` (e.g. `search.example.com`): SNI or `Host`
    /// routing, and the host in cluster URLs.
    #[arg(long = "search-domain", env = "ROLL_SEARCH_DOMAIN")]
    pub search_domain: Option<String>,
    /// The URL clients reach clusters at when something sits in front of this node (a load
    /// balancer): cluster URLs use its scheme, host and port.
    #[arg(long = "search-public-url", env = "ROLL_SEARCH_PUBLIC_URL")]
    pub search_public_url: Option<String>,
    /// Unload clusters idle this long, in seconds (a cluster's own idle timeout wins).
    #[arg(long = "search-idle-unload", env = "ROLL_SEARCH_IDLE_UNLOAD", default_value_t = 300)]
    pub search_idle_unload_s: u64,
    /// Local disk cache for segments, in bytes.
    #[arg(long = "search-cache-bytes", env = "ROLL_SEARCH_CACHE_BYTES", default_value_t = 10 << 30)]
    pub search_cache_bytes: u64,
    /// Memory all loaded clusters may hold together, in MiB (0: no budget). Past it the least
    /// recently used idle clusters unload.
    #[arg(long = "search-memory-mb", env = "ROLL_SEARCH_MEMORY_MB", default_value_t = 0)]
    pub search_memory_mb: u64,
    /// Acknowledge writes once the WAL is in object storage (for nodes with ephemeral disks;
    /// always on in a cluster).
    #[arg(long = "search-sync-objects", env = "ROLL_SEARCH_SYNC_OBJECTS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_sync_objects: bool,
    /// Keep search data in this directory instead of rolld's object storage (tests).
    #[arg(long = "search-storage-dir", env = "ROLL_SEARCH_STORAGE_DIR")]
    pub search_storage_dir: Option<PathBuf>,
    /// Keep the WAL in this S3 Express One Zone directory bucket (`name--azid--x-s3`): acks in
    /// single-digit milliseconds instead of S3 Standard's tens.
    #[arg(long = "search-wal-bucket", env = "ROLL_SEARCH_WAL_BUCKET")]
    pub search_wal_bucket: Option<String>,
    /// Expect PROXY protocol headers on the REST port (behind an AWS NLB or HAProxy).
    #[arg(long = "search-proxy-protocol", env = "ROLL_SEARCH_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_proxy_protocol: bool,
    /// Point-in-time branching window, in seconds (0 disables it).
    #[arg(long = "search-history", env = "ROLL_SEARCH_HISTORY", default_value_t = 86_400)]
    pub search_history_s: u64,
    /// Collect unreferenced segments and WAL files every this many seconds (0: never). In a
    /// cluster, one node at a time runs it.
    #[arg(long = "search-gc-interval", env = "ROLL_SEARCH_GC_INTERVAL", default_value_t = 3600)]
    pub search_gc_interval_s: u64,
    /// The name of every cluster's default snapshot repository, on roll's own object storage
    /// (empty: none).
    #[arg(long = "search-default-repository", env = "ROLL_SEARCH_DEFAULT_REPOSITORY", default_value = "found-snapshots")]
    pub search_default_repository: String,
    /// Let `s3` snapshot repositories reach private addresses (local development with MinIO).
    #[arg(long = "search-repository-allow-private", env = "ROLL_SEARCH_REPOSITORY_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_repository_allow_private: bool,
    /// Publish Elasticsearch's test cluster features (to run the YAML REST suites against
    /// clusters created through the API). Test deployments only.
    #[arg(long = "search-test-features", env = "ROLL_SEARCH_TEST_FEATURES", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_test_features: bool,
}

pub struct Search {
    pub control: Arc<Control>,
    /// The roll-cluster fleet clusters are served in (`None`: this node alone owns them all).
    cluster: Option<Arc<roll_cluster::Cluster>>,
    listener: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
    opts: search_server::ListenOptions,
    stop: search_server::Shutdown,
}

/// How long a stop may take to flush what's still loaded (a drain has handed the rest over).
const SHUTDOWN: Duration = Duration::from_secs(60);

impl Search {
    /// `cluster`: the node's roll-cluster handle. On a cluster backend other than memory every
    /// cluster is served by the holder of its lease (`search_control::cluster`), with the catalog
    /// in the system database and acknowledgements once writes are in object storage.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(home: &Path, args: &SearchArgs, storage: &StorageConfig, public_host: &str, node: &str, cluster: &Arc<roll_cluster::Cluster>, system_database: Option<&str>, db_connections: usize, public: Option<&crate::edge::Public>) -> anyhow::Result<Self> {
        let mut cfg = SearchConfig::local(home.join("search"));
        let fleet = (cluster.backend_name() != "memory").then(|| cluster.clone());
        cfg.node_name = node.to_string();
        if fleet.is_some() {
            anyhow::ensure!(!matches!(storage, StorageConfig::Local(_)) || args.search_storage_dir.is_some(), "search in a cluster needs S3 (--s3-bucket): the other nodes can't read a local directory");
            let Some(db) = system_database.filter(|u| !u.is_empty()) else {
                anyhow::bail!("search in a cluster needs the system database (ROLL_SYSTEM_DATABASE): every node must see the same catalog");
            };
            cfg.catalog = Some(db.to_string());
            cfg.catalog_connections = db_connections;
            // Another node replays this one's WAL after a takeover.
            cfg.sync_objects = true;
            // WAL objects are named by the node: the cluster's node id is unique per process.
            cfg.node_name = cluster.node_id().to_string();
        }
        cfg.public_host = public_host.to_string();
        cfg.http_port = args.search_http.port();
        cfg.domain = args.search_domain.clone().filter(|d| !d.is_empty());
        cfg.public_url = args.search_public_url.clone().filter(|u| !u.is_empty());
        cfg.idle_unload = Duration::from_secs(args.search_idle_unload_s.max(1));
        cfg.defaults.idle_timeout_s = args.search_idle_unload_s as i64;
        cfg.cache_bytes = args.search_cache_bytes;
        cfg.memory_budget = (args.search_memory_mb > 0).then_some(args.search_memory_mb << 20);
        cfg.sync_objects |= args.search_sync_objects;
        cfg.history_retention = Duration::from_secs(args.search_history_s);
        cfg.test_features = args.search_test_features;
        cfg.gc_interval = (args.search_gc_interval_s > 0).then(|| Duration::from_secs(args.search_gc_interval_s));
        cfg.default_repository = Some(args.search_default_repository.trim().to_string()).filter(|n| !n.is_empty());
        if args.search_repository_allow_private {
            cfg.node_settings.push((search_control::S3_ALLOW_PRIVATE.to_string(), "true".to_string()));
        }
        // Watch passwords, connector and Fleet secrets are sealed in cluster state under the
        // deployment's key; without one they're stored as given (as on Elasticsearch).
        cfg.sealer = std::env::var("ROLL_SECRET_KEY").ok().and_then(|k| search_control::Sealer::from_secret(&k));
        // Behind the edge, cluster URLs name `<cluster>.search.<region>.<domain>` over HTTPS.
        if let Some(p) = public {
            p.search(&mut cfg);
        }
        let mut opts = search_server::ListenOptions { proxy_protocol: args.search_proxy_protocol, ..Default::default() };
        if let (Some(c), Some(k)) = (&args.search_tls_cert, &args.search_tls_key) {
            anyhow::ensure!(fleet.is_none(), "--search-tls-cert is for a single node: in a cluster TLS ends at the edge");
            opts.tls = Some(search_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?);
        }
        cfg.tls = opts.tls.is_some();
        let wal = match args.search_wal_bucket.as_deref().filter(|b| !b.is_empty()) {
            Some(bucket) => {
                let region = match storage {
                    StorageConfig::S3 { region, .. } => region.clone(),
                    StorageConfig::Local(_) => std::env::var("AWS_REGION").ok(),
                };
                Some(search_store::Objects::new(&StorageConfig::S3 { bucket: bucket.to_string(), region, endpoint: None, prefix: Some("search-wal".into()) })?)
            }
            None => None,
        };
        let objects = match &args.search_storage_dir {
            Some(dir) => search_store::Objects::new(&StorageConfig::Local(dir.clone()))?,
            None => search_store::Objects::new(storage)?,
        };
        let control = Control::open(cfg, objects, wal).await?;
        if let Some(c) = &fleet {
            control.join(c.clone())?;
        }
        let listener = tokio::net::TcpListener::bind(args.search_http).await.map_err(|e| anyhow::anyhow!("bind {}: {e}", args.search_http))?;
        info!(addr = %args.search_http, tls = control.cfg.tls, domain = ?control.cfg.domain, storage = %control.objects().description, clustered = fleet.is_some(), "search listening");
        Ok(Search { control, cluster: fleet, listener: tokio::sync::Mutex::new(Some(listener)), opts, stop: search_server::Shutdown::new() })
    }

    pub fn router(&self) -> axum::Router {
        search_control::api::router(self.control.clone())
    }

    /// The Elasticsearch REST API for the edge's `<cluster>.search.…` hostnames: each request
    /// runs on its cluster's owner (here on a node alone).
    pub fn rest_router(&self) -> axum::Router {
        search_control::cluster::rest_router(self.control.clone())
    }

    /// The internal route REST requests other nodes forward come in on (their cluster is here).
    pub fn internal(&self, router: axum::Router) -> axum::Router {
        match &self.cluster {
            Some(c) => router.merge(search_control::cluster::internal_router(self.control.clone(), c.clone())),
            None => router,
        }
    }

    /// Serves the REST listener until it fails or [`Search::shutdown`] stops it.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = self.listener.lock().await.take().ok_or_else(|| anyhow::anyhow!("search listener already serving"))?;
        // On a cluster each request runs on its cluster's owner: the front names the cluster and
        // dispatches it (`search_control::cluster`).
        if self.cluster.is_some() {
            let front = search_control::cluster::rest_router(self.control.clone());
            let stop = self.stop.clone();
            axum::serve(listener, front).with_graceful_shutdown(async move { stop.wait().await }).await?;
            return Ok(());
        }
        search_server::serve_until(listener, self.control.node.clone(), self.opts.clone(), self.stop.clone()).await
    }

    /// Unloads every cluster still loaded here (flushing it and giving its lease back),
    /// accounts usage and makes the WAL durable, then stops the REST listener.
    pub async fn shutdown(&self) {
        if let Err(e) = self.control.shutdown_within(SHUTDOWN).await {
            tracing::warn!(error = %e, "search shutdown");
        }
        self.stop.trigger();
    }
}
