//! roll queues inside rolld: the broker, its protocol listeners and its control-plane routes.
//!
//! Alone, a node keeps metadata in SQLite and queue logs on local disk. With
//! `--queues-cluster-meta postgres://…` it joins a cluster: metadata (and queue ownership) live in
//! Postgres, every node accepts every client, and queues move between nodes without clients
//! reconnecting.

use clap::Args;
use mq_cluster::{Cluster, ClusterConfig, PgMeta};
use mq_control::{ControlConfig, Endpoints, QueuesControl};
use mq_core::log::{FileStore, LogStore};
use mq_core::meta::{MetaStore, SqliteMeta};
use mq_core::transport::{Transport, TransportConfig};
use mq_core::{Broker, BrokerConfig};
use mq_shovel::Shovels;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// Availability zone of this node (AZ-aware placement).
    #[arg(long = "queues-az", env = "ROLL_QUEUES_AZ")]
    pub az: Option<String>,
    /// Join a cluster whose metadata lives in this Postgres database.
    #[arg(long = "queues-cluster-meta", env = "ROLL_QUEUES_CLUSTER_META")]
    pub cluster_meta: Option<String>,
    /// Stable node id (defaults to the hostname).
    #[arg(long = "queues-node-id", env = "ROLL_QUEUES_NODE_ID")]
    pub node_id: Option<String>,
    /// Internal broker-to-broker RPC listener (cluster mode).
    #[arg(long = "queues-rpc", env = "ROLL_QUEUES_RPC", default_value = "0.0.0.0:55679")]
    pub rpc: SocketAddr,
    /// Address other nodes dial for RPC (defaults to `--queues-rpc`).
    #[arg(long = "queues-rpc-advertise", env = "ROLL_QUEUES_RPC_ADVERTISE")]
    pub rpc_advertise: Option<SocketAddr>,
    /// Host other nodes and `mq-proxy` reach this node's listeners on (defaults to the RPC
    /// advertise address, then `--public-host`).
    #[arg(long = "queues-advertise-host", env = "ROLL_QUEUES_ADVERTISE_HOST")]
    pub advertise_host: Option<String>,
    /// Where queue logs live:
    /// - `file:<dir>` (default `<home>/queues/logs`), one node (group-commits through a shared
    ///   WAL unless clustered);
    /// - `quorum:<ip:port>@<az>,<ip:port>@<az>,<ip:port>@<az>`: replicated keepers, 2 of 3;
    /// - `s3express:<directory-bucket>[/prefix]`: S3 Express One Zone, no keepers.
    #[arg(long = "queues-storage", env = "ROLL_QUEUES_STORAGE")]
    pub storage: Option<String>,
    /// Tier sealed log segments to S3 (`s3://bucket/prefix`), with `quorum:` storage.
    #[arg(long = "queues-tier", env = "ROLL_QUEUES_TIER")]
    pub tier: Option<String>,
    /// More CA certificates (PEM) to verify the metadata Postgres with, besides the system roots
    /// (e.g. the RDS bundle; the image ships it). TLS itself follows the URL's `sslmode`.
    #[arg(long = "queues-cluster-meta-ca", env = "ROLL_QUEUES_CLUSTER_META_CA")]
    pub cluster_meta_ca: Option<PathBuf>,
    /// Shared secret every node must know to talk to the others (cluster RPC).
    #[arg(long = "queues-cluster-secret", env = "ROLL_QUEUES_CLUSTER_SECRET", hide_env_values = true)]
    pub cluster_secret: Option<String>,
    /// Cluster RPC over TLS: this node's certificate chain (PEM). Needs the key and the CA.
    #[arg(long = "queues-cluster-tls-cert", env = "ROLL_QUEUES_CLUSTER_TLS_CERT", requires_all = ["cluster_tls_key", "cluster_tls_ca"])]
    pub cluster_tls_cert: Option<PathBuf>,
    #[arg(long = "queues-cluster-tls-key", env = "ROLL_QUEUES_CLUSTER_TLS_KEY", requires = "cluster_tls_cert")]
    pub cluster_tls_key: Option<PathBuf>,
    /// CA that signed the nodes' certificates.
    #[arg(long = "queues-cluster-tls-ca", env = "ROLL_QUEUES_CLUSTER_TLS_CA", requires = "cluster_tls_cert")]
    pub cluster_tls_ca: Option<PathBuf>,
    /// Name the nodes' certificates are issued for (default: each node's IP address).
    #[arg(long = "queues-cluster-tls-name", env = "ROLL_QUEUES_CLUSTER_TLS_NAME")]
    pub cluster_tls_name: Option<String>,
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
    /// With `s3express:` storage, write the appends of every queue on this node into one shared
    /// stream (a few PUTs per batch window instead of one per queue append) and move them to each
    /// queue's own objects later in large batches.
    #[arg(long = "queues-express-group", env = "ROLL_QUEUES_EXPRESS_GROUP", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = true, num_args = 0..=1, default_missing_value = "true")]
    pub express_group: bool,
    /// How long a draining node waits for its queues to move, in seconds.
    #[arg(long = "queues-drain-timeout", env = "ROLL_QUEUES_DRAIN_TIMEOUT", default_value_t = 30)]
    pub drain_timeout_s: u64,
}

