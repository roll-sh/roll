//! `search-dev`: one roll search cluster on a port, without authentication, for local
//! development and the YAML REST test runner.
//!
//! ```text
//! search-dev --listen 127.0.0.1:59200 [--cluster dev] [--dialect elasticsearch|opensearch]
//!            [--data-dir .roll/search-dev [--s3-bucket b [--s3-endpoint url] [--s3-prefix p]]]
//!            [--tls-cert cert.pem --tls-key key.pem]
//!            [-E node.attr.rack=r1 ...] [--test-features] [--yaml-test-cluster]
//! ```
//!
//! `--yaml-test-cluster` sets up the node like the clusters Elasticsearch's YAML REST suites run
//! against (`node.attr.testattr=test`, `node.processors=2`, `path.repo` in the temp directory, test
//! features published).
//!
//! Without `--data-dir` the cluster lives in memory; with it, the cluster (metadata and
//! documents) is kept through `search-store` in that directory (objects under `objects/`, or in
//! S3 with `--s3-bucket`; the WAL and the local cache under `cache/`): every acknowledged write survives a restart or a
//! crash (`kill -9`). `--idle-unload <s>` (or `--idle-unload-ms`) unloads the cluster when idle (it wakes on the next
//! request); `--sync-objects` acknowledges writes once they are in object storage. SIGINT and
//! SIGTERM stop gracefully: requests in flight finish, the cluster is flushed.

