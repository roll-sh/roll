//! roll queues inside rolld: the broker, its protocol listeners and its control-plane routes.
//!
//! Alone (the memory cluster backend), a node keeps metadata in SQLite and queue logs on local
//! disk. On a cluster (docs/queues/architecture.md, "Many nodes") every node accepts every
//! client; each durable queue runs on the holder of its roll-cluster lease (`mq:<vhost>/<queue>`)
//! and the others reach it over the internal API. Metadata is in the system database
//! (`ROLL_SYSTEM_DATABASE`) and logs in S3 (the node's bucket, under `queues/logs`), so queues
//! move between nodes without clients reconnecting and without losing a confirmed message.

use clap::Args;
use mq_cluster::{Cluster, ClusterConfig, PgMeta};
use mq_control::{ControlConfig, Endpoints, QueuesControl};
use mq_core::log::{FileStore, LogStore};
use mq_core::meta::{MetaStore, SqliteMeta};
use mq_core::transport::{Transport, TransportConfig};
use mq_core::{Broker, BrokerConfig};
use mq_shovel::Shovels;
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Args, Clone, Debug)]
pub struct QueuesArgs {
    /// Disable the queues broker.
    #[arg(long = "no-queues", env = "ROLL_QUEUES_DISABLED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub disabled: bool,
    /// AMQP 0-9-1 (and AMQP 1.0) listener.
    #[arg(long = "queues-amqp", env = "ROLL_QUEUES_AMQP", default_value = "127.0.0.1:55672")]
    pub amqp: SocketAddr,
    /// RabbitMQ-compatible management API and Prometheus metrics.
    #[arg(long = "queues-management", env = "ROLL_QUEUES_MANAGEMENT", default_value = "127.0.0.1:55673")]
    pub management: SocketAddr,
    /// STOMP listener.
    #[arg(long = "queues-stomp", env = "ROLL_QUEUES_STOMP", default_value = "127.0.0.1:56613")]
    pub stomp: SocketAddr,
    /// STOMP over WebSocket (`/ws`).
    #[arg(long = "queues-web-stomp", env = "ROLL_QUEUES_WEB_STOMP", default_value = "127.0.0.1:55674")]
    pub web_stomp: SocketAddr,
    /// MQTT listener.
    #[arg(long = "queues-mqtt", env = "ROLL_QUEUES_MQTT", default_value = "127.0.0.1:51883")]
    pub mqtt: SocketAddr,
    /// MQTT over WebSocket (`/ws`).
    #[arg(long = "queues-web-mqtt", env = "ROLL_QUEUES_WEB_MQTT", default_value = "127.0.0.1:55675")]
    pub web_mqtt: SocketAddr,
    /// RabbitMQ Stream protocol listener.
    #[arg(long = "queues-stream", env = "ROLL_QUEUES_STREAM", default_value = "127.0.0.1:55552")]
    pub stream: SocketAddr,
    /// HTTP messaging API (publish, receive, SSE, WebSocket, webhooks).
    #[arg(long = "queues-http", env = "ROLL_QUEUES_HTTP", default_value = "127.0.0.1:55680")]
    pub http: SocketAddr,
    /// Default user created on first start.
    #[arg(long = "queues-user", env = "ROLL_QUEUES_USER", default_value = "guest")]
    pub user: String,
    #[arg(long = "queues-password", env = "ROLL_QUEUES_PASSWORD", default_value = "guest")]
    pub password: String,
    /// Default queue type for new vhosts (classic, quorum or stream).
    #[arg(long = "queues-default-type", env = "ROLL_QUEUES_DEFAULT_QUEUE_TYPE", default_value = "classic")]
    pub default_queue_type: String,
    /// Unload vhosts idle this long (scale to zero), in seconds; 0 keeps everything loaded. An
    /// unloaded vhost keeps its data and wakes on the next connection.
    #[arg(long = "queues-idle-suspend", env = "ROLL_QUEUES_IDLE_SUSPEND", default_value_t = 300)]
    pub idle_suspend_s: u64,
    /// Publish budget of every vhost without its own `max-publish-rate` limit, in messages per
    /// second (0 = unlimited): on a shared broker, one tenant can't crowd out the rest.
    #[arg(long = "queues-vhost-publish-rate", env = "ROLL_QUEUES_VHOST_PUBLISH_RATE", default_value_t = 0)]
    pub vhost_publish_rate: u64,
    /// The same in message bytes per second (`max-publish-bytes-rate`).
    #[arg(long = "queues-vhost-publish-bytes-rate", env = "ROLL_QUEUES_VHOST_PUBLISH_BYTES_RATE", default_value_t = 0)]
    pub vhost_publish_bytes_rate: u64,
    /// Host clients reach this node's listeners on, as stream clients are told (`Metadata`)
    /// (default: `--public-host`).
    #[arg(long = "queues-advertise-host", env = "ROLL_QUEUES_ADVERTISE_HOST")]
    pub advertise_host: Option<String>,
    /// Where queue logs live:
    /// - `file:<dir>` (default `<home>/queues/logs` on a node alone), group-committed through a
    ///   shared WAL unless clustered;
    /// - `s3:<bucket>[/prefix]`: objects in S3 Standard (any bucket with conditional writes),
    ///   group-committed; what a cluster uses by default, in the node's bucket (`--s3-bucket`)
    ///   under `queues/logs`;
    /// - `s3express:<directory-bucket>[/prefix]`: the same in S3 Express One Zone (one zone);
    /// - `quorum:<ip:port>@<az>,<ip:port>@<az>,<ip:port>@<az>`: self-hosted `roll-keeper`s, 2 of 3.
    #[arg(long = "queues-storage", env = "ROLL_QUEUES_STORAGE")]
    pub storage: Option<String>,
    /// Tier sealed log segments to S3 (`s3://bucket/prefix`), with `quorum:` storage.
    #[arg(long = "queues-tier", env = "ROLL_QUEUES_TIER")]
    pub tier: Option<String>,
    /// More CA certificates (PEM) to verify the system database with, besides the system roots
    /// (e.g. the RDS bundle; the image ships it). TLS itself follows the URL's `sslmode`.
    #[arg(long = "queues-database-ca", env = "ROLL_QUEUES_DATABASE_CA")]
    pub database_ca: Option<PathBuf>,
    /// Shared secret the keepers expect (`quorum:` storage; the keepers' `ROLL_KEEPER_SECRET`).
    #[arg(long = "queues-keeper-secret", env = "ROLL_QUEUES_KEEPER_SECRET", hide_env_values = true)]
    pub keeper_secret: Option<String>,
    /// Talk to keepers over TLS, verified against `--queues-keeper-tls-ca` (or the system roots).
    #[arg(long = "queues-keeper-tls", env = "ROLL_QUEUES_KEEPER_TLS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub keeper_tls: bool,
    #[arg(long = "queues-keeper-tls-ca", env = "ROLL_QUEUES_KEEPER_TLS_CA")]
    pub keeper_tls_ca: Option<PathBuf>,
    /// Name the keepers' certificates are issued for (default: each keeper's IP address).
    #[arg(long = "queues-keeper-tls-name", env = "ROLL_QUEUES_KEEPER_TLS_NAME")]
    pub keeper_tls_name: Option<String>,
    /// Compress replication traffic to and from keepers (zstd, large frames only). Cross-AZ
    /// bytes are billed; turn it off only for keepers in the broker's own AZ.
    #[arg(long = "queues-keeper-compress", env = "ROLL_QUEUES_KEEPER_COMPRESS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = true, num_args = 0..=1, default_missing_value = "true")]
    pub keeper_compress: bool,
    /// With `s3:` and `s3express:` storage, write the appends of every queue on this node into
    /// one shared stream (one PUT per batch window instead of one per queue append) and move them
    /// to each queue's own objects later in large batches.
    #[arg(long = "queues-express-group", env = "ROLL_QUEUES_EXPRESS_GROUP", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = true, num_args = 0..=1, default_missing_value = "true")]
    pub express_group: bool,
}

