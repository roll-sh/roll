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

mod code;
mod nosql;
mod queues;
mod storage_events;

/// roll daemon: serverless Postgres, serverless Redis, S3-compatible storage, Kafka-compatible streams, tasks,
/// RabbitMQ-compatible queues and ScyllaDB/Cassandra-compatible nosql on your machine (or an EC2 host).
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
    /// Run only roll code (no Postgres control plane or proxy, Redis, streams, tasks or queues):
    /// the nodes of a roll code cluster.
    #[arg(long, env = "ROLL_CODE_ONLY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    code_only: bool,
    #[command(flatten)]
    code: code::CodeArgs,
    /// S3-compatible storage listen address.
    #[arg(long, env = "ROLL_S3", default_value = "127.0.0.1:59000")]
    s3: SocketAddr,
    /// `<bucket>.<domain>` addresses a bucket (virtual-host style); path style always works.
    #[arg(long, env = "ROLL_S3_DOMAIN")]
    s3_domain: Option<String>,
    /// `<bucket>.<domain>` serves the bucket's static website (S3 website endpoint semantics).
    #[arg(long, env = "ROLL_S3_WEBSITE_DOMAIN")]
    s3_website_domain: Option<String>,
    /// Block public access to every bucket (all four S3 Block Public Access settings, on top of
    /// each bucket's own).
    #[arg(long, env = "ROLL_S3_BLOCK_PUBLIC_ACCESS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    s3_block_public_access: bool,
    /// Storage class of stored objects (default: INTELLIGENT_TIERING on AWS S3, STANDARD on a
    /// custom endpoint; ignored for a local directory).
    #[arg(long, env = "ROLL_S3_STORAGE_CLASS")]
    s3_storage_class: Option<String>,
    /// Peers in these comma-separated CIDRs are inside roll: their egress is free (default:
    /// loopback and the private ranges).
    #[arg(long, env = "ROLL_S3_INTERNAL_CIDRS")]
    s3_internal_cidrs: Option<String>,
    /// Pass SSE-KMS requests through to the backing bucket (default: on for AWS S3 without a
    /// custom endpoint, off otherwise; a local directory can't).
    #[arg(long, env = "ROLL_S3_KMS")]
    s3_kms: Option<bool>,
    /// Declare that the backing store encrypts every object at rest, so objects without SSE-C or
    /// SSE-KMS are reported as SSE-S3 (`AES256`), as S3 does (default: on for AWS S3 without a
    /// custom endpoint, off otherwise). roll doesn't encrypt them itself: only set it when the
    /// backing store (or the disk under a local directory) really does.
    #[arg(long, env = "ROLL_S3_SSE_S3_AT_REST")]
    s3_sse_s3_at_rest: Option<bool>,
    /// An admin access key to create at startup (local dev and tests), with ROLL_S3_SECRET_KEY.
    #[arg(long, env = "ROLL_S3_ACCESS_KEY", requires = "s3_secret_key")]
    s3_access_key: Option<String>,
    #[arg(long, env = "ROLL_S3_SECRET_KEY", requires = "s3_access_key", hide_env_values = true)]
    s3_secret_key: Option<String>,
    /// Deliver storage event notifications to AWS (SQS, SNS, Lambda, EventBridge) with this
    /// host's AWS credentials. Off, AWS destinations fail validation.
    #[arg(long, env = "ROLL_S3_NOTIFY_AWS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    s3_notify_aws: bool,
    /// Reach AWS services for storage notifications, Object Lambda and replication checks at
    /// this endpoint instead of AWS (LocalStack, a VPC endpoint).
    #[arg(long, env = "ROLL_S3_NOTIFY_AWS_ENDPOINT")]
    s3_notify_aws_endpoint: Option<String>,
    /// Per-service endpoints, `sqs=<url>,sns=<url>,lambda=<url>,events=<url>` (win over
    /// ROLL_S3_NOTIFY_AWS_ENDPOINT).
    #[arg(long, env = "ROLL_S3_NOTIFY_AWS_ENDPOINTS")]
    s3_notify_aws_endpoints: Option<String>,
    /// Publish storage replication metrics (S3's `ReplicationLatency`, `BytesPendingReplication`,
    /// `OperationsPendingReplication`, `OperationsFailedReplication`) to CloudWatch every minute,
    /// in the `roll/S3` namespace (needs ROLL_S3_NOTIFY_AWS).
    #[arg(long, env = "ROLL_S3_REPLICATION_METRICS_CLOUDWATCH", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    s3_replication_metrics_cloudwatch: bool,
    /// How Object Lambda functions reach the S3 API (`inputS3Url`); default
    /// `http://<public host>:<S3 port>`.
    #[arg(long, env = "ROLL_S3_PUBLIC_ENDPOINT")]
    s3_public_endpoint: Option<String>,
    /// roll storage's developer API (Supabase-Storage-compatible REST, `/storage/v1`) listen
    /// address.
    #[arg(long, env = "ROLL_STORAGE_API", default_value = "127.0.0.1:59001")]
    storage_api: SocketAddr,
    /// The developer API's public URL (`https://storage.example.com/storage/v1`): resumable
    /// upload URLs and the dashboard; default `http://<public host>:<port>/storage/v1`.
    #[arg(long, env = "ROLL_STORAGE_API_PUBLIC_URL")]
    storage_api_public_url: Option<String>,
    /// More endpoints SAML assertions may be for (comma-separated `Recipient`/`Audience`
    /// values), besides `https://signin.aws.amazon.com/saml`.
    #[arg(long, env = "ROLL_S3_SAML_AUDIENCES")]
    s3_saml_audiences: Option<String>,
    /// Let storage webhooks reach loopback and private addresses (single-tenant installs).
    #[arg(long, env = "ROLL_S3_WEBHOOK_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    s3_webhook_allow_private: bool,
    /// Storage's hot-object cache in memory (`256MiB`, `1GiB`; `0` turns the cache off).
    #[arg(long, env = "ROLL_S3_CACHE_MEMORY", default_value = "256MiB")]
    s3_cache_memory: String,
    /// Its disk tier (`10GiB`; `0`: memory only), under ROLL_S3_CACHE_DIR.
    #[arg(long, env = "ROLL_S3_CACHE_DISK", default_value = "10GiB")]
    s3_cache_disk: String,
    /// Where the disk tier lives (default: <home>/storage/cache). Local NVMe is best.
    #[arg(long, env = "ROLL_S3_CACHE_DIR")]
    s3_cache_dir: Option<PathBuf>,
    /// Objects larger than this bypass the cache.
    #[arg(long, env = "ROLL_S3_CACHE_MAX_OBJECT", default_value = "256MiB")]
    s3_cache_max_object: String,
    /// Cache SSE-KMS objects too (S3 would check KMS access on every read; off by default).
    #[arg(long, env = "ROLL_S3_CACHE_KMS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    s3_cache_kms: bool,
    /// The secret the CDN Worker sends in `X-Roll-CDN-Auth` (set the Worker's
    /// ROLL_CDN_ORIGIN_SECRET to the same). Without it, CDN requests are refused.
    #[arg(long, env = "ROLL_CDN_ORIGIN_SECRET", hide_env_values = true)]
    cdn_origin_secret: Option<String>,
    /// Cloudflare's API, for purges (tests point it at a mock).
    #[arg(long, env = "ROLL_CDN_CLOUDFLARE_API", default_value = "https://api.cloudflare.com/client/v4")]
    cdn_cloudflare_api: String,
    /// Don't run roll storage (the S3 API).
    #[arg(long, env = "ROLL_NO_STORAGE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    no_storage: bool,
    #[command(flatten)]
    queues: queues::QueuesArgs,
    #[command(flatten)]
    nosql: nosql::NosqlArgs,
}