use clap::Parser;
use search_es::Dialect;
use std::time::Duration;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
use search_server::{ClusterSpec, ListenOptions, MemoryStores, Node, NodeConfig, StaticCatalog};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "search-dev", about = "Serve one roll search cluster without authentication")]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:59200")]
    listen: String,
    /// Cluster name (`GET /` `cluster_name`).
    #[arg(long, default_value = "roll-search")]
    cluster: String,
    /// Node name (`GET /` `name`).
    #[arg(long, default_value = "roll-search-dev")]
    node_name: String,
    /// REST dialect: elasticsearch or opensearch.
    #[arg(long, default_value = "elasticsearch")]
    dialect: String,
    /// `action.destructive_requires_name` default: Elasticsearch's test clusters (which the
    /// YAML suites target) allow wildcard deletes, so the dev server does too unless asked.
    #[arg(long, default_value_t = false)]
    destructive_requires_name: bool,
    /// Keep the cluster on disk (through `search-store`) instead of in memory.
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,
    /// A node setting, `key=value` (Elasticsearch's `-E`): `node.attr.*`, `node.processors`.
    #[arg(short = 'E', value_name = "KEY=VALUE")]
    setting: Vec<String>,
    /// Publish Elasticsearch's test cluster features (`-Dtests.testfeatures.enabled=true`).
    #[arg(long)]
    test_features: bool,
    /// Configure the node like Elasticsearch's YAML REST test clusters.
    #[arg(long)]
    yaml_test_cluster: bool,
    /// Unload the cluster after this many idle seconds (with `--data-dir`; 0: never).
    #[arg(long, default_value_t = 0)]
    idle_unload: u64,
    /// Like `--idle-unload`, in milliseconds (stress tests: wake fidelity under constant
    /// unloads); overrides it.
    #[arg(long)]
    idle_unload_ms: Option<u64>,
    /// Acknowledge writes once the WAL is in object storage instead of on local disk.
    #[arg(long)]
    sync_objects: bool,
    /// Keep the objects in this S3 bucket (or one of an S3-compatible endpoint) instead of
    /// `<data-dir>/objects`; credentials come from the usual AWS environment variables.
    /// `--data-dir` still holds the local cache and the WAL.
    #[arg(long, requires = "data_dir")]
    s3_bucket: Option<String>,
    /// S3 endpoint URL (MinIO, R2, …).
    #[arg(long, requires = "s3_bucket")]
    s3_endpoint: Option<String>,
    /// Key prefix inside the bucket.
    #[arg(long, requires = "s3_bucket")]
    s3_prefix: Option<String>,
    /// S3 region (default: the AWS environment's).
    #[arg(long, requires = "s3_bucket")]
    s3_region: Option<String>,
    /// PEM certificate chain for HTTPS.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<std::path::PathBuf>,
    /// PEM private key for HTTPS.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    // Hand freed memory back at once, keep THP from inflating a sparse heap, and collect a
    // worker's heap when it parks, so an unloaded cluster's memory leaves the process.
    search_server::process::tune_memory();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().on_thread_park(search_server::process::on_thread_park).build()?;
    // Tokio's default worker count.
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get());
    search_server::process::start_purge_governor(rt.handle().clone(), workers);
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let args = Args::parse();
    let dialect = match args.dialect.as_str() {
        "elasticsearch" => Dialect::Elasticsearch,
        "opensearch" => Dialect::OpenSearch,
        other => anyhow::bail!("unknown dialect [{other}], expected elasticsearch or opensearch"),
    };
    let catalog = Arc::new(StaticCatalog::new());
    let mut spec = ClusterSpec::new(args.cluster.clone());
    spec.dialect = dialect;
    spec.max_in_flight = 256;
    spec.destructive_requires_name = args.destructive_requires_name;
    catalog.insert(spec);
    let mut node_settings = Vec::new();
    if args.yaml_test_cluster {
        node_settings.push(("node.attr.testattr".to_string(), "test".to_string()));
        node_settings.push(("node.processors".to_string(), "2".to_string()));
        // Elasticsearch's test clusters register a temporary directory as `path.repo`.
        node_settings.push(("path.repo".to_string(), std::env::temp_dir().join("roll-search-repo").to_string_lossy().into_owned()));
        // Like Elasticsearch's test clusters: reindex from remote may target this node, which
        // advertises its real port.
        node_settings.push(("reindex.remote.whitelist".to_string(), "127.0.0.1:*".to_string()));
        // The file-realm superuser Elasticsearch's test clusters define (some tests log in as it).
        node_settings.push(("roll.security.file_users".to_string(), "x_pack_rest_user:x-pack-test-password:superuser".to_string()));
        // The services of Elasticsearch's `inference-service-test` plugin, which its test clusters load.
        node_settings.push(("roll.inference.test_services".to_string(), "true".to_string()));
        if let Some((_, port)) = args.listen.rsplit_once(':') {
            node_settings.push(("http.publish_port".to_string(), port.to_string()));
        }
    }
    for s in &args.setting {
        let Some((k, v)) = s.split_once('=') else { anyhow::bail!("setting [{s}] must be key=value") };
        if k == "node.processors" && !v.parse::<f64>().is_ok_and(|p| p > 0.0 && p.is_finite()) {
            anyhow::bail!("Failed to parse value [{v}] for setting [node.processors]");
        }
        node_settings.push((k.trim().to_string(), v.trim().to_string()));
    }
    // `http.max_content_length` sizes the node's body limit, as on Elasticsearch.
    let max_body_bytes = match node_settings.iter().rev().find(|(k, _)| k == "http.max_content_length") {
        Some((_, v)) => match search_es::units::ByteSize::parse(v, "http.max_content_length") {
            Ok(b) if b.bytes() >= 0 => b.bytes() as usize,
            _ => anyhow::bail!("failed to parse value [{v}] for setting [http.max_content_length]"),
        },
        None => search_server::node::DEFAULT_MAX_BODY,
    };
    let cfg = NodeConfig {
        max_body_bytes,
        node_name: args.node_name,
        default_cluster: Some(args.cluster.clone()),
        idle_unload: Some(args.idle_unload_ms.map_or(Duration::from_secs(args.idle_unload), Duration::from_millis))
            .filter(|d| !d.is_zero() && args.data_dir.is_some()),
        // One tenant: no node-wide cap beyond the cluster's own.
        max_running: usize::MAX,
        node_settings,
        test_features: args.test_features || args.yaml_test_cluster,
        // One tenant: the host's numbers are its own.
        host_metrics: true,
        // As rolld does: stored secrets sealed under ROLL_SECRET_KEY when it's set.
        sealer: std::env::var("ROLL_SECRET_KEY").ok().and_then(|k| search_es::seal::Sealer::from_secret(&k)),
        ..NodeConfig::default()
    };
    let stores: Arc<dyn search_server::StoreProvider> = match &args.data_dir {
        Some(dir) => {
            let storage = match &args.s3_bucket {
                Some(bucket) => search_store::StorageConfig::S3 {
                    bucket: bucket.clone(),
                    region: args.s3_region.clone(),
                    endpoint: args.s3_endpoint.clone(),
                    prefix: args.s3_prefix.clone(),
                },
                None => search_store::StorageConfig::Local(dir.join("objects")),
            };
            let objects = search_store::Objects::new(&storage)?;
            let mut scfg = search_store::StoreConfig { verify: Some(search_server::persist::segment_verifier()), ..Default::default() };
            scfg.wal.sync_objects = args.sync_objects;
            let store = search_store::Store::open(objects, dir.join("cache"), scfg).await?;
            Arc::new(search_server::ObjectStores::new(store))
        }
        None => Arc::new(MemoryStores::new()),
    };
    let node = Node::new(catalog, stores, cfg);
    let mut opts = ListenOptions::default();
    if let (Some(c), Some(k)) = (&args.tls_cert, &args.tls_key) {
        opts.tls = Some(search_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?);
    }
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, cluster = args.cluster, persistent = args.data_dir.is_some(), "roll search (dev) listening");
    let shutdown = search_server::Shutdown::new();
    let server = tokio::spawn(search_server::serve_until(listener, node.clone(), opts, shutdown.clone()));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(30), server).await;
    node.shutdown(Duration::from_secs(30)).await
}