pub struct Queues {
    pub broker: Arc<Broker>,
    pub control: Arc<QueuesControl>,
    pub cluster: Option<Arc<Cluster>>,
    shovels: Arc<Shovels>,
    logs: Arc<dyn LogStore>,
    args: QueuesArgs,
    /// The stream protocol's server: its listener and the edge's connections share it.
    pub stream: mq_stream::Handle,
    home: std::path::PathBuf,
    clients_closed: AtomicBool,
}

impl Queues {
    /// Start the broker on `nodes` (the node's roll-cluster handle). `system_database`: the
    /// shared database's URL, where a cluster keeps metadata; `objects`: the node's object
    /// storage, where a cluster keeps logs by default.
    pub async fn start(home: &Path, args: QueuesArgs, public_host: &str, nodes: Arc<roll_cluster::Cluster>, system_database: Option<&str>, objects: &StorageConfig) -> anyhow::Result<Self> {
        let dir = home.join("queues");
        let clustered = nodes.backend_name() != "memory";
        if args.user == "guest" && args.password == "guest" && !args.amqp.ip().is_loopback() {
            tracing::warn!("queues listen on {} with the default guest/guest login; set ROLL_QUEUES_USER and ROLL_QUEUES_PASSWORD", args.amqp.ip());
        }
        let logs = open_storage(&args, &dir, objects, clustered).await?;
        let meta: Arc<dyn MetaStore> = match system_database {
            Some(url) => PgMeta::connect_with(url, args.database_ca.as_deref()).await?,
            None if clustered => anyhow::bail!("roll queues on a cluster keeps its metadata in the system database: set ROLL_SYSTEM_DATABASE"),
            None => SqliteMeta::open(&dir.join("meta.db"))?,
        };
        let node_id = if clustered { nodes.node_id().to_string() } else { hostname() };
        let config = BrokerConfig {
            node_name: format!("rabbit@{node_id}"),
            default_user: args.user.clone(),
            default_pass: args.password.clone(),
            default_queue_type: args.default_queue_type.clone(),
            az: nodes.config().zone.clone(),
            vhost_idle_suspend_ms: args.idle_suspend_s * 1000,
            vhost_publish_rate: args.vhost_publish_rate,
            vhost_publish_bytes_rate: args.vhost_publish_bytes_rate,
            ..BrokerConfig::default()
        };
        let advertise_host = args.advertise_host.clone().unwrap_or_else(|| public_host.to_string());
        let cluster = match clustered {
            true => {
                let mut listeners = std::collections::BTreeMap::new();
                for (proto, addr) in [("amqp", args.amqp), ("stream", args.stream), ("management", args.management), ("stomp", args.stomp), ("web-stomp", args.web_stomp), ("mqtt", args.mqtt), ("web-mqtt", args.web_mqtt), ("http", args.http)] {
                    let a = tokio::net::lookup_host((advertise_host.as_str(), addr.port())).await?.next();
                    listeners.insert(proto.to_string(), a.ok_or_else(|| anyhow::anyhow!("--queues-advertise-host {advertise_host} doesn't resolve"))?);
                }
                Some(Cluster::join(nodes.clone(), ClusterConfig { listeners, ..ClusterConfig::default() }, meta.clone(), logs.clone()).await?)
            }
            false => None,
        };
        let broker = Broker::start_with_placement(config, meta.clone(), logs.clone(), cluster.as_ref().map(|c| c.placement())).await?;
        if let Some(c) = &cluster {
            c.attach(&broker);
        }
        info!(storage = %logs.describe(), metadata = %meta.describe(), clustered, "queues broker started");

        let url = |scheme: &str, addr: SocketAddr| Some(format!("{scheme}://{public_host}:{}", addr.port()));
        let endpoints = Endpoints {
            amqp: url("amqp", args.amqp),
            mqtt: url("mqtt", args.mqtt),
            stream: url("rabbitmq-stream", args.stream),
            stomp: url("stomp", args.stomp),
            http: url("http", args.http),
            management: url("http", args.management),
            ..Default::default()
        };
        let control = QueuesControl::with_config(
            broker.clone(),
            endpoints,
            ControlConfig {
                usage_db: Some(dir.join("usage.db")),
                idle_suspend_ms: args.idle_suspend_s * 1000,
                storage_backend: logs.describe(),
                metadata_backend: meta.describe(),
            },
        )?;
        control.spawn_background();

        let shovels = Shovels::start(broker.clone());
        let (b, b2) = (broker.clone(), broker.clone());
        let s = shovels.clone();
        control.set_migrator(mq_control::Migrator {
            start: Arc::new(move |body| {
                let b = b.clone();
                Box::pin(async move { mq_shovel::migrate_body(&b, body).await })
            }),
            status: Arc::new(move || s.migration_status_body()),
            stop: Arc::new(move || {
                let b = b2.clone();
                Box::pin(async move { mq_shovel::stop_migration_body(&b).await })
            }),
        });
        let stream = mq_stream::start(broker.clone(), mq_stream::Config { advertised_host: advertise_host, advertised_port: args.stream.port(), ..mq_stream::Config::new(args.stream) }).await;
        Ok(Queues { broker, control, cluster, shovels, logs, args, stream, home: dir, clients_closed: AtomicBool::new(false) })
    }

