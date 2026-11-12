use clap::Parser;
use pg_control::{Config, Control, api, reconciler};
use pg_proxy::{Proxy, ProxyConfig};
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::info;

/// mimalloc hands freed memory back to the OS; the system allocator on macOS keeps it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `mi_option_purge_delay` (libmimalloc-sys doesn't export the constant).
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

mod auth;
mod cluster;
mod code;
mod edge;
mod isolation;
mod logs;
mod nosql;
mod products;
mod queues;
mod storage;
mod redis;
mod search;
mod storage_events;
mod streams;
mod tasks;
mod workspace;

use products::{Product, Products};

/// roll daemon: serverless Postgres, serverless Redis, S3-compatible storage, Kafka-compatible streams, tasks,
/// RabbitMQ-compatible queues, ScyllaDB/Cassandra-compatible nosql and Elasticsearch-compatible search on your
/// machine (or an EC2 host).
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
    /// Require an API key on the HTTP stream data plane (always on with ROLL_ADMIN_TOKEN).
    #[arg(long, env = "ROLL_STREAM_REQUIRE_AUTH")]
    stream_require_auth: bool,
    /// Local disk target for stream data across open partitions, in MiB (0 = only per-topic
    /// `local.retention.*`). Size the volume for this plus a few seconds of ingest.
    #[arg(long, env = "ROLL_STREAM_DISK_MB", default_value_t = 0)]
    stream_disk_mb: u64,
    /// Keep closed partitions' data on local disk instead of serving it from object storage.
    #[arg(long, env = "ROLL_STREAM_KEEP_LOCAL")]
    stream_keep_local: bool,
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
    /// roll code checks running at once from this node (each in its own container or Fargate
    /// task; `ROLL_CODE_RUNNER`, see `isolation.rs`).
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
    tasks: tasks::TasksArgs,
    #[command(flatten)]
    nosql: nosql::NosqlArgs,
    #[command(flatten)]
    edge: edge::EdgeArgs,
    #[command(flatten)]
    streams: streams::StreamArgs,
    #[command(flatten)]
    search: search::SearchArgs,
    /// Products this node runs, comma-separated: postgres, redis, streams, tasks, storage, nosql,
    /// queues, code, search (default: all). `--no-<product>` and `--<product>-only` narrow it.
    #[arg(long, env = "ROLL_PRODUCTS", value_enum, value_delimiter = ',')]
    products: Option<Vec<Product>>,
    /// `text`, or `json`: one JSON object per line, with an access line per HTTP request and
    /// CloudWatch metrics (Embedded Metric Format).
    #[arg(long, env = "ROLL_LOG_FORMAT", value_enum, default_value_t = logs::LogFormat::Text)]
    log_format: logs::LogFormat,
}

impl Args {
    /// The products this node runs: one set, from `--products` and the older switches.
    /// The system database's connections per product pool on this node
    /// (`ROLL_SYSTEM_DB_CONNECTIONS`, split by [`cluster::DbBudget::split`]).
    fn db_budget(&self) -> anyhow::Result<cluster::DbBudget> {
        Ok(cluster::DbBudget::split(self.code.cluster.system_db_connections, self.products()?))
    }

    fn products(&self) -> anyhow::Result<Products> {
        Products::resolve(products::Switches {
            products: self.products.clone(),
            no_storage: self.no_storage,
            no_queues: self.queues.disabled,
            no_nosql: self.nosql.nosql_disabled,
            no_search: self.search.search_disabled,
            code_only: self.code_only,
            queues_only: self.queues_only,
            nosql_only: self.nosql.nosql_only,
            search_only: self.search.search_only,
        })
    }
}

/// Every socket is a file descriptor: client connections, each product's pools to S3, DynamoDB
/// and Postgres, node-to-node hops. Containers start with a soft limit of 1024, which a fleet node
/// ran out of while draining under the fleet test (`Too many open files`, then nothing could
/// reach the next owner); the hard limit is the real ceiling, so take it, as Go's runtime does.
fn raise_open_files_limit() {
    let mut rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit and setrlimit only read and write the struct passed.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 && rl.rlim_cur < rl.rlim_max {
            rl.rlim_cur = rl.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
        }
    }
}

