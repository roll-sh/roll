use clap::Parser;
use pg_control::{Config, Control, api, reconciler};
use pg_proxy::{Proxy, ProxyConfig};
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

/// roll daemon: serverless Postgres on your machine (or an EC2 host).
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
    /// Redis (RESP) listen address.
    #[arg(long, env = "ROLL_REDIS", default_value = "127.0.0.1:56380")]
    redis: SocketAddr,
    /// Redis REST API (Upstash-compatible) listen address.
    #[arg(long, env = "ROLL_REDIS_HTTP", default_value = "127.0.0.1:58080")]
    redis_http: SocketAddr,
    /// TLS certificate chain and key (PEM) for Redis: clients connect with `rediss://`, and the
    /// TLS server name can select the database.
    #[arg(long, env = "ROLL_REDIS_TLS_CERT", requires = "redis_tls_key")]
    redis_tls_cert: Option<PathBuf>,
    #[arg(long, env = "ROLL_REDIS_TLS_KEY", requires = "redis_tls_cert")]
    redis_tls_key: Option<PathBuf>,
    /// Expect PROXY protocol headers on the Redis port (behind an AWS NLB or HAProxy).
    #[arg(long, env = "ROLL_REDIS_PROXY_PROTOCOL")]
    redis_proxy_protocol: bool,
    /// This node's name in object storage (unique per host).
    #[arg(long, env = "ROLL_NODE", default_value = "local")]
    node: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,pg_proxy=info,pg_control=info,pg_storage=info".into()),
        )
        .init();
    let args = Args::parse();

    let mut config = Config::local(std::path::absolute(&args.home)?);
    config.public_host = args.public_host.clone();
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

    let storage = config.storage.clone();
    let control = Control::new(config).await?;
    info!(storage = %control.objects.description, "durable storage");
    reconciler::recover(&control).await?;

    let proxy = Proxy::new(
        api::proxy_router(control.clone()),
        ProxyConfig { listen: args.proxy, default_endpoint: args.default_endpoint },
    );
    // Serverless Redis: same object storage, its own node.
    let mut rcfg = redis_control::RedisConfig::local(std::path::absolute(&args.home)?.join("redis"));
    rcfg.node_name = args.node;
    rcfg.public_host = args.public_host.clone();
    rcfg.resp_port = args.redis.port();
    rcfg.http_port = args.redis_http.port();
    let tls = match (&args.redis_tls_cert, &args.redis_tls_key) {
        (Some(c), Some(k)) => Some(redis_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?),
        _ => None,
    };
    rcfg.tls = tls.is_some();
    let redis = redis_control::Control::new(rcfg, redis_server::persist::Objects::new(&storage)?).await?;
    let redis_dir: std::sync::Arc<dyn redis_server::Directory> = redis.node.clone();
    let redis_listener = tokio::net::TcpListener::bind(args.redis).await?;
    let redis_http = tokio::net::TcpListener::bind(args.redis_http).await?;
    info!(resp = %args.redis, http = %args.redis_http, tls = tls.is_some(), "redis listening");
    let redis_opts = redis_server::net::ListenOptions { tls, proxy_protocol: args.redis_proxy_protocol };

    let listener = tokio::net::TcpListener::bind(args.api).await?;
    info!(addr = %args.api, "control API listening");

    tokio::select! {
        r = proxy.serve() => r?,
        r = axum::serve(listener, api::router(control.clone()).merge(redis_control::api::router(redis.clone()))) => r?,
        r = redis_server::net::serve(redis_listener, redis_dir.clone(), redis_opts) => r?,
        r = redis_server::http::serve_http(redis_http, redis_dir) => r?,
        _ = reconciler::run(control.clone()) => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down; computes keep running and are adopted on next start");
            // Make every acknowledged Redis write durable before exiting.
            redis.shipper.flush().await?;
            redis.shipper.write_manifests(true).await?;
        }
    }
    Ok(())
}