fn main() -> anyhow::Result<()> {
    // Return freed memory to the OS right away (mimalloc's default waits 1 s), and have each worker
    // collect its heap when it goes idle: otherwise memory freed after a burst stays with threads
    // that are parked. Measured: no throughput cost; idle footprint after 256 MB of writes 36–56 MB
    // → 24–31 MB (and ~90 MB → ~60 MB with 600 partitions).
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
    nosql_storage::memory::set_allocator_release(|| unsafe { libmimalloc_sys::mi_collect(true) });
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_park(|| unsafe { libmimalloc_sys::mi_collect(false) })
        // Memory a thread frees that another (blocking-pool) thread allocated stays with that
        // thread's heap until it allocates in the same size class again or exits. Blocking
        // threads (file reads, block decoding, flushes) mostly don't: with tokio's 10 s
        // keep-alive a compaction left ~3x its working set pinned, and an unload's memory came
        // back only 10 s later. Idle ones exit after 50 ms instead (measured: no slower).
        .thread_keep_alive(std::time::Duration::from_millis(50))
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,pg_proxy=info,pg_control=info,pg_storage=info,redis_server=info,redis_control=info,stream_broker=info,tasks=info,code_control=info,code_store=info,code_git=info,code_review=info,code_index=info,mq_core=info,mq_amqp=info,mq_cluster=info,mq_mgmt=info,mq_http=info,mq_mqtt=info,mq_stomp=info,mq_shovel=info,nosql_server=info,nosql_control=info,nosql_storage=info,s3_store=info,s3_server=info,s3_control=info".into()),
        )
        .init();
    let args = Args::parse();
    let home = std::path::absolute(&args.home)?;
    let home_abs = home.clone();
    let storage_opts = StorageOpts::from(&args);
    let storage = match &args.s3_bucket {
        Some(bucket) => StorageConfig::S3 {
            bucket: bucket.clone(),
            region: args.s3_region.clone(),
            endpoint: args.s3_endpoint.clone(),
            prefix: args.s3_prefix.clone(),
        },
        None => StorageConfig::Local(home.join("objects")),
    };

    if args.code_only {
        anyhow::ensure!(!args.queues_only, "--code-only and --queues-only can't both be set");
        let node = start_code(&args, &home, &storage, None).await?;
        let listener = tokio::net::TcpListener::bind(args.api).await?;
        info!(addr = %args.api, "roll code API listening");
        let router = node.router(code_control::api::router(node.code.clone()));
        let (close, closed) = tokio::sync::oneshot::channel::<()>();
        let api = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = closed.await;
            })
            .into_future();
        tokio::pin!(api);
        tokio::select! {
            r = &mut api => r?,
            grace = node.shutdown(&args) => finish_requests(api, close, grace).await?,
        }
        info!("shut down");
        return Ok(());
    }

    if args.nosql.nosql_only {
        anyhow::ensure!(!args.nosql.nosql_disabled, "--nosql-only with --no-nosql leaves nothing to run");
        let n = nosql::Nosql::start(&home, args.nosql.clone(), &args.public_host, &args.node, &storage).await?;
        let listener = tokio::net::TcpListener::bind(args.api).await?;
        info!(addr = %args.api, "nosql control API listening");
        tokio::select! {
            r = axum::serve(listener, n.router()) => r?,
            r = n.serve() => r?,
            _ = shutdown_signal() => info!("shutting down"),
        }
        n.shutdown().await?;
        return Ok(());
    }

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

    let mut config = Config::local(home.clone());
    config.public_host = args.public_host.clone();
    config.public_port = args.proxy.port();
    config.mem_mb_per_cu = args.mem_per_cu;
    config.evict_after_s = args.evict_after;
    config.internal_url = args.internal_url.clone().unwrap_or_else(|| format!("http://host.docker.internal:{}", args.api.port()));
    config.storage = storage.clone();
    stream_config.storage = config.storage.clone();

    let control = Control::new(config).await?;
    info!(storage = %control.objects.description, "durable storage");
    reconciler::recover(&control).await?;

    let proxy = Proxy::new(
        api::proxy_router(control.clone()),
        ProxyConfig { listen: args.proxy, default_endpoint: args.default_endpoint.clone() },
    );
    // Serverless Redis: same object storage, its own node.
    let mut rcfg = redis_control::RedisConfig::local(std::path::absolute(&args.home)?.join("redis"));
    rcfg.node_name = args.node.clone();
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

    let (storage_svc, s3_listener) = if args.no_storage {
        (None, None)
    } else {
        let (st, l) = start_storage(&storage_opts, &home_abs, &storage).await?;
        (Some(st), Some(l))
    };
    // roll nosql: same object storage, its own node.
    let nosql = if args.nosql.nosql_disabled { None } else { Some(nosql::Nosql::start(&home, args.nosql.clone(), &args.public_host, &args.node, &storage).await?) };

    let broker = stream_broker::Broker::new(stream_config).await?;
    if let Some(name) = &broker.config.default_stream
        && broker.find_stream(name).is_none()
    {
        broker.create_stream(name, Settings::default())?;
    }
    broker.txns.recover(&broker).await?;
    let kafka = tokio::net::TcpListener::bind(args.kafka).await?;
    // Storage event notifications into roll queues and streams, in process.
    if let Some(st) = &storage_svc {
        let sink = storage_events::RollSink { queues: queues.as_ref().map(|q| q.broker.clone()), streams: broker.clone() };
        st.server.notifier.set_sink(std::sync::Arc::new(sink));
        st.control.set_server(st.server.clone());
    }

    // roll code: same object storage; previews branch roll Postgres and Redis.
    let data: std::sync::Arc<dyn code_preview::DataBranches> = std::sync::Arc::new(code_preview::RollData { postgres: control.clone(), redis: redis.clone() });
    let node = start_code(&args, &home, &storage, Some(data)).await?;

    let listener = tokio::net::TcpListener::bind(args.api).await?;
    info!(addr = %args.api, "control API listening");
    let mut router = api::router(control.clone())
        .merge(redis_control::api::router(redis.clone()))
        .merge(stream_broker::http::router(broker.clone()))
        .merge(tasks::api::router(tasks.clone()))
        .merge(code_control::api::router(node.code.clone()));
    if let Some(q) = &queues {
        router = router.merge(q.router());
    }
    if let Some(st) = &storage_svc {
        router = router.merge(s3_control::api::router(st.control.clone()));
    }
    let storage_serve = async {
        match (&storage_svc, s3_listener) {
            (Some(st), Some((l, api))) => tokio::select! {
                r = s3_server::serve(l, st.server.clone()) => r,
                r = s3_api::serve(api, st.api.clone()) => r,
            },
            _ => std::future::pending().await,
        }
    };
    if let Some(n) = &nosql {
        router = router.merge(n.router());
    }
    let nosql_serve = async {
        match &nosql {
            Some(n) => n.serve().await,
            None => std::future::pending().await,
        }
    };
    let router = node.router(router);
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
        r = storage_serve => r?,
        r = nosql_serve => r?,
        r = stream_broker::server::serve_on(broker.clone(), kafka) => r?,
        _ = reconciler::run(control.clone()) => {}
        _ = broker.clone().run() => {}
        _ = node.shutdown(&args) => {
            info!("shutting down; computes keep running and are adopted on next start");
            // Make every acknowledged Redis write durable before exiting.
            redis.shipper.flush().await?;
            redis.shipper.write_manifests(true).await?;
        }
    }
    if let Some(n) = &nosql {
        n.shutdown().await?;
    }
    if let Some(q) = &queues {
        q.shutdown().await;
    }
    if let Some(st) = &storage_svc {
        // Commit pending metadata and usage before exiting.
        st.server.store.close().await;
    }
    tasks_worker.shutdown().await;
    broker.sync_all().await;
    Ok(())
}