pub struct Queues {
    pub broker: Arc<Broker>,
    pub control: Arc<QueuesControl>,
    pub cluster: Option<Arc<Cluster>>,
    shovels: Arc<Shovels>,
    logs: Arc<dyn LogStore>,
    args: QueuesArgs,
    /// Host clients are told to connect to (stream metadata).
    advertise: String,
    home: std::path::PathBuf,
}

impl Queues {
    pub async fn start(home: &Path, args: QueuesArgs, public_host: &str) -> anyhow::Result<Self> {
        let dir = home.join("queues");
        let mut args = args;
        if args.az.is_none() {
            args.az = mq_cluster::aws::ecs_availability_zone().await;
        }
        if args.user == "guest" && args.password == "guest" && !args.amqp.ip().is_loopback() {
            tracing::warn!("queues listen on {} with the default guest/guest login; set ROLL_QUEUES_USER and ROLL_QUEUES_PASSWORD", args.amqp.ip());
        }
        let logs = open_storage(&args, &dir).await?;
        let meta: Arc<dyn MetaStore> = match &args.cluster_meta {
            Some(url) => PgMeta::connect_with(url, args.cluster_meta_ca.as_deref()).await?,
            None => SqliteMeta::open(&dir.join("meta.db"))?,
        };
        let node_id = args.node_id.clone().unwrap_or_else(hostname);
        let config = BrokerConfig {
            node_name: format!("rabbit@{node_id}"),
            default_user: args.user.clone(),
            default_pass: args.password.clone(),
            default_queue_type: args.default_queue_type.clone(),
            az: args.az.clone(),
            vhost_idle_suspend_ms: args.idle_suspend_s * 1000,
            vhost_publish_rate: args.vhost_publish_rate,
            vhost_publish_bytes_rate: args.vhost_publish_bytes_rate,
            ..BrokerConfig::default()
        };
        // Containers and VMs: the address our hostname resolves to is what peers can dial.
        let own_ip = match tokio::net::lookup_host((hostname().as_str(), 0)).await {
            Ok(mut addrs) => addrs.find(|a| a.is_ipv4() && !a.ip().is_loopback()).map(|a| a.ip()),
            Err(_) => None,
        };
        let rpc_advertise = args.rpc_advertise.or_else(|| match (args.rpc.ip().is_unspecified(), own_ip) {
            (true, Some(ip)) => Some(SocketAddr::new(ip, args.rpc.port())),
            _ => None,
        });
        let advertise_host = match &args.cluster_meta {
            Some(_) => args.advertise_host.clone().or_else(|| rpc_advertise.map(|a| a.ip().to_string())).unwrap_or_else(|| public_host.to_string()),
            None => args.advertise_host.clone().unwrap_or_else(|| public_host.to_string()),
        };
        let cluster = match &args.cluster_meta {
            Some(_) => {
                let host = &advertise_host;
                let mut listeners = std::collections::BTreeMap::new();
                for (proto, addr) in [("amqp", args.amqp), ("stream", args.stream), ("management", args.management), ("stomp", args.stomp), ("web-stomp", args.web_stomp), ("mqtt", args.mqtt), ("web-mqtt", args.web_mqtt), ("http", args.http)] {
                    let a = tokio::net::lookup_host((host.as_str(), addr.port())).await?.next();
                    let a = a.ok_or_else(|| anyhow::anyhow!("--queues-advertise-host {host} doesn't resolve"))?;
                    listeners.insert(proto.to_string(), a);
                }
                let tc = TransportConfig {
                    secret: args.cluster_secret.clone(),
                    tls_cert: args.cluster_tls_cert.clone(),
                    tls_key: args.cluster_tls_key.clone(),
                    tls: args.cluster_tls_cert.is_some(),
                    tls_ca: args.cluster_tls_ca.clone(),
                    tls_server_name: args.cluster_tls_name.clone(),
                };
                if tc.secret.is_none() {
                    tracing::warn!("no ROLL_QUEUES_CLUSTER_SECRET: any host that can reach {} can join the cluster RPC", args.rpc);
                }
                let transport = Transport::new("cluster", &tc)?;
                let cc = ClusterConfig { node_id, listen: args.rpc, advertise: rpc_advertise, az: args.az.clone(), listeners, transport, ..ClusterConfig::default() };
                Some(Cluster::join(cc, meta.clone(), logs.clone()).await?)
            }
            None => None,
        };
        let broker = Broker::start_with_placement(config, meta.clone(), logs.clone(), cluster.as_ref().map(|c| c.placement())).await?;
        if let Some(c) = &cluster {
            c.attach(&broker);
        }
        info!(storage = %logs.describe(), metadata = %meta.describe(), clustered = cluster.is_some(), "queues broker started");

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
        Ok(Queues { broker, control, cluster, shovels, logs, args, home: dir, advertise: advertise_host })
    }

