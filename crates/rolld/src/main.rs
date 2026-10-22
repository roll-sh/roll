use clap::Parser;
use pg_control::{Config, Control, api, reconciler};
use pg_proxy::{Proxy, ProxyConfig};
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use stream_broker::config::TlsFiles;
use stream_broker::store::Settings;
use tracing::info;

/// mimalloc hands freed memory back to the OS; the system allocator on macOS keeps it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_purge_delay` (libmimalloc-sys doesn't export the constant).
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

mod queues;

/// roll daemon: serverless Postgres, serverless Redis, Kafka-compatible streams, tasks and RabbitMQ-compatible queues on
/// your machine (or an EC2 host).
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
    /// Kafka listener for streams.
    #[arg(long, env = "ROLL_KAFKA", default_value = "127.0.0.1:9092")]
    kafka: SocketAddr,
    /// Host and port Kafka clients are told to connect to (default: --public-host and the
    /// listener's port).
    #[arg(long, env = "ROLL_KAFKA_ADVERTISED_HOST")]
    kafka_advertised_host: Option<String>,
    #[arg(long, env = "ROLL_KAFKA_ADVERTISED_PORT")]
    kafka_advertised_port: Option<u16>,
    /// Stream for Kafka connections that don't authenticate; created if missing. Empty to
    /// require SASL on every connection.
    #[arg(long, env = "ROLL_DEFAULT_STREAM", default_value = "default")]
    default_stream: String,
    /// Serve the Kafka listener over TLS with this PEM certificate chain and key.
    #[arg(long, env = "ROLL_KAFKA_TLS_CERT", requires = "kafka_tls_key")]
    kafka_tls_cert: Option<PathBuf>,
    #[arg(long, env = "ROLL_KAFKA_TLS_KEY", requires = "kafka_tls_cert")]
    kafka_tls_key: Option<PathBuf>,
    /// Require an API key on the HTTP stream data plane.
    #[arg(long, env = "ROLL_STREAM_REQUIRE_AUTH")]
    stream_require_auth: bool,
    /// Local disk target for stream data across open partitions, in MiB (0 = only per-topic
    /// `local.retention.*`). Size the volume for this plus a few seconds of ingest.
    #[arg(long, env = "ROLL_STREAM_DISK_MB", default_value_t = 0)]
    stream_disk_mb: u64,
    /// Keep closed partitions' data on local disk instead of serving it from object storage.
    #[arg(long, env = "ROLL_STREAM_KEEP_LOCAL")]
    stream_keep_local: bool,
    /// Token for the tasks API (`/v1/tasks`); without one it's open, like the rest of the local API.
    #[arg(long, env = "ROLL_TASKS_TOKEN")]
    tasks_token: Option<String>,
    /// Run only roll queues (no Postgres control plane or proxy): broker nodes on AWS.
    #[arg(long, env = "ROLL_QUEUES_ONLY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    queues_only: bool,
    /// Admin token for roll code (`/v1/code`, `/git`); without one the code API is open, like
    /// the rest of the local API.
    #[arg(long, env = "ROLL_CODE_TOKEN")]
    code_token: Option<String>,
    /// Where the API is reachable from outside, for roll code links (default:
    /// http://<public host>:<API port>).
    #[arg(long, env = "ROLL_CODE_PUBLIC_URL")]
    code_public_url: Option<String>,
    /// Base of roll code clone URLs (default: <public URL>/git; `https://roll.sh` behind the web app).
    #[arg(long, env = "ROLL_CODE_GIT_URL")]
    code_git_url: Option<String>,
    /// Where the web app serves roll code's public pages.
    #[arg(long, env = "ROLL_WEB_URL", default_value = "http://localhost:3000")]
    web_url: String,
    /// Let roll code's webhooks and review apps reach loopback and private addresses (local
    /// development only). Unset: allowed only on an open daemon (no ROLL_CODE_TOKEN).
    #[arg(long, env = "ROLL_CODE_WEBHOOK_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), num_args = 0..=1, default_missing_value = "true")]
    code_webhook_allow_private: Option<bool>,
    /// Let owners' own reviewer providers (`base_url`) be loopback or private addresses (a
    /// self-hosted model on the operator's network). Unset: only on an open daemon.
    #[arg(long, env = "ROLL_CODE_PROVIDER_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), num_args = 0..=1, default_missing_value = "true")]
    code_provider_allow_private: Option<bool>,
    /// roll code previews answer on `<change>.<this domain>` (on the API port).
    #[arg(long, env = "ROLL_CODE_PREVIEW_DOMAIN", default_value = "preview.localhost")]
    code_preview_domain: String,
    /// roll code checks running at once on this host (each in its own container).
    #[arg(long, env = "ROLL_CODE_RUNNER_CONCURRENCY", default_value_t = 2)]
    code_runner_concurrency: usize,
    /// roll code event streams (`/events`, MCP) one caller may hold open: a token, or an
    /// anonymous client address.
    #[arg(long, env = "ROLL_CODE_MAX_STREAMS_PER_CALLER", default_value_t = 32)]
    code_max_streams_per_caller: usize,
    /// roll code event streams open at once, for everyone.
    #[arg(long, env = "ROLL_CODE_MAX_STREAMS", default_value_t = 10_000)]
    code_max_streams: usize,
    /// Count anonymous roll code event streams by the first X-Forwarded-For address (only behind
    /// a proxy that sets it).
    #[arg(long, env = "ROLL_CODE_TRUST_FORWARDED_FOR", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    code_trust_forwarded_for: bool,
    #[command(flatten)]
    queues: queues::QueuesArgs,
}