/// roll code, running: the control plane and its previews.
struct CodeNode {
    code: std::sync::Arc<code_control::Code>,
    previews: std::sync::Arc<code_preview::Previews>,
    _checks: tokio::task::JoinHandle<()>,
}

impl CodeNode {
    /// The top-level router: requests for preview hosts go to the preview's container, the rest
    /// to `router`; in a cluster, requests from other nodes are verified first.
    fn router(&self, router: axum::Router) -> axum::Router {
        let router = router.layer(axum::middleware::from_fn_with_state(self.previews.clone(), code_preview::route));
        match self.code.cluster() {
            Some(c) => router.layer(axum::middleware::from_fn_with_state(c.clone(), code_cluster::accept_forwarded)),
            None => router,
        }
    }

    /// Wait for Ctrl-C or SIGTERM; then, in a cluster, hand every repo this node owns to the
    /// others (the API keeps serving meanwhile) before the process exits. Returns how long the
    /// API may then take to finish the requests it still has in flight: what's left of the drain
    /// timeout, between 5 and 10 s (an open event stream holds the API until then).
    async fn shutdown(&self, args: &Args) -> std::time::Duration {
        shutdown_signal().await;
        let started = std::time::Instant::now();
        if self.code.cluster().is_some() {
            info!(timeout_s = args.code.code_drain_timeout_s, "roll code: draining this node");
            self.code.drain(args.code.drain_timeout()).await;
            info!("roll code: drained");
        }
        let left = args.code.drain_timeout().saturating_sub(started.elapsed());
        left.clamp(std::time::Duration::from_secs(5), std::time::Duration::from_secs(10))
    }
}