fn main() -> anyhow::Result<()> {
    // Return freed memory to the OS right away (mimalloc's default waits 1 s), and have each worker
    // collect its heap when it goes idle: otherwise memory freed after a burst stays with threads
    // that are parked. Measured: no throughput cost; idle footprint after 256 MB of writes 36–56 MB
    // → 24–31 MB (and ~90 MB → ~60 MB with 600 partitions).
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
    nosql_storage::memory::set_allocator_release(|| unsafe { libmimalloc_sys::mi_collect(true) });
    raise_open_files_limit();
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
    let mut args = Args::parse();
    logs::init(args.log_format);
    edge::configure(&mut args);
    let auth = auth::Auth::new(&args)?;
    let on = args.products()?;
    info!(products = %on, "starting");
    let home = std::path::absolute(&args.home)?;
    let storage_opts = storage::StorageOpts::from(&args);
    let storage = match &args.s3_bucket {
        Some(bucket) => StorageConfig::S3 {
            bucket: bucket.clone(),
            region: args.s3_region.clone(),
            endpoint: args.s3_endpoint.clone(),
            prefix: args.s3_prefix.clone(),
        },
        None => StorageConfig::Local(home.join("objects")),
    };

    // Each product on starts here (ROLL_PRODUCTS); one that's off binds nothing and runs nothing.
    let postgres = match on.on(Product::Postgres) {
        true => Some(start_postgres(&args, &home, &storage).await?),
        false => None,
    };
    let mut redis = match on.on(Product::Redis) {
        true => Some(redis::Redis::start(&args, &storage, redis::Redis::clustered(&args)).await?),
        false => None,
    };
    // roll code: same object storage; previews branch roll Postgres and Redis.
    let code = match on.on(Product::Code) {
        true => {
            let data = code_preview::RollData { postgres: postgres.as_ref().map(|p| p.control.clone()), redis: redis.as_ref().map(|r| r.control.clone()) };
            Some(start_code(&args, &auth, &home, &storage, std::sync::Arc::new(data)).await?)
        }
        false => None,
    };
    let cluster = cluster::handle(code.as_ref().map(|c| &*c.code), &args.code, &args.code.default_url(&args.public_host, args.api))?;
    let metrics = logs::ClusterMetrics::start(cluster.clone());
    // roll streams: one owner per stream in a cluster, so it starts once the cluster is joined.
    let mut streams = match on.on(Product::Streams) {
        true => Some(streams::start(&args, &auth, &home, &storage, &cluster).await?),
        false => None,
    };
    // roll tasks on the node's cluster: every node serves it, the cell's leader runs it.
    let tasks = match on.on(Product::Tasks) {
        true => Some(tasks::start(&args.tasks, &auth, &home, &storage, cluster.clone()).await?),
        false => None,
    };
    if let Some(r) = &redis {
        r.join(&cluster);
    }
    // roll nosql: same object storage; in a cluster its databases follow their leases.
    let nosql = match on.on(Product::Nosql) {
        true => Some(nosql::Nosql::start(&home, args.nosql.clone(), &args.public_host, &args.node, &storage, Some(&cluster), args.code.cluster.system_database.as_deref(), args.db_budget()?.nosql, edge::Public::of(&args).as_ref()).await?),
        false => None,
    };
    // roll queues on the node's cluster: each queue runs on its lease's holder.
    let queues = match on.on(Product::Queues) {
        true => Some(queues::Queues::start(&home, args.queues.clone(), &args.public_host, cluster.clone(), args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty()), &storage, edge::Public::of(&args).as_ref()).await?),
        false => None,
    };

    // roll storage on the node's cluster: every node serves it, the metadata's owner runs it, with
    // its event notifications into roll queues and streams, in process.
    let mut storage_svc = match on.on(Product::Storage) {
        true => {
            let sink = std::sync::Arc::new(storage_events::RollSink { queues: queues.as_ref().map(|q| q.broker.clone()), streams: streams.as_ref().map(|s| s.broker.clone()) });
            Some(storage::start(&storage_opts, &home, &storage, cluster.clone(), sink).await?)
        }
        false => None,
    };

    // roll search: same object storage; the Elasticsearch REST listener and `/v1/search`.
    let search = match on.on(Product::Search) {
        true => Some(search::Search::start(&home, &args.search, &storage, &args.public_host, &args.node, &cluster, args.code.cluster.system_database.as_deref(), edge::Public::of(&args).as_ref()).await?),
        false => None,
    };

    // The internal API: every product's routes, and a clear 404 for the ones that are off.
    let mut router = axum::Router::new();
    if let Some(p) = &postgres {
        router = router.merge(api::router(p.control.clone()));
    }
    if let Some(r) = &redis {
        router = router.merge(redis_control::api::router(r.control.clone()));
    }
    if let Some(s) = &streams {
        router = router.merge(stream_broker::http::router(s.broker.clone()));
    }
    if let Some(t) = &tasks {
        router = router.merge(t.api_routes());
    }
    if let Some(c) = &code {
        router = router.merge(code_control::api::router(c.code.clone()));
    }
    if let Some(q) = &queues {
        router = router.merge(q.router());
    }
    if let Some(st) = &storage_svc {
        router = router.merge(st.api_routes());
    }
    if let Some(n) = &nosql {
        router = router.merge(n.router());
    }
    if let Some(s) = &search {
        router = router.merge(s.router());
    }
    // Workspace projects: always on, behind the same operator token.
    router = router.merge(roll_workspace::router(workspace::open(&args, &home)?));
    // The operator token in front of every product's routes (auth.rs); `/v1/info` and the 404s of
    // products that are off answer before it. The gated router merges last so its fallback (401
    // without the token) stays the fallback.
    let router = on.info_route(cluster.node_id().to_string()).merge(on.off_routes()).merge(auth.gate("api", router));
    let mut router = auth.cors(router).layer(axum::middleware::from_fn(logs::access));
    if let Some(c) = &code {
        router = c.router(router);
    }
    // In a cluster, requests from other nodes are verified first.
    if cluster.backend_name() != "memory" {
        router = router.layer(axum::middleware::from_fn_with_state(cluster.clone(), roll_cluster::accept_forwarded));
    }
    // roll cloud's public listeners (ROLL_EDGE); the internal API gains /healthz.
    let products = edge::Products {
        api: router.clone(),
        pg: postgres.as_ref().map(|p| p.proxy.clone()),
        redis: redis.as_ref(),
        streams: streams.as_ref().map(|s| s.broker.clone()),
        tasks: tasks.as_ref().map(|t| t.router()),
        storage: storage_svc.as_ref().map(|s| s.s3_router()),
        nosql: nosql.as_ref(),
        queues: queues.as_ref(),
        search: search.as_ref().map(|s| s.rest_router()),
        auth: &auth,
    };
    // Connections other nodes splice here, on every cluster node (the edge or not).
    let splice = edge::splice_routes(&cluster, &products)?;
    let edge = edge::Edge::start(&args, &storage, products, std::sync::Arc::new(edge::ClusterSingleton(cluster.clone()))).await?;
    let mut router = edge::internal(router, edge.as_ref());
    if let Some(s) = splice {
        router = router.merge(s);
    }
    if let Some(r) = &redis {
        router = r.internal(router);
    }
    if let Some(s) = &search {
        router = s.internal(router);
    }

    // Everything that serves until it fails; any of them ending ends the process.
    type Serving<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + 'a>>;
    let mut serving: Vec<Serving<'_>> = vec![Box::pin(edge::serve(edge))];
    if let Some(p) = &postgres {
        serving.push(Box::pin(p.proxy.clone().serve_shared()));
        serving.push(Box::pin(async {
            reconciler::run(p.control.clone()).await;
            Ok(())
        }));
    }
    let redis_listeners = redis.as_mut().and_then(|r| r.listeners.take());
    if let (Some(r), Some((resp, http))) = (&redis, redis_listeners) {
        serving.push(Box::pin(r.serve_resp(resp)));
        serving.push(Box::pin(r.serve_rest(http)));
    }
    let kafka = streams.as_mut().and_then(|s| s.kafka.take());
    if let (Some(s), Some(kafka)) = (&streams, kafka) {
        serving.push(Box::pin(stream_broker::server::serve_on(s.broker.clone(), kafka)));
        serving.push(Box::pin(async {
            s.broker.clone().run().await;
            Ok(())
        }));
    }
    if let Some(st) = storage_svc.as_mut() {
        let serve = st.serve();
        serving.push(Box::pin(async move { Ok(serve.await?) }));
    }
    if let Some(q) = &queues {
        serving.push(Box::pin(q.serve()));
    }
    if let Some(n) = &nosql {
        serving.push(Box::pin(n.serve()));
    }
    if let Some(s) = &search {
        serving.push(Box::pin(s.serve()));
    }

    let listener = tokio::net::TcpListener::bind(args.api).await?;
    info!(addr = %args.api, products = %on, "control API listening");
    let (close, closed) = tokio::sync::oneshot::channel::<()>();
    // Client addresses reach handlers (roll code counts anonymous event streams per address).
    let api = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = closed.await;
        })
        .into_future();
    tokio::pin!(api);
    tokio::select! {
        r = &mut api => r?,
        (r, ..) = futures::future::select_all(serving) => r?,
        grace = stop(code.as_ref(), queues.as_ref(), &cluster, &args) => {
            metrics.flush();
            // Queue clients go while the API finishes its requests: gradually, so they don't
            // all reconnect at once (a node alone closes them quickly, as it always did).
            let clustered = cluster.backend_name() != "memory";
            let close_clients = async {
                if let Some(q) = &queues {
                    q.close_clients(if clustered { grace } else { std::time::Duration::from_secs(2) }).await;
                }
            };
            let (finished, ()) = tokio::join!(finish_requests(api, close, grace), close_clients);
            finished?;
            if postgres.is_some() {
                info!("computes keep running and are adopted on next start");
            }
            if let Some(r) = &redis {
                r.shutdown().await?;
            }
        }
    }
    if let Some(n) = &nosql {
        n.shutdown().await?;
    }
    if let Some(s) = &search {
        s.shutdown().await;
    }
    if let Some(q) = &queues {
        q.shutdown().await;
    }
    if let Some(st) = &storage_svc {
        st.shutdown().await;
    }
    if let Some(t) = &tasks {
        t.shutdown().await;
    }
    if let Some(s) = &streams {
        s.broker.sync_all().await;
    }
    info!("shut down");
    Ok(())
}