fn main() -> anyhow::Result<()> {
    // Return freed memory to the OS right away (mimalloc's default waits 1 s), and have each worker
    // collect its heap when it goes idle: otherwise memory freed after a burst stays with threads
    // that are parked. Measured: no throughput cost; idle footprint after 256 MB of writes 36–56 MB
    // → 24–31 MB (and ~90 MB → ~60 MB with 600 partitions).
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_park(|| unsafe { libmimalloc_sys::mi_collect(false) })
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,pg_proxy=info,pg_control=info,pg_storage=info,redis_server=info,redis_control=info,stream_broker=info,tasks=info,code_control=info,code_store=info,code_git=info,code_review=info,code_index=info,mq_core=info,mq_amqp=info,mq_cluster=info,mq_mgmt=info,mq_http=info,mq_mqtt=info,mq_stomp=info,mq_shovel=info".into()),
        )
        .init();
    let args = Args::parse();
    let home = std::path::absolute(&args.home)?;

    let queues = if args.queues.disabled { None } else { Some(queues::Queues::start(&home, args.queues.clone(), &args.public_host).await?) };
    if args.queues_only {
        let q = queues.ok_or_else(|| anyhow::anyhow!("--queues-only with --no-queues leaves nothing to run"))?;
        let listener = tokio::net::TcpListener::bind(args.api).await?;
        info!(addr = %args.api, "queues control API listening");
        tokio::select! {
            r = axum::serve(listener, q.router()) => r?,
            r = q.serve() => r?,
            _ = shutdown_signal() => info!("shutting down"),
        }
        q.shutdown().await;
        return Ok(());
    }

    let tasks_store = std::sync::Arc::new(tasks::store::sqlite::SqliteStore::open(&home.join("tasks.db"), 4)?);
    let tasks = tasks::Tasks::new(
        tasks::Config { token: args.tasks_token.clone().filter(|t| !t.is_empty()), ..tasks::Config::default() },
        tasks_store,
    )
    .await?;
    let tasks_worker = tasks::start(tasks.clone());

    let mut stream_config = stream_broker::Config::local(home.clone());
    stream_config.listen = args.kafka;
    stream_config.advertised_host = args.kafka_advertised_host.clone().unwrap_or_else(|| args.public_host.clone());
    stream_config.advertised_port = args.kafka_advertised_port.unwrap_or(args.kafka.port());
    stream_config.default_stream = Some(args.default_stream.clone()).filter(|s| !s.is_empty());
    stream_config.tls = args.kafka_tls_cert.clone().zip(args.kafka_tls_key.clone()).map(|(cert, key)| TlsFiles { cert, key });
    stream_config.http_url = format!("http://{}:{}", args.public_host, args.api.port());
    stream_config.http_require_auth = args.stream_require_auth;
    stream_config.local_disk_bytes = args.stream_disk_mb << 20;
    stream_config.evict_on_unload = !args.stream_keep_local;

    let mut config = Config::local(home);
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
    stream_config.storage = config.storage.clone();

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

    let broker = stream_broker::Broker::new(stream_config).await?;
    if let Some(name) = &broker.config.default_stream
        && broker.find_stream(name).is_none()
    {
        broker.create_stream(name, Settings::default())?;
    }
    broker.txns.recover(&broker).await?;
    let kafka = tokio::net::TcpListener::bind(args.kafka).await?;

    // roll code: same object storage, its own metadata under <home>/code.
    let code_public = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    let code_config = code_control::Config {
        home: std::path::absolute(&args.home)?,
        admin_token: args.code_token.clone().filter(|t| !t.is_empty()),
        git_url: args.code_git_url.clone().unwrap_or_else(|| format!("{}/git", code_public.trim_end_matches('/'))),
        api_url: code_public,
        web_url: args.web_url.clone(),
        storage: control.objects.description.clone(),
        allow_private_webhooks: args.code_webhook_allow_private,
        allow_private_providers: args.code_provider_allow_private,
        // roll's reviewer: ROLL_REVIEW_PROVIDER, ROLL_REVIEW_ANTHROPIC_API_KEY, ROLL_SECRET_KEY, … (docs/code/reviews.md).
        review: code_control::ai::AiConfig::from_env(),
        max_streams_per_caller: args.code_max_streams_per_caller,
        max_streams: args.code_max_streams,
        trust_forwarded_for: args.code_trust_forwarded_for,
        ..code_control::Config::local(std::path::absolute(&args.home)?)
    };
    let code = code_control::Code::open(code_config, pg_storage::Objects::new(&storage)?)?;
    code.start_reviewer();
    // Webhook deliveries pick up where they left off (landings while the daemon was down too).
    code.start_webhooks();
    match &code.config.review.hosted {
        Some(h) => info!(provider = %h.provider, "roll code: hosted reviewer configured"),
        None => info!("roll code: no hosted reviewer (owners can bring their own key)"),
    }
    // GitHub-connected repos, when ROLL_GITHUB_APP_ID and friends are set (docs/code/github-app.md).
    if code_control::github::enable_from_env(&code)? {
        info!("roll code: GitHub App configured");
    }
    // Checks and previews: containers, with previews branching roll Postgres and Redis.
    let previews = code_preview::Previews::new(
        code_preview::PreviewConfig {
            domain: args.code_preview_domain.trim_matches('.').to_ascii_lowercase(),
            port: Some(args.api.port()),
            ..Default::default()
        },
        Some(std::sync::Arc::new(code_preview::RollData { postgres: control.clone(), redis: redis.clone() })),
    );
    let _code_checks = code_preview::start(&code, std::sync::Arc::new(code_preview::DockerRunner::new(args.code_runner_concurrency)), previews.clone());

    let listener = tokio::net::TcpListener::bind(args.api).await?;
    info!(addr = %args.api, "control API listening");
    let mut router = api::router(control.clone())
        .merge(redis_control::api::router(redis.clone()))
        .merge(stream_broker::http::router(broker.clone()))
        .merge(tasks::api::router(tasks.clone()))
        .merge(code_control::api::router(code.clone()));
    if let Some(q) = &queues {
        router = router.merge(q.router());
    }
    // Requests for preview hosts go to the preview's container; everything else to the API.
    let router = router.layer(axum::middleware::from_fn_with_state(previews, code_preview::route));
    let queues_serve = async {
        match &queues {
            Some(q) => q.serve().await,
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        r = proxy.serve() => r?,
        // Client addresses reach handlers (roll code counts anonymous event streams per address).
        r = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()) => r?,
        r = redis_server::net::serve(redis_listener, redis_dir.clone(), redis_opts) => r?,
        r = redis_server::http::serve_http(redis_http, redis_dir) => r?,
        r = queues_serve => r?,
        r = stream_broker::server::serve_on(broker.clone(), kafka) => r?,
        _ = reconciler::run(control.clone()) => {}
        _ = broker.clone().run() => {}
        _ = shutdown_signal() => {
            info!("shutting down; computes keep running and are adopted on next start");
            // Make every acknowledged Redis write durable before exiting.
            redis.shipper.flush().await?;
            redis.shipper.write_manifests(true).await?;
        }
    }
    if let Some(q) = &queues {
        q.shutdown().await;
    }
    tasks_worker.shutdown().await;
    broker.sync_all().await;
    Ok(())
}

/// Ctrl-C, or SIGTERM from a supervisor (systemd, ECS, Kubernetes) during a rolling upgrade.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