    /// The control plane (`/v1/queues`), and on a cluster the route other nodes open their RPC
    /// connection on (signed).
    pub fn router(&self) -> axum::Router {
        let r = mq_control::api::router(self.control.clone());
        match &self.cluster {
            Some(c) => r.merge(c.routes()),
            None => r,
        }
    }

    /// Run every listener until one fails.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let a = &self.args;
        let b = &self.broker;
        let _alarms = mq_mgmt::watch_alarms(b.clone(), self.home.clone());
        let mgmt = mq_mgmt::Mgmt::new(
            b.clone(),
            mq_mgmt::Config { listen: a.management, data_dir: self.home.clone(), ..mq_mgmt::Config::default() },
        );
        let s = self.shovels.clone();
        mgmt.set_shovel_status(Arc::new(move || s.status()));
        let s = self.shovels.clone();
        mgmt.set_shovel_restart(Arc::new(move |vhost, name| s.restart(vhost, name)));
        let s = self.shovels.clone();
        mgmt.set_federation_status(Arc::new(move || s.federation_status()));
        let mgmt_listener = bind(a.management).await?;

        let stomp = mq_stomp::Config { listen: a.stomp, ws_listen: Some(a.web_stomp), ..mq_stomp::Config::default() };
        let stream_listener = bind(a.stream).await?;
        let mqtt = mq_mqtt::Config { listen: a.mqtt, ws_listen: Some(a.web_mqtt), ..mq_mqtt::Config::default() };
        let http = mq_http::Config { listen: a.http, public_url: Some(format!("http://{}", a.http)), ..mq_http::Config::default() };