/// Stop taking connections and let the requests in flight finish, for up to `grace`: requests
/// that passed through this node to a repo's owner on another node (forwarded requests, clones,
/// pushes) would otherwise be cut when the process exits. Event streams don't end by themselves,
/// hence the bound.
async fn finish_requests<F>(api: std::pin::Pin<&mut F>, close: tokio::sync::oneshot::Sender<()>, grace: std::time::Duration) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    let _ = close.send(());
    match tokio::time::timeout(grace, api).await {
        Ok(r) => r,
        Err(_) => {
            tracing::warn!(grace_s = grace.as_secs(), "requests still in flight after the grace period; closing them");
            Ok(())
        }
    }
}

/// roll storage: its store (metadata under `<home>/storage`, blobs next to everything else's
/// durable state), the S3 API and the control plane.
struct Storage {
    server: std::sync::Arc<s3_server::Server>,
    control: std::sync::Arc<s3_control::Control>,
    /// The developer API (`s3-api`).
    api: std::sync::Arc<s3_api::Api>,
}

/// rolld's storage flags (taken before the other products consume theirs).
struct StorageOpts {
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

/// Open roll code on `storage` with its flags, and start its reviewer, webhooks, GitHub App,
/// checks and previews (`data`: what previews branch; none on a roll code-only node).
async fn start_code(
    args: &Args,
    home: &std::path::Path,
    storage: &StorageConfig,
    data: Option<std::sync::Arc<dyn code_preview::DataBranches>>,
) -> anyhow::Result<CodeNode> {
    // Blobs are fsynced before a write returns (a local directory; S3 always is durable).
    let objects = pg_storage::Objects::new_durable(storage)?;
    let code_public = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    let mut code_config = code_control::Config {
        admin_token: args.code_token.clone().filter(|t| !t.is_empty()),
        git_url: args.code_git_url.clone().unwrap_or_else(|| format!("{}/git", code_public.trim_end_matches('/'))),
        api_url: code_public,
        web_url: args.web_url.clone(),
        storage: objects.description.clone(),
        allow_private_webhooks: args.code_webhook_allow_private,
        allow_private_providers: args.code_provider_allow_private,
        // roll's reviewer: ROLL_REVIEW_PROVIDER, ROLL_REVIEW_ANTHROPIC_API_KEY, ROLL_SECRET_KEY, … (docs/code/reviews.md).
        review: code_control::ai::AiConfig::from_env(),
        max_streams_per_caller: args.code_max_streams_per_caller,
        max_streams: args.code_max_streams,
        trust_forwarded_for: args.code_trust_forwarded_for,
        ..code_control::Config::local(home.to_path_buf())
    };
    args.code.apply(&mut code_config, &args.public_host, args.api)?;
    let code = code_control::Code::open(code_config, objects)?;
    match (&code.config.database, code.cluster()) {
        (_, Some(c)) => info!(node = %c.node_id(), url = %c.config().advertise_url, "roll code: cluster node (Postgres)"),
        (Some(_), None) => info!("roll code: one node on Postgres"),
        (None, None) => info!("roll code: one node on SQLite"),
    }
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
        data,
    );
    let checks = code_preview::start(&code, std::sync::Arc::new(code_preview::DockerRunner::new(args.code_runner_concurrency)), previews.clone());
    Ok(CodeNode { code, previews, _checks: checks })
}