/// roll postgres: the control plane (computes in Docker) and the proxy.
struct Postgres {
    control: std::sync::Arc<Control>,
    proxy: std::sync::Arc<Proxy<pg_control::control::ProxyRouter>>,
}

async fn start_postgres(args: &Args, home: &std::path::Path, storage: &StorageConfig) -> anyhow::Result<Postgres> {
    let mut config = Config::local(home.to_path_buf());
    config.public_host = args.public_host.clone();
    config.public_port = args.proxy.port();
    config.mem_mb_per_cu = args.mem_per_cu;
    config.evict_after_s = args.evict_after;
    config.internal_url = args.internal_url.clone().unwrap_or_else(|| format!("http://host.docker.internal:{}", args.api.port()));
    config.storage = storage.clone();
    // Behind the edge, connection strings name the endpoint's public hostname.
    if let Some(p) = edge::Public::of(args) {
        p.postgres(&mut config);
    }
    let control = Control::new(config).await?;
    info!(storage = %control.objects.description, "durable storage");
    reconciler::recover(&control).await?;
    let proxy = std::sync::Arc::new(Proxy::new(api::proxy_router(control.clone()), ProxyConfig { listen: args.proxy, default_endpoint: args.default_endpoint.clone() }));
    Ok(Postgres { control, proxy })
}

