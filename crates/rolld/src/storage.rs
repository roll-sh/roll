//! roll storage in rolld: the S3 API (`--s3`), the developer API (`--storage-api`) and the
//! control plane (`/v1/storage` on the API port), on every node.
//!
//! The deployment's metadata has one owner at a time, the holder of the lease `storage:meta`
//! (`s3_server::cluster`, docs/storage/architecture.md "Many nodes"). Every node binds the
//! listeners and hands each request to the owner; the owner builds the S3 server, the control
//! plane and the developer API on the store it took over, with the storage events sink into roll
//! queues and streams (`storage_events`). On a node alone (no cluster) it owns the metadata from
//! the start, as rolld always did.

use pg_storage::StorageConfig;
use s3_server::cluster::{NodeConfig, Services, StorageNode, Surface};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;

use crate::Args;

/// roll storage on this node, its listeners bound.
pub struct Storage {
    pub node: Arc<StorageNode>,
    listeners: Option<(tokio::net::TcpListener, tokio::net::TcpListener)>,
}

impl Storage {
    /// The S3 API's router (any node; requests run on the metadata's owner): for the edge.
    pub fn s3_router(&self) -> axum::Router {
        self.node.router(Surface::S3)
    }

    /// The control plane (`/v1/storage`) and the routes other nodes forward requests to, for
    /// the API router.
    pub fn api_routes(&self) -> axum::Router {
        self.node.control_routes().merge(self.node.internal_routes())
    }

    /// Serving the S3 API and the developer API, until either fails (once; later calls never
    /// finish).
    pub fn serve(&mut self) -> impl std::future::Future<Output = std::io::Result<()>> + Send + 'static {
        let listeners = self.listeners.take();
        let s3_router = self.node.router(Surface::S3).into_make_service_with_connect_info::<SocketAddr>();
        let api_router = self.node.router(Surface::Api).into_make_service_with_connect_info::<SocketAddr>();
        async move {
            let Some((s3, api)) = listeners else { return std::future::pending().await };
            tokio::select! {
                r = axum::serve(s3, s3_router) => r,
                r = axum::serve(api, api_router) => r,
            }
        }
    }

    /// Commit pending metadata and usage before exiting.
    pub async fn shutdown(&self) {
        self.node.shutdown().await;
    }
}

/// rolld's storage flags (taken before the other products consume theirs).
pub struct StorageOpts {
    s3: SocketAddr,
    public_host: String,
    node: String,
    s3_domain: Option<String>,
    s3_website_domain: Option<String>,
    s3_block_public_access: bool,
    s3_storage_class: Option<String>,
    s3_internal_cidrs: Option<String>,
    s3_kms: Option<bool>,
    s3_sse_s3_at_rest: Option<bool>,
    s3_access_key: Option<String>,
    s3_secret_key: Option<String>,
    s3_notify_aws: bool,
    s3_notify_aws_endpoint: Option<String>,
    s3_notify_aws_endpoints: Option<String>,
    s3_replication_metrics_cloudwatch: bool,
    s3_public_endpoint: Option<String>,
    storage_api: SocketAddr,
    storage_api_public_url: Option<String>,
    s3_saml_audiences: Option<String>,
    s3_webhook_allow_private: bool,
    s3_cache_memory: String,
    s3_cache_disk: String,
    s3_cache_dir: Option<PathBuf>,
    s3_cache_max_object: String,
    s3_cache_kms: bool,
    cdn_origin_secret: Option<String>,
    cdn_cloudflare_api: String,
}