async fn start_storage(args: &StorageOpts, home: &std::path::Path, storage: &StorageConfig) -> anyhow::Result<(Storage, (tokio::net::TcpListener, tokio::net::TcpListener))> {
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
    cfg.node = args.node.clone();
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
    let store = s3_store::Store::open(cfg, backend).await?;
    if let (Some(id), Some(secret)) = (&args.s3_access_key, &args.s3_secret_key) {
        store.ensure_key(id, secret, "ROLL_S3_ACCESS_KEY").await?;
    }
    std::fs::create_dir_all(&scfg.spool_dir)?;
    if let Some(c) = &args.s3_internal_cidrs {
        scfg.internal_cidrs = s3_server::parse_cidrs(c)?;
    }
    let endpoint = format!("http://{}:{}", args.public_host, args.s3.port());
    let ccfg = s3_control::ControlConfig { endpoint, region: scfg.region.clone(), virtual_host_domain: args.s3_domain.clone(), website_domain: args.s3_website_domain.clone(), storage_class: class, pricing: Default::default() };
    let control = s3_control::Control::new(store.clone(), ccfg).await?;
    let spool = scfg.spool_dir.clone();
    let server = s3_server::Server::new(store, scfg);
    info!(s3 = %args.s3, storage = %server.store.describe(), "storage (S3 API) listening");
    let api_url = args.storage_api_public_url.clone().unwrap_or_else(|| format!("http://{}:{}{}", args.public_host, args.storage_api.port(), s3_api::PREFIX));
    control.set_api_endpoint(api_url.clone());
    let api = s3_api::Api::new(server.clone(), s3_api::Config { public_url: args.storage_api_public_url.clone(), spool_dir: spool, ..Default::default() });
    info!(api = %args.storage_api, url = api_url, "storage developer API listening");
    Ok((Storage { server, control, api }, (listener, api_listener)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// Every flag has a unique id (clap's own checks), and the roll code cluster flags reach
    /// roll code's config.
    #[test]
    fn flags_are_consistent_and_cluster_flags_configure_roll_code() {
        Args::command().debug_assert();
        let args = Args::parse_from([
            "rolld",
            "--code-only",
            "--code-database",
            "postgres://roll:pw@db:5432/roll",
            "--code-db-connections",
            "8",
            "--code-cluster-secret",
            "0123456789abcdef0123",
            "--code-node-id",
            "n1",
            "--code-git-memory-budget",
            "256",
            "--code-git-max-request-memory",
            "64",
            "--code-git-memory-wait-secs",
            "5",
        ]);
        assert!(args.code_only);
        let mut config = code_control::Config::local(PathBuf::from("/tmp/x"));
        args.code.apply(&mut config, "10.0.0.7", "0.0.0.0:7070".parse().unwrap()).unwrap();
        let d = config.database.as_ref().unwrap();
        assert_eq!((d.connections, d.store_schema.as_str(), d.control_schema.as_str()), (8, "code_store", "code_control"));
        assert!(!format!("{d:?}").contains("pw"), "the password isn't printed");
        let c = config.cluster.as_ref().unwrap();
        assert_eq!((c.node_id.as_str(), c.advertise_url.as_str()), ("n1", "http://10.0.0.7:7070"));
        assert_eq!((config.git_memory_budget_bytes, config.git_max_request_memory_bytes), (256 << 20, Some(64 << 20)));
        assert_eq!(config.git_memory_wait, std::time::Duration::from_secs(5));
        // A cluster needs Postgres; a single node may use it without one.
        let no_db = Args::parse_from(["rolld", "--code-cluster-secret", "0123456789abcdef0123"]);
        assert!(no_db.code.apply(&mut code_control::Config::local(PathBuf::from("/tmp/x")), "h", "0.0.0.0:1".parse().unwrap()).is_err());
        let single = Args::parse_from(["rolld", "--code-database", "postgres://db/roll"]);
        let mut config = code_control::Config::local(PathBuf::from("/tmp/x"));
        single.code.apply(&mut config, "h", "0.0.0.0:1".parse().unwrap()).unwrap();
        assert!(config.database.is_some() && config.cluster.is_none());
    }
}
