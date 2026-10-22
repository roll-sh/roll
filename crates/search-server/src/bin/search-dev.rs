//! `search-dev`: one roll search cluster on a port, without authentication, for local
//! development and the YAML REST test runner.
//!
//! ```text
//! search-dev --listen 127.0.0.1:59200 [--cluster dev] [--dialect elasticsearch|opensearch]
//!            [--data-dir .roll/search-dev] [--tls-cert cert.pem --tls-key key.pem]
//!            [-E node.attr.rack=r1 ...] [--test-features] [--yaml-test-cluster]
//! ```
//!
//! `--yaml-test-cluster` sets up the node like the clusters Elasticsearch's YAML REST suites run
//! against (`node.attr.testattr=test`, `node.processors=2`, `path.repo` in the temp directory, test
//! features published).
//!
//! Without `--data-dir` the cluster lives in memory; with it, metadata is kept through
//! `search-store` in that directory (objects under `objects/`, the local cache under `cache/`)
//! and survives restarts.

use clap::Parser;
use search_es::Dialect;
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
    /// PEM certificate chain for HTTPS.
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<std::path::PathBuf>,
    /// PEM private key for HTTPS.
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    }
    for s in &args.setting {
        let Some((k, v)) = s.split_once('=') else { anyhow::bail!("setting [{s}] must be key=value") };
        if k == "node.processors" && !v.parse::<f64>().is_ok_and(|p| p > 0.0 && p.is_finite()) {
            anyhow::bail!("Failed to parse value [{v}] for setting [node.processors]");
        }
        node_settings.push((k.trim().to_string(), v.trim().to_string()));
    }
    let cfg = NodeConfig {
        node_name: args.node_name,
        default_cluster: Some(args.cluster.clone()),
        idle_unload: None,
        node_settings,
        test_features: args.test_features || args.yaml_test_cluster,
        // One tenant: the host's numbers are its own.
        host_metrics: true,
        ..NodeConfig::default()
    };
    let stores: Arc<dyn search_server::StoreProvider> = match &args.data_dir {
        Some(dir) => {
            let objects = search_store::Objects::new(&search_store::StorageConfig::Local(dir.join("objects")))?;
            let store = search_store::Store::open(objects, dir.join("cache"), search_store::StoreConfig::default()).await?;
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
    tracing::info!(addr = %listener.local_addr()?, cluster = args.cluster, "roll search (dev) listening");
    tokio::select! {
        r = search_server::serve(listener, node, opts) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