impl From<&Args> for StorageOpts {
    fn from(a: &Args) -> Self {
        Self {
            s3: a.s3,
            public_host: a.public_host.clone(),
            node: a.node.clone(),
            s3_domain: a.s3_domain.clone(),
            s3_website_domain: a.s3_website_domain.clone(),
            s3_block_public_access: a.s3_block_public_access,
            s3_storage_class: a.s3_storage_class.clone(),
            s3_internal_cidrs: a.s3_internal_cidrs.clone(),
            s3_kms: a.s3_kms,
            s3_sse_s3_at_rest: a.s3_sse_s3_at_rest,
            s3_access_key: a.s3_access_key.clone(),
            s3_secret_key: a.s3_secret_key.clone(),
            s3_notify_aws: a.s3_notify_aws,
            s3_notify_aws_endpoint: a.s3_notify_aws_endpoint.clone(),
            s3_notify_aws_endpoints: a.s3_notify_aws_endpoints.clone(),
            s3_replication_metrics_cloudwatch: a.s3_replication_metrics_cloudwatch,
            s3_public_endpoint: a.s3_public_endpoint.clone(),
            storage_api: a.storage_api,
            storage_api_public_url: a.storage_api_public_url.clone(),
            s3_saml_audiences: a.s3_saml_audiences.clone(),
            s3_webhook_allow_private: a.s3_webhook_allow_private,
            s3_cache_memory: a.s3_cache_memory.clone(),
            s3_cache_disk: a.s3_cache_disk.clone(),
            s3_cache_dir: a.s3_cache_dir.clone(),
            s3_cache_max_object: a.s3_cache_max_object.clone(),
            s3_cache_kms: a.s3_cache_kms,
            cdn_origin_secret: a.cdn_origin_secret.clone().filter(|s| !s.is_empty()),
            cdn_cloudflare_api: a.cdn_cloudflare_api.clone(),
        }
    }
}


