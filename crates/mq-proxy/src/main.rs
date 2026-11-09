//! `mq-proxy`: run the roll queues front proxy (one per availability zone).

use clap::Parser;
use mq_proxy::ecs::EcsWaker;
use mq_proxy::{CommandWaker, Directory, Proxy, ProxyConfig, Waker};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "mq-proxy", about = "roll queues front proxy: wake-on-connect, AZ-aware routing")]
struct Args {
    /// Cluster metadata database (the brokers' `--queues-cluster-meta`).
    #[arg(long, env = "ROLL_QUEUES_CLUSTER_META")]
    meta: String,
    /// `protocol=address` to listen on, repeatable (e.g. `amqp=0.0.0.0:5672`).
    #[arg(long = "listen", env = "ROLL_PROXY_LISTEN", value_delimiter = ',', default_value = "amqp=0.0.0.0:5672")]
    listen: Vec<String>,
    /// This proxy's availability zone; brokers in the same zone are preferred.
    #[arg(long, env = "ROLL_QUEUES_AZ")]
    az: Option<String>,
    /// Shell command that starts brokers when none is running.
    #[arg(long, env = "ROLL_PROXY_WAKE_COMMAND")]
    wake_command: Option<String>,
    /// Wake by scaling this ECS service (`cluster/service`) to `--wake-count` tasks.
    #[arg(long, env = "ROLL_PROXY_WAKE_ECS_SERVICE")]
    wake_ecs_service: Option<String>,
    #[arg(long, env = "ROLL_PROXY_WAKE_COUNT", default_value_t = 1)]
    wake_count: u32,
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// Health check listener for load balancers (answers HTTP 200 without touching brokers, so
    /// health checks never wake a cluster that scaled to zero).
    #[arg(long, env = "ROLL_PROXY_HEALTH")]
    health: Option<SocketAddr>,
    /// More CA certificates (PEM) to verify the metadata Postgres with (e.g. the RDS bundle).
    #[arg(long, env = "ROLL_QUEUES_CLUSTER_META_CA")]
    meta_ca: Option<std::path::PathBuf>,
    /// Seconds a connection waits for a broker to start.
    #[arg(long, env = "ROLL_PROXY_WAKE_TIMEOUT", default_value_t = 60)]
    wake_timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let mut listeners = Vec::new();
    for l in &args.listen {
        let (protocol, addr) = l.split_once('=').ok_or_else(|| anyhow::anyhow!("--listen wants protocol=address, got {l}"))?;
        listeners.push((protocol.to_string(), addr.parse::<SocketAddr>()?));
    }
    let az = match args.az {
        Some(az) => Some(az),
        None => mq_cluster::aws::ecs_availability_zone().await,
    };
    if let Some(h) = args.health {
        let l = tokio::net::TcpListener::bind(h).await?;
        tokio::spawn(mq_proxy::serve_health(l));
    }
    let config = ProxyConfig { listeners, az, wake_timeout: Duration::from_secs(args.wake_timeout), ..ProxyConfig::default() };
    let meta = mq_cluster::PgMeta::connect_with(&args.meta, args.meta_ca.as_deref()).await?;
    let directory = Directory::new(meta, config);
    directory.refresh().await?;
    directory.spawn_refresh();
    let waker: Option<Arc<dyn Waker>> = match (args.wake_ecs_service, args.wake_command) {
        (Some(svc), _) => {
            let (cluster, service) = svc.split_once('/').ok_or_else(|| anyhow::anyhow!("--wake-ecs-service wants cluster/service"))?;
            let region = args.region.ok_or_else(|| anyhow::anyhow!("--wake-ecs-service needs AWS_REGION"))?;
            Some(Arc::new(EcsWaker::new(cluster.into(), service.into(), args.wake_count, region)))
        }
        (None, Some(c)) => Some(Arc::new(CommandWaker(c))),
        (None, None) => None,
    };
    Proxy::new(directory, waker).serve().await
}
