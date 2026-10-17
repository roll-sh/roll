use clap::{Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tasks::store::Store;
use tasks::{Config, Tasks};
use tracing::info;

/// roll tasks: durable messages, queues, schedules and workflows over HTTP.
///
/// One binary, several roles:
///
/// - `all` (default): API and workers on one store (SQLite locally).
/// - `node` + `--store log`: a cell node on the log store in S3. The leader serves and works
///   messages; the others stand by and forward requests to it.
/// - `proxy`: forwards the API to the cell's leader (on Lambda, the function URL front door).
/// - `api` / `worker`: the DynamoDB layout, API on Lambda and workers on Fargate.
#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, env = "ROLL_TASKS_ROLE", value_enum, default_value_t = Role::All)]
    role: Role,
    #[arg(long, env = "ROLL_TASKS_LISTEN", default_value = "127.0.0.1:7071")]
    listen: SocketAddr,
    #[arg(long, env = "ROLL_TASKS_STORE", value_enum, default_value_t = StoreKind::Sqlite)]
    store: StoreKind,
    /// SQLite database file.
    #[arg(long, env = "ROLL_TASKS_SQLITE", default_value = ".roll/tasks.db")]
    sqlite: PathBuf,
    /// Log store location: `s3://bucket/prefix`, `s3express://bucket/prefix`, `file:///dir`.
    #[arg(long, env = "ROLL_TASKS_LOG", default_value = "file://.roll/tasks-log")]
    log: String,
    /// Put the log's commits in their own bucket, e.g. `s3express://<name>--<az-id>--x-s3/tasks`:
    /// S3 Express One Zone PUTs cost about a quarter, but the commits since the last snapshot then
    /// live in one AZ. Snapshots and the lease stay in `--log`.
    #[arg(long, env = "ROLL_TASKS_WAL")]
    wal: Option<String>,
    /// Local working directory of a log store node.
    #[arg(long, env = "ROLL_TASKS_DATA", default_value = ".roll/tasks-node")]
    data: PathBuf,
    /// This node's private `host:port` for the others (default: from ECS metadata, else the
    /// listening address).
    #[arg(long, env = "ROLL_TASKS_ADVERTISE")]
    advertise: Option<String>,
    /// Minimum time between the starts of two log commits. Every commit is one S3 PUT, so this
    /// caps the storage bill at 1000 / commit_ms PUTs a second, however busy the cell; API writes
    /// wait up to this long (plus the PUT) for theirs.
    #[arg(long, env = "ROLL_TASKS_COMMIT_MS", default_value_t = 100)]
    commit_ms: u64,
    /// Worker writes (delivery outcomes, leases) are committed within this long.
    #[arg(long, env = "ROLL_TASKS_RELAXED_MS", default_value_t = 5000)]
    relaxed_ms: u64,
    /// Due-index shards of each hosted (non-default) tenant on a log store cell.
    #[arg(long, env = "ROLL_TASKS_TENANT_SHARDS", default_value_t = 2)]
    tenant_shards: u32,
    /// DynamoDB table.
    #[arg(long, env = "ROLL_TASKS_TABLE", default_value = "roll-tasks")]
    table: String,
    /// DynamoDB endpoint (DynamoDB Local, LocalStack …); default: AWS.
    #[arg(long, env = "ROLL_TASKS_DYNAMO_ENDPOINT")]
    dynamo_endpoint: Option<String>,
    /// Create the table if it's missing (dev; on AWS the stack creates it).
    #[arg(long, env = "ROLL_TASKS_CREATE_TABLE", value_parser = clap::builder::BoolishValueParser::new())]
    create_table: bool,
    /// S3 bucket for bodies too large to keep inline.
    #[arg(long, env = "ROLL_TASKS_BUCKET")]
    bucket: Option<String>,
    #[arg(long, env = "ROLL_TASKS_S3_ENDPOINT")]
    s3_endpoint: Option<String>,
    /// Bodies up to this many bytes stay in DynamoDB.
    #[arg(long, env = "ROLL_TASKS_INLINE_MAX", default_value_t = 32 << 10)]
    inline_max: usize,
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
    /// Shared due-index shards. Each handles roughly 300 messages/s on DynamoDB; raise to scale.
    #[arg(long, env = "ROLL_TASKS_SHARDS", default_value_t = 16)]
    shards: u32,
    /// What carries API calls to the cell, `lambda` or `nlb` (prices usage).
    #[arg(long, env = "ROLL_TASKS_FRONT_DOOR", default_value = "lambda")]
    front_door: tasks::cost::FrontDoor,
    /// Admin API token. Without one, the API is open (local use).
    #[arg(long, env = "ROLL_TASKS_TOKEN")]
    token: Option<String>,
    /// Refuse private and link-local destinations (always on in hosted deployments).
    #[arg(long, env = "ROLL_TASKS_BLOCK_PRIVATE", value_parser = clap::builder::BoolishValueParser::new())]
    block_private_urls: bool,
    #[arg(long, env = "ROLL_TASKS_MAX_INFLIGHT", default_value_t = 1000)]
    max_inflight: usize,
    /// Longest idle wait before a worker re-checks a lane.
    #[arg(long, env = "ROLL_TASKS_POLL_MS", default_value_t = 500)]
    poll_ms: i64,
    /// How long finished messages, runs and events are kept.
    #[arg(long, env = "ROLL_TASKS_RETENTION", default_value = "7d")]
    retention: String,
    /// Largest message body.
    #[arg(long, env = "ROLL_TASKS_MAX_BODY", default_value_t = 1 << 20)]
    max_body: usize,
    /// Seconds to let deliveries finish on shutdown before handing them back.
    #[arg(long, env = "ROLL_TASKS_DRAIN_S", default_value_t = 25)]
    drain_s: i64,
    /// Print CloudWatch EMF metrics (worker saturation) for autoscaling.
    #[arg(long, env = "ROLL_TASKS_EMF_METRICS", value_parser = clap::builder::BoolishValueParser::new())]
    emf_metrics: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Role {
    All,
    Api,
    Worker,
    Node,
    Proxy,
}

#[derive(Subcommand)]
enum Command {
    /// Put the log store at `--log` back to how it was at a moment, from the versions its
    /// (versioned) S3 bucket keeps. Stop the cell first. Shows what it would do unless `--apply`.
    Restore {
        /// RFC 3339 (`2026-10-17T09:30:00Z`) or unix seconds.
        #[arg(long)]
        at: String,
        #[arg(long)]
        apply: bool,
        /// Run even though a node holds the lease (it crashed and can't release it).
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StoreKind {
    Sqlite,
    Log,
    Dynamo,
}

async fn open_store(a: &Args) -> anyhow::Result<Arc<dyn Store>> {
    Ok(match a.store {
        StoreKind::Sqlite => Arc::new(tasks::store::sqlite::SqliteStore::open(&a.sqlite, a.shards.min(4))?),
        StoreKind::Log => anyhow::bail!("the log store runs as a cell: use --role node (and --role proxy in front)"),
        #[cfg(feature = "dynamo")]
        StoreKind::Dynamo => Arc::new(
            tasks::store::dynamo::DynamoStore::connect(tasks::store::dynamo::DynamoConfig {
                table: a.table.clone(),
                endpoint: a.dynamo_endpoint.clone(),
                region: a.region.clone(),
                shards: a.shards,
                bucket: a.bucket.clone(),
                s3_endpoint: a.s3_endpoint.clone(),
                inline_max: a.inline_max,
                create_table: a.create_table,
            })
            .await?,
        ),
        #[cfg(not(feature = "dynamo"))]
        StoreKind::Dynamo => anyhow::bail!("built without the dynamo feature"),
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tasks=info".into()))
        .init();
    let a = Args::parse();
    if let Some(Command::Restore { at, apply, force }) = &a.command {
        return restore(&a, at, *apply, *force).await;
    }
    let config = Config {
        poll_ms: a.poll_ms,
        max_inflight: a.max_inflight,
        token: a.token.clone().filter(|t| !t.is_empty()),
        block_private_urls: a.block_private_urls,
        retention_ms: tasks::schedule::parse_duration_ms(&a.retention).map_err(anyhow::Error::msg)?,
        max_body_bytes: a.max_body,
        drain_ms: a.drain_s * 1000,
        emf_metrics: a.emf_metrics,
        front_door: a.front_door,
        log_express: a.wal.as_deref().is_some_and(|u| u.starts_with("s3express://")),
        ..Config::default()
    };
    if a.role == Role::Node || a.role == Role::Proxy {
        return cell(a, config).await;
    }
    let store = open_store(&a).await?;
    info!(store = %store.describe(), "roll tasks");
    let tasks = Tasks::new(config, store).await?;

    #[cfg(feature = "lambda")]
    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
        // On Lambda the API is the function; workers run elsewhere.
        return lambda_http::run(tasks::api::router(tasks)).await.map_err(|e| anyhow::anyhow!(e));
    }

    let worker = (a.role != Role::Api).then(|| tasks::start(tasks.clone()));
    let serve = async {
        if a.role == Role::Worker {
            std::future::pending::<()>().await;
            return Ok(());
        }
        let listener = tokio::net::TcpListener::bind(a.listen).await?;
        info!(addr = %a.listen, "API listening");
        axum::serve(listener, tasks::api::router(tasks.clone())).await
    };
    tokio::select! {
        r = serve => r?,
        _ = shutdown_signal() => info!("shutting down"),
    }
    if let Some(w) = worker {
        w.shutdown().await;
    }
    tasks.flush_usage().await?;
    Ok(())
}

#[cfg(feature = "restore")]
async fn restore(a: &Args, at: &str, apply: bool, force: bool) -> anyhow::Result<()> {
    anyhow::ensure!(a.wal.as_deref().unwrap_or("").is_empty(), "restore needs the whole log in one versioned bucket (no --wal)");
    let at = tasks::restore::parse_time(at)?;
    let s = tasks::restore::run(&a.log, at, apply, force).await?;
    if s.applied {
        println!("restored {} objects and deleted {} newer ones; start the cell again", s.restored, s.deleted);
    } else if s.restored + s.deleted == 0 {
        println!("nothing to do: the log already is as it was then");
    } else {
        println!("would restore {} objects and delete {} newer ones; run again with --apply", s.restored, s.deleted);
    }
    Ok(())
}

#[cfg(not(feature = "restore"))]
async fn restore(_: &Args, _: &str, _: bool, _: bool) -> anyhow::Result<()> {
    anyhow::bail!("built without the restore feature")
}

/// The log store layout: a node of the cell, or a proxy in front of it.
async fn cell(a: Args, config: Config) -> anyhow::Result<()> {
    let (objects, prefix) = tasks::store::log::objects_from_url(&a.log)?;
    if a.role == Role::Proxy {
        let proxy = Arc::new(tasks::cluster::Proxy::new(objects, prefix));
        #[cfg(feature = "lambda")]
        if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
            return lambda_http::run(proxy.router()).await.map_err(|e| anyhow::anyhow!(e));
        }
        let listener = tokio::net::TcpListener::bind(a.listen).await?;
        info!(addr = %a.listen, log = a.log, "proxy listening");
        return Ok(axum::serve(listener, proxy.router()).with_graceful_shutdown(shutdown_signal()).await?);
    }
    let mut log = tasks::store::log::LogConfig::new(objects, prefix, a.data.clone());
    if let Some(url) = a.wal.as_deref().filter(|u| !u.is_empty()) {
        log.wal = Some(tasks::store::log::objects_from_url(url)?);
    }
    // One SQLite writer either way: a few lanes are plenty.
    log.shards = a.shards.min(4);
    log.tenant_shards = a.tenant_shards.clamp(1, 4);
    log.commit_ms = a.commit_ms;
    log.relaxed_ms = a.relaxed_ms;
    let mut node = tasks::cluster::NodeConfig::new(log, config);
    node.advertise = match a.advertise.clone() {
        Some(addr) => Some(addr),
        None => tasks::cluster::ecs_address().await.map(|ip| format!("{ip}:{}", a.listen.port())),
    };
    let listener = tokio::net::TcpListener::bind(a.listen).await?;
    info!(addr = %a.listen, log = a.log, "node listening");
    let (stop, stopped) = tokio::sync::watch::channel(tasks::cluster::Stop::Run);
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutting down");
        let _ = stop.send(tasks::cluster::Stop::Graceful);
    });
    match tasks::cluster::run_node(listener, node, stopped).await? {
        tasks::cluster::Exit::Stopped => Ok(()),
        // Exit non-zero so the scheduler starts a fresh standby.
        other => anyhow::bail!("node stopped: {other:?}"),
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
