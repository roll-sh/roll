use clap::Parser;
use pg_control::{Config, Control, api, reconciler};
use pg_proxy::{Proxy, ProxyConfig};
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

/// roll daemon: serverless Postgres and tasks on your machine (or an EC2 host).
#[derive(Parser)]
#[command(version)]
struct Args {
    /// State directory (SQLite, local data cache, WAL spool).
    #[arg(long, env = "ROLL_HOME", default_value = ".roll")]
    home: PathBuf,
    /// Control-plane HTTP API.
    #[arg(long, env = "ROLL_API", default_value = "127.0.0.1:7070")]
    api: SocketAddr,
    /// Postgres proxy listen address.
    #[arg(long, env = "ROLL_PROXY", default_value = "127.0.0.1:55432")]
    proxy: SocketAddr,
    /// Host written into connection strings.
    #[arg(long, env = "ROLL_PUBLIC_HOST", default_value = "127.0.0.1")]
    public_host: String,
    /// How computes reach this daemon (default: host.docker.internal on the API port).
    #[arg(long, env = "ROLL_INTERNAL_URL")]
    internal_url: Option<String>,
    /// Memory per compute unit, in MiB (AWS sizing is 4096).
    #[arg(long, env = "ROLL_MEM_PER_CU", default_value_t = 1024)]
    mem_per_cu: u64,
    /// Endpoint for clients that can't pass `options=endpoint=...`.
    #[arg(long, env = "ROLL_DEFAULT_ENDPOINT")]
    default_endpoint: Option<String>,
    /// S3 bucket for durable storage. Without it, a local directory under --home stands in.
    #[arg(long, env = "ROLL_S3_BUCKET")]
    s3_bucket: Option<String>,
    #[arg(long, env = "ROLL_S3_PREFIX")]
    s3_prefix: Option<String>,
    /// S3-compatible endpoint (MinIO, R2, …). Credentials: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
    /// or the instance role.
    #[arg(long, env = "ROLL_S3_ENDPOINT")]
    s3_endpoint: Option<String>,
    #[arg(long, env = "AWS_REGION")]
    s3_region: Option<String>,
    /// Evict suspended branches to object storage after this many seconds.
    #[arg(long, env = "ROLL_EVICT_AFTER", default_value_t = 86_400)]
    evict_after: i64,
    /// Token for the tasks API (`/v1/tasks`); without one it's open, like the rest of the local API.
    #[arg(long, env = "ROLL_TASKS_TOKEN")]
    tasks_token: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,pg_proxy=info,pg_control=info,pg_storage=info,tasks=info".into()),
        )
        .init();
    let args = Args::parse();

    let home = std::path::absolute(&args.home)?;
    let tasks_store = std::sync::Arc::new(tasks::store::sqlite::SqliteStore::open(&home.join("tasks.db"), 4)?);
    let tasks = tasks::Tasks::new(
        tasks::Config { token: args.tasks_token.clone().filter(|t| !t.is_empty()), ..tasks::Config::default() },
        tasks_store,
    )
    .await?;
    let tasks_worker = tasks::start(tasks.clone());

    let mut config = Config::local(home);
    config.public_host = args.public_host;
    config.public_port = args.proxy.port();
    config.mem_mb_per_cu = args.mem_per_cu;
    config.evict_after_s = args.evict_after;
    config.internal_url = args.internal_url.unwrap_or_else(|| format!("http://host.docker.internal:{}", args.api.port()));
    if let Some(bucket) = args.s3_bucket {
        config.storage = StorageConfig::S3 {
            bucket,
            region: args.s3_region,
            endpoint: args.s3_endpoint,
            prefix: args.s3_prefix,
        };
    }

    let control = Control::new(config).await?;
    info!(storage = %control.objects.description, "durable storage");
    reconciler::recover(&control).await?;

    let proxy = Proxy::new(
        api::proxy_router(control.clone()),
        ProxyConfig { listen: args.proxy, default_endpoint: args.default_endpoint },
    );
    let listener = tokio::net::TcpListener::bind(args.api).await?;
    info!(addr = %args.api, "control API listening");

    tokio::select! {
        r = proxy.serve() => r?,
        r = axum::serve(listener, api::router(control.clone()).merge(tasks::api::router(tasks.clone()))) => r?,
        _ = reconciler::run(control.clone()) => {}
        _ = tokio::signal::ctrl_c() => info!("shutting down; computes keep running and are adopted on next start"),
    }
    tasks_worker.shutdown().await;
    Ok(())
}