/// roll code, running: the control plane and its previews.
struct CodeNode {
    code: std::sync::Arc<code_control::Code>,
    previews: std::sync::Arc<code_preview::Previews>,
    _checks: tokio::task::JoinHandle<()>,
}

impl CodeNode {
    /// Requests for preview hosts go to the preview's container, the rest to `router`.
    fn router(&self, router: axum::Router) -> axum::Router {
        router.layer(axum::middleware::from_fn_with_state(self.previews.clone(), code_preview::route))
    }
}

/// Wait for Ctrl-C or SIGTERM; then, in a cluster, hand everything this node owns to the others
/// (the API keeps serving meanwhile) before the process exits. With the edge, that runs once the
/// load balancer has let go and the public listeners stopped accepting (`edge::stopping`).
/// Returns how long the API may then take to finish the requests it still has in flight: what's
/// left of the drain timeout, between 5 and 10 s (an open event stream holds the API until then).
async fn stop(code: Option<&CodeNode>, queues: Option<&queues::Queues>, cluster: &roll_cluster::Cluster, args: &Args) -> std::time::Duration {
    shutdown_signal().await;
    let started = std::time::Instant::now();
    let timeout = args.code.drain_timeout();
    let clustered = cluster.backend_name() != "memory";
    let grace = move || timeout.saturating_sub(started.elapsed()).clamp(std::time::Duration::from_secs(5), std::time::Duration::from_secs(10));
    let products = async {
        if !clustered {
            return;
        }
        match code {
            // roll code stops its own duties too.
            Some(c) if c.code.cluster().is_some() => {
                info!(timeout_s = timeout.as_secs(), "roll code: draining this node");
                c.code.drain(timeout).await;
                info!("roll code: drained");
            }
            _ => {
                info!(timeout_s = timeout.as_secs(), "draining this node");
                cluster.drain(timeout).await;
                info!("drained");
            }
        }
        // Behind the edge, queue clients are the edge's connections served here: queues closes
        // them gradually (they reconnect to other nodes) before the edge closes what's left.
        if args.edge.enabled
            && let Some(q) = queues
        {
            q.close_clients(grace()).await;
        }
    };
    let edge = edge::stopping(products).await;
    if clustered && !edge {
        // Without the edge, a cluster node fails its health check when its drain starts and keeps
        // serving until the load balancer has stopped sending to it (docs/code/cluster.md).
        tokio::time::sleep(cluster::DEREGISTER.saturating_sub(started.elapsed())).await;
    }
    grace()
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


/// Open roll code on `storage` with its flags, and start its reviewer, webhooks, GitHub App,
/// checks and previews (`data`: what previews branch).
async fn start_code(
    args: &Args,
    auth: &auth::Auth,
    home: &std::path::Path,
    storage: &StorageConfig,
    data: std::sync::Arc<dyn code_preview::DataBranches>,
) -> anyhow::Result<CodeNode> {
    // Blobs are fsynced before a write returns (a local directory; S3 always is durable).
    let objects = pg_storage::Objects::new_durable(storage)?;
    let code_public = args.code_public_url.clone().unwrap_or_else(|| format!("http://{}:{}", args.public_host, args.api.port()));
    let mut code_config = code_control::Config {
        admin_token: args.code_token.clone().filter(|t| !t.is_empty()),
        operator_token: auth.token(),
        git_url: args.code_git_url.clone().unwrap_or_else(|| format!("{}/git", code_public.trim_end_matches('/'))),
        api_url: code_public,
        web_url: args.web_url.clone(),
        storage: objects.description.clone(),
        allow_private_webhooks: args.code_webhook_allow_private,
        allow_private_providers: args.code_provider_allow_private,
        // roll's reviewer: ROLL_REVIEW_PROVIDER, ROLL_REVIEW_ANTHROPIC_API_KEY, ROLL_SECRET_KEY, … (docs/code/reviews.md).
        review: code_control::ai::AiConfig::from_env(),
        // roll code agents' sandboxes: ROLL_SANDBOX_BACKEND, ROLL_AGENT37_API_KEY, … (docs/code/agents.md).
        sandboxes: code_control::agents::sandbox::SandboxConfig::from_env()?,
        max_streams_per_caller: args.code_max_streams_per_caller,
        max_streams: args.code_max_streams,
        trust_forwarded_for: args.code_trust_forwarded_for,
        ..code_control::Config::local(home.to_path_buf())
    };
    args.code.apply(&mut code_config, &args.public_host, args.api)?;
    if args.code.cluster.system_db_connections.is_some()
        && let Some(d) = code_config.database.as_mut()
    {
        d.connections = args.db_budget()?.code;
    }
    // Docker on this host, or a Fargate task per run (ROLL_CODE_RUNNER; isolation.rs).
    let isolation = isolation::setup(home, &mut code_config.sandboxes).await?;
    let code = code_control::Code::open(code_config, objects)?;
    match (&code.config.database, code.cluster()) {
        (_, Some(c)) => info!(node = %c.node_id(), url = %c.config().advertise_url, backend = c.backend_name(), "roll code: cluster node (data on Postgres)"),
        (Some(_), None) => info!("roll code: one node on Postgres"),
        (None, None) => info!("roll code: one node on SQLite"),
    }
    code.start_reviewer();
    // Webhook deliveries pick up where they left off (landings while the daemon was down too).
    code.start_webhooks();
    // Sandboxes' reconcilers (on a cluster each starts where its owner's agents are).
    if let Some(b) = code.sandboxes.backend_name() {
        info!(backend = b, "roll code: sandboxes on");
    }
    code.start_sandboxes();
    // Linear: roll's OAuth app (ROLL_LINEAR_CLIENT_ID, ROLL_LINEAR_CLIENT_SECRET,
    // ROLL_LINEAR_WEBHOOK_SECRET; docs/code/automations.md "Linear"). API keys work without it.
    let linear = code_control::agents::linear::LinearConfig::from_env();
    if linear.oauth().is_some() {
        info!(webhook = linear.webhook_secret.is_some(), "roll code: Linear app configured");
    }
    code.configure_linear(linear);
    code.start_automations();
    match &code.config.review.hosted {
        Some(h) => info!(provider = %h.provider, "roll code: hosted reviewer configured"),
        None => info!("roll code: no hosted reviewer (owners can bring their own key)"),
    }
    // GitHub-connected repos, when ROLL_GITHUB_APP_ID and friends are set (docs/code/github-app.md).
    if code_control::github::enable_from_env(&code)? {
        info!("roll code: GitHub App configured");
    }
    // Slack and Discord for the agents, when ROLL_SLACK_* / ROLL_DISCORD_* are set (docs/code/chatops.md).
    if code_control::chatops::enable_from_env(&code)? {
        info!("roll code: chat ops configured");
    }
    // Checks and previews: containers or Fargate tasks, with previews branching roll Postgres and Redis.
    let previews = code_preview::Previews::new(
        code_preview::PreviewConfig {
            domain: args.code_preview_domain.trim_matches('.').to_ascii_lowercase(),
            port: Some(args.api.port()),
            ..Default::default()
        },
        Some(data),
        isolation.clone(),
    );
    let runner = code_preview::CheckRunner::new(isolation, args.code_runner_concurrency);
    let checks = code_preview::start(&code, std::sync::Arc::new(runner), previews.clone());
    Ok(CodeNode { code, previews, _checks: checks })
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
        // The stack sets only the system database: code's data goes there, unless named.
        let system = Args::parse_from(["rolld", "--system-database", "postgres://sys/roll", "--cluster-secret", "0123456789abcdef0123"]);
        let mut config = code_control::Config::local(PathBuf::from("/tmp/x"));
        system.code.apply(&mut config, "h", "0.0.0.0:1".parse().unwrap()).unwrap();
        assert_eq!(config.database.unwrap().url, "postgres://sys/roll");
        let both = Args::parse_from(["rolld", "--system-database", "postgres://sys/roll", "--code-database", "postgres://code/roll"]);
        let mut config = code_control::Config::local(PathBuf::from("/tmp/x"));
        both.code.apply(&mut config, "h", "0.0.0.0:1".parse().unwrap()).unwrap();
        assert_eq!(config.database.unwrap().url, "postgres://code/roll");
    }
}