        tokio::select! {
            r = mq_amqp::serve(b.clone(), mq_amqp::ServerConfig { listen: a.amqp, amqp10: Some(mq_amqp10::handoff(mq_amqp10::Config { listen: a.amqp, ..Default::default() })) }) => r,
            r = mq_mgmt::serve_mgmt(mgmt, mgmt_listener) => r,
            r = mq_stomp::serve(b.clone(), stomp.clone()) => r,
            r = mq_stomp::serve_ws(b.clone(), stomp) => r,
            r = mq_mqtt::serve(b.clone(), mqtt) => r,
            r = self.stream.serve_listener(stream_listener) => r,
            r = mq_http::serve(b.clone(), http) => r,
        }
    }

    /// Ask every client connection to close, spread over `window` so they don't all reconnect
    /// (to the other nodes) at once. On a cluster, the node's drain has handed every queue it
    /// owned to other nodes by then, and the connections kept working through it.
    pub async fn close_clients(&self, window: Duration) {
        self.clients_closed.store(true, Ordering::SeqCst);
        self.broker.drain(window).await;
    }

    /// Stop: close the clients still connected (over 2 s), flush and stop every queue still
    /// here, and the log store.
    pub async fn shutdown(&self) {
        if !self.clients_closed.load(Ordering::SeqCst) {
            self.close_clients(Duration::from_secs(2)).await;
        }
        self.shovels.stop();
        self.broker.stop().await;
        if let Some(c) = &self.cluster {
            c.leave().await;
        }
        self.logs.close().await;
        self.control.flush();
    }
}

async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))
}

/// Stable node name: `HOSTNAME` when exported (containers), else the system hostname.
fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length and gethostname NUL-terminates within it.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    match std::str::from_utf8(&buf[..end]) {
        Ok(h) if ok && !h.is_empty() => h.split('.').next().unwrap_or(h).to_string(),
        _ => "localhost".into(),
    }
}

/// The storage spec a node uses: `--queues-storage`, else local files alone, else (a cluster)
/// the node's S3 bucket under `queues/logs`.
fn storage_spec(args: &QueuesArgs, objects: &StorageConfig, clustered: bool) -> anyhow::Result<Option<String>> {
    if let Some(s) = &args.storage {
        return Ok(Some(s.clone()));
    }
    if !clustered {
        return Ok(None);
    }
    match objects {
        StorageConfig::S3 { bucket, prefix, .. } => {
            let prefix = prefix.as_deref().map(|p| p.trim_matches('/')).filter(|p| !p.is_empty());
            Ok(Some(match prefix {
                Some(p) => format!("s3:{bucket}/{p}/queues/logs"),
                None => format!("s3:{bucket}/queues/logs"),
            }))
        }
        StorageConfig::Local(_) => anyhow::bail!("roll queues on a cluster keeps its logs in S3 (--s3-bucket) or where --queues-storage says: this node's disk isn't shared"),
    }
}