/// Bind storage's listeners and take part in serving it on `cluster`: on a node alone, the
/// store is open when this returns (and an error opening it fails startup); in a cluster it opens
/// whenever this node gets the metadata's lease. `sink`: storage events into roll queues and
/// streams.
pub async fn start(args: &StorageOpts, home: &Path, storage: &StorageConfig, cluster: Arc<roll_cluster::Cluster>, sink: Arc<dyn s3_server::EventSink>) -> anyhow::Result<Storage> {
    let single = cluster.backend_name() == "memory";
    if !single && matches!(storage, StorageConfig::Local(_)) {
        anyhow::bail!("a storage cluster needs S3 (--s3-bucket): a local directory isn't shared between nodes");
    }
    let class = args.s3_storage_class.clone().or_else(|| match storage {
        StorageConfig::S3 { endpoint: None, .. } => Some("INTELLIGENT_TIERING".into()),
        StorageConfig::S3 { endpoint: Some(_), .. } => Some("STANDARD".into()),
        StorageConfig::Local(_) => None,
    });
    let class = class.filter(|_| matches!(storage, StorageConfig::S3 { .. }));
    // Bind first: a node that can't serve shouldn't take over the metadata log.
    let listener = tokio::net::TcpListener::bind(args.s3).await?;
    let api_listener = tokio::net::TcpListener::bind(args.storage_api).await?;
    let backend = s3_store::from_config(storage, class.as_deref()).await?;
    let mut scfg = s3_server::Config {
        virtual_host_domain: args.s3_domain.clone(),
        website_domain: args.s3_website_domain.clone(),
        spool_dir: home.join("storage").join("spool"),
        ..Default::default()
    };
    scfg.notify.aws = args.s3_notify_aws;
    if args.s3_replication_metrics_cloudwatch {
        scfg.notify.replication_metrics_interval = Some(std::time::Duration::from_secs(60));
    }
    scfg.notify.webhook_allow_private = args.s3_webhook_allow_private;
    let size = |name: &str, v: &str| s3_server::cache::parse_size(v).map_err(|e| anyhow::anyhow!("{name}: {e}"));
    scfg.cache = s3_server::CacheConfig {
        memory_bytes: size("ROLL_S3_CACHE_MEMORY", &args.s3_cache_memory)?,
        disk_bytes: size("ROLL_S3_CACHE_DISK", &args.s3_cache_disk)?,
        dir: Some(args.s3_cache_dir.clone().unwrap_or_else(|| home.join("storage").join("cache"))),
        max_object_bytes: size("ROLL_S3_CACHE_MAX_OBJECT", &args.s3_cache_max_object)?,
        kms: args.s3_cache_kms,
        ..Default::default()
    };
    scfg.cdn.origin_secret = args.cdn_origin_secret.clone();
    scfg.cdn.cloudflare_api = args.cdn_cloudflare_api.clone();
    scfg.notify.aws_endpoint = args.s3_notify_aws_endpoint.clone().filter(|e| !e.is_empty());
    for pair in args.s3_notify_aws_endpoints.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (service, url) = pair.split_once('=').ok_or_else(|| anyhow::anyhow!("ROLL_S3_NOTIFY_AWS_ENDPOINTS: {pair} isn't <service>=<url>"))?;
        scfg.notify.aws_endpoints.insert(service.trim().to_string(), url.trim().to_string());
    }
    scfg.public_endpoint = Some(args.s3_public_endpoint.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.s3.port())));
    scfg.torrent_announce = std::env::var("ROLL_S3_TORRENT_ANNOUNCE").ok().filter(|a| !a.trim().is_empty());
    scfg.saml_audiences = args.s3_saml_audiences.as_deref().unwrap_or_default().split(',').map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect();
    let mut cfg = s3_store::StoreConfig::new(home.join("storage"));
    // Log entries name their writer: the cluster's node id when there's more than one.
    cfg.node = if single { args.node.clone() } else { cluster.node_id().to_string() };
    cfg.block_public_access = args.s3_block_public_access;
    cfg.region = scfg.region.clone();
    cfg.kms = args.s3_kms;
    cfg.sse_s3_at_rest = args.s3_sse_s3_at_rest;
    if let Some(d) = std::env::var("ROLL_S3_DERIVED_EXPIRY_DAYS").ok().filter(|v| !v.trim().is_empty()) {
        cfg.derived_expiry_days = d.trim().parse().map_err(|_| anyhow::anyhow!("ROLL_S3_DERIVED_EXPIRY_DAYS: {d} isn't a number of days"))?;
    }
    if let Some(n) = std::env::var("ROLL_S3_IMAGE_CONCURRENCY").ok().filter(|v| !v.trim().is_empty()) {
        scfg.image_concurrency = n.trim().parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| anyhow::anyhow!("ROLL_S3_IMAGE_CONCURRENCY: {n} isn't a positive number"))?;
    }
    std::fs::create_dir_all(&scfg.spool_dir)?;
    if let Some(c) = &args.s3_internal_cidrs {
        scfg.internal_cidrs = s3_server::parse_cidrs(c)?;
    }
    // The public endpoint (the edge's S3 host behind it), else this node's listener.
    let endpoint = args.s3_public_endpoint.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.s3.port()));
    let ccfg = s3_control::ControlConfig { endpoint, region: scfg.region.clone(), virtual_host_domain: args.s3_domain.clone(), website_domain: args.s3_website_domain.clone(), storage_class: class, pricing: Default::default() };
    let api_url = args.storage_api_public_url.clone().unwrap_or_else(|| format!("http://{}:{}{}", args.public_host, args.storage_api.port(), s3_api::PREFIX));
    let api_cfg = s3_api::Config { public_url: args.storage_api_public_url.clone(), spool_dir: scfg.spool_dir.clone(), ..Default::default() };
    // One hot-object cache for every store this node opens: it's keyed by blob.
    let cache = s3_server::Server::cache(&scfg);
    let admin = args.s3_access_key.clone().zip(args.s3_secret_key.clone());
    let url = api_url.clone();
    let open: s3_server::cluster::Open = Arc::new(move |store: Arc<s3_store::Store>| {
        let (scfg, ccfg, api_cfg, cache, admin, sink, url) = (scfg.clone(), ccfg.clone(), api_cfg.clone(), cache.clone(), admin.clone(), sink.clone(), url.clone());
        Box::pin(async move {
            if let Some((id, secret)) = &admin {
                store.ensure_key(id, secret, "ROLL_S3_ACCESS_KEY").await?;
            }
            let control = s3_control::Control::new(store.clone(), ccfg).await?;
            let server = s3_server::Server::with_cache(store, scfg, cache);
            server.notifier.set_sink(sink);
            control.set_server(server.clone());
            control.set_api_endpoint(url);
            let api = s3_api::Api::new(server.clone(), api_cfg);
            Ok(Services { s3: s3_server::router(server), api: Some(s3_api::router(api)), control: Some(s3_control::api::router(control)) })
        })
    });
    let mut ncfg = NodeConfig::new(cfg);
    ncfg.single = single;
    let node = StorageNode::start(cluster, backend.clone(), ncfg, open).await?;
    info!(s3 = %args.s3, api = %args.storage_api, url = api_url, storage = %backend.describe(), single, "storage (S3 API and developer API) listening");
    Ok(Storage { node, listeners: Some((listener, api_listener)) })
}