    pub fn router(&self) -> axum::Router {
        mq_control::api::router(self.control.clone())
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
        let stream = mq_stream::Config { advertised_host: self.advertise.clone(), ..mq_stream::Config::new(a.stream) };
        let mqtt = mq_mqtt::Config { listen: a.mqtt, ws_listen: Some(a.web_mqtt), ..mq_mqtt::Config::default() };
        let http = mq_http::Config { listen: a.http, public_url: Some(format!("http://{}", a.http)), ..mq_http::Config::default() };

        tokio::select! {
            r = mq_amqp::serve(b.clone(), mq_amqp::ServerConfig { listen: a.amqp, amqp10: Some(mq_amqp10::handoff(mq_amqp10::Config { listen: a.amqp, ..Default::default() })) }) => r,
            r = mq_mgmt::serve_mgmt(mgmt, mgmt_listener) => r,
            r = mq_stomp::serve(b.clone(), stomp.clone()) => r,
            r = mq_stomp::serve_ws(b.clone(), stomp) => r,
            r = mq_mqtt::serve(b.clone(), mqtt) => r,
            r = mq_stream::serve(b.clone(), stream) => r,
            r = mq_http::serve(b.clone(), http) => r,
        }
    }

    /// Graceful stop: hand queues to other nodes (cluster), close client connections gradually,
    /// then flush and stop everything.
    pub async fn shutdown(&self) {
        if let Some(c) = &self.cluster {
            let left = c.drain(Duration::from_secs(self.args.drain_timeout_s)).await;
            info!(left, "queues handed off");
        }
        self.broker.drain(Duration::from_secs(2)).await;
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

/// The log store `--queues-storage` names.
async fn open_storage(args: &QueuesArgs, dir: &Path) -> anyhow::Result<Arc<dyn LogStore>> {
    // A node alone owns its logs, so they can group-commit through one WAL. In a cluster on a
    // shared `file:` directory another node may take a log over, and it can't replay our WAL.
    let file = |root: PathBuf| -> anyhow::Result<Arc<dyn LogStore>> {
        Ok(if args.cluster_meta.is_some() { FileStore::new(root) } else { FileStore::with_wal(root, 16 << 20)? })
    };
    let Some(spec) = args.storage.as_deref() else { return file(dir.join("logs")) };
    let (kind, rest) = spec.split_once(':').ok_or_else(|| anyhow::anyhow!("--queues-storage {spec}: expected <backend>:<target>"))?;
    let region = || std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION"));
    Ok(match kind {
        "file" => file(rest.into())?,
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
            let cfg = mq_storage::QuorumConfig { keepers, write_quorum, az: args.az.clone().unwrap_or_default(), tier, transport, compress: args.keeper_compress, ..Default::default() };
            mq_storage::QuorumStore::connect(cfg).await?
        }
        "s3express" => {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let region = region().map_err(|_| anyhow::anyhow!("s3express storage needs AWS_REGION"))?;
            let store = mq_storage::S3ExpressStore::directory_bucket(bucket, &region)?;
            let group = args.express_group.then(mq_storage::GroupConfig::default);
            mq_storage::S3ExpressStore::new(mq_storage::ExpressConfig { group, ..mq_storage::ExpressConfig::new(store, prefix) })
        }
        _ => anyhow::bail!("--queues-storage: unknown backend {kind} (file, quorum or s3express)"),
    })
}