/// The log store `--queues-storage` names (see [`storage_spec`]).
async fn open_storage(args: &QueuesArgs, dir: &Path, objects: &StorageConfig, clustered: bool) -> anyhow::Result<Arc<dyn LogStore>> {
    // A node alone owns its logs, so they can group-commit through one WAL. In a cluster on a
    // shared `file:` directory another node may take a log over, and it can't replay our WAL.
    let file = |root: PathBuf| -> anyhow::Result<Arc<dyn LogStore>> { Ok(if clustered { FileStore::new(root) } else { FileStore::with_wal(root, 16 << 20)? }) };
    let Some(spec) = storage_spec(args, objects, clustered)? else { return file(dir.join("logs")) };
    let (kind, rest) = spec.split_once(':').ok_or_else(|| anyhow::anyhow!("--queues-storage {spec}: expected <backend>:<target>"))?;
    let region = || std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION"));
    let group = || args.express_group.then(mq_storage::GroupConfig::default);
    Ok(match kind {
        "file" => file(rest.into())?,
        "s3" => {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
            // The node's own S3 settings (an S3-compatible endpoint in tests), else the environment's.
            match objects {
                StorageConfig::S3 { region: r, endpoint, .. } => {
                    if let Some(r) = r.clone().or_else(|| region().ok()) {
                        b = b.with_region(r);
                    }
                    if let Some(e) = endpoint {
                        b = b.with_endpoint(e).with_allow_http(e.starts_with("http://"));
                    }
                }
                StorageConfig::Local(_) => {
                    if let Ok(r) = region() {
                        b = b.with_region(r);
                    }
                }
            }
            mq_storage::S3ExpressStore::new(mq_storage::ExpressConfig { group: group(), ..mq_storage::ExpressConfig::new(Arc::new(b.build()?), prefix) })
        }
        "quorum" => {
            let mut keepers = Vec::new();
            for k in rest.split(',').filter(|k| !k.is_empty()) {
                let (addr, az) = k.split_once('@').unwrap_or((k, ""));
                let addr = tokio::net::lookup_host(addr).await?.next().ok_or_else(|| anyhow::anyhow!("keeper {addr} doesn't resolve"))?;
                keepers.push((addr, az.to_string()));
            }
            let tier = match &args.tier {
                Some(t) => {
                    let (bucket, prefix) = t.strip_prefix("s3://").ok_or_else(|| anyhow::anyhow!("--queues-tier {t}: expected s3://bucket/prefix"))?.split_once('/').unwrap_or((t.trim_start_matches("s3://"), ""));
                    let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
                    if let Ok(r) = region() {
                        b = b.with_region(r);
                    }
                    // Keep a little sealed data on the keepers for fast replays; the rest goes to S3.
                    Some(mq_storage::TierConfig { keep_hot_bytes: 256 << 20, ..mq_storage::TierConfig::new(Arc::new(b.build()?), prefix) })
                }
                None => None,
            };
            let write_quorum = keepers.len() / 2 + 1;
            let tc = TransportConfig { secret: args.keeper_secret.clone(), tls: args.keeper_tls, tls_ca: args.keeper_tls_ca.clone(), tls_server_name: args.keeper_tls_name.clone(), ..Default::default() };
            let transport = Transport::new("keeper", &tc)?;
            let az = std::env::var("ROLL_ZONE").unwrap_or_default();
            let cfg = mq_storage::QuorumConfig { keepers, write_quorum, az, tier, transport, compress: args.keeper_compress, ..Default::default() };
            mq_storage::QuorumStore::connect(cfg).await?
        }
        "s3express" => {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let region = region().map_err(|_| anyhow::anyhow!("s3express storage needs AWS_REGION"))?;
            let store = mq_storage::S3ExpressStore::directory_bucket(bucket, &region)?;
            mq_storage::S3ExpressStore::new(mq_storage::ExpressConfig { group: group(), ..mq_storage::ExpressConfig::new(store, prefix) })
        }
        _ => anyhow::bail!("--queues-storage: unknown backend {kind} (file, s3, s3express or quorum)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        q: QueuesArgs,
    }

    #[test]
    fn a_cluster_keeps_its_logs_in_the_nodes_bucket() {
        let args = T::parse_from(["t"]).q;
        let s3 = |prefix: Option<&str>| StorageConfig::S3 { bucket: "data".into(), region: None, endpoint: None, prefix: prefix.map(String::from) };
        assert_eq!(storage_spec(&args, &s3(None), false).unwrap(), None, "a node alone keeps local files");
        assert_eq!(storage_spec(&args, &s3(None), true).unwrap().as_deref(), Some("s3:data/queues/logs"));
        assert_eq!(storage_spec(&args, &s3(Some("eu/")), true).unwrap().as_deref(), Some("s3:data/eu/queues/logs"));
        assert!(storage_spec(&args, &StorageConfig::Local("/x".into()), true).is_err(), "a node's disk isn't shared");
        let args = T::parse_from(["t", "--queues-storage", "file:/shared"]).q;
        assert_eq!(storage_spec(&args, &StorageConfig::Local("/x".into()), true).unwrap().as_deref(), Some("file:/shared"));
    }
}
