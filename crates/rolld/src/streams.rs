//! roll streams inside rolld (the broker README, "On the fleet"; docs/cloud/README.md).
//!
//! Alone (the memory cluster backend), the broker keeps its catalog in SQLite under `--home`, as
//! it always has. In a cluster every node runs it on the system database (`ROLL_SYSTEM_DATABASE`,
//! schema `streams`) and S3, with one owner per stream (`stream:cluster:<id>`): Kafka connections
//! a node doesn't own are spliced to the owner over the edge's internal hop
//! (`/internal/edge/splice`, product `kafka`), and HTTP requests are forwarded through
//! roll-cluster. Acknowledgements wait for the owner's write-ahead log object in S3.

use crate::Args;
use bytes::Bytes;
use futures::future::BoxFuture;
use pg_storage::StorageConfig;
use roll_cluster::{Cluster, Node};
use std::sync::Arc;
use std::time::Duration;
use stream_broker::config::TlsFiles;
use stream_broker::fleet::{Fleet, Hop, Io, Splicer};
use stream_broker::store::Settings;
use tracing::info;

#[derive(clap::Args, Clone, Debug)]
pub struct StreamArgs {
    /// The fleet: how long a stream's owner gathers appends before writing them to its
    /// write-ahead log in S3 (`acks=all` latency against S3 requests: at most one PUT per busy
    /// stream per round).
    #[arg(id = "roll_stream_ack_linger_ms", long = "stream-ack-linger-ms", env = "ROLL_STREAM_ACK_LINGER_MS", default_value_t = 50)]
    pub ack_linger_ms: u64,
}

/// roll streams: the broker, and its Kafka listener until it's served.
pub struct Streams {
    pub broker: Arc<stream_broker::Broker>,
    pub kafka: Option<tokio::net::TcpListener>,
}

pub async fn start(args: &Args, auth: &crate::auth::Auth, home: &std::path::Path, storage: &StorageConfig, cluster: &Arc<Cluster>) -> anyhow::Result<Streams> {
    let mut c = stream_broker::Config::local(home.to_path_buf());
    c.listen = args.kafka;
    c.advertised_host = args.kafka_advertised_host.clone().unwrap_or_else(|| args.public_host.clone());
    c.advertised_port = args.kafka_advertised_port.unwrap_or(args.kafka.port());
    c.default_stream = Some(args.default_stream.clone()).filter(|s| !s.is_empty());
    c.tls = args.kafka_tls_cert.clone().zip(args.kafka_tls_key.clone()).map(|(cert, key)| TlsFiles { cert, key });
    c.http_url = format!("http://{}:{}", args.public_host, args.api.port());
    c.http_require_auth = args.stream_require_auth || auth.enforcing();
    c.local_disk_bytes = args.stream_disk_mb << 20;
    c.evict_on_unload = !args.stream_keep_local;
    c.storage = storage.clone();
    let broker = match cluster.backend_name() {
        "memory" => stream_broker::Broker::new(c).await?,
        _ => {
            let url = args.code.cluster.system_database.as_deref().filter(|u| !u.is_empty());
            let url = url.ok_or_else(|| anyhow::anyhow!("roll streams in a cluster need ROLL_SYSTEM_DATABASE: the stream catalog is shared"))?;
            anyhow::ensure!(matches!(storage, StorageConfig::S3 { .. }), "roll streams in a cluster need S3 (--s3-bucket): a local directory isn't shared");
            let secret = args.code.cluster.resolve(args.code.code_cluster_secret.as_deref(), None, None)?.secret;
            let secret = secret.ok_or_else(|| anyhow::anyhow!("a cluster needs ROLL_CLUSTER_SECRET"))?;
            let db = tokio::task::block_in_place(|| code_sql::Db::connect(url, "streams", code_sql::PgOptions { max_connections: 8, ..Default::default() }))?;
            let mut fleet = Fleet::new(cluster.clone(), Arc::new(EdgeSplicer { signer: roll_edge::splice::Signer::new(&secret)? }));
            fleet.linger = Duration::from_millis(args.streams.ack_linger_ms);
            let broker = stream_broker::Broker::on_fleet(c, db, fleet).await?;
            info!(node = %cluster.node_id(), "roll streams: fleet node (catalog on the system database, acknowledgements from S3)");
            broker
        }
    };
    if let Some(name) = &broker.config.default_stream
        && broker.find_stream(name).is_none()
        && let Err(e) = broker.create_stream(name, Settings::default())
        // Another node created it at the same moment.
        && e.code != kafka_protocol::ResponseError::DuplicateResource
    {
        return Err(e.into());
    }
    broker.txns.recover(&broker).await?;
    let kafka = tokio::net::TcpListener::bind(args.kafka).await?;
    Ok(Streams { broker, kafka: Some(kafka) })
}

/// Connections for a stream another node owns go there over the edge's signed internal hop. The
/// owner gets the stream the connection authenticated as (the hop's tenant) and the broker
/// address the client was told (`host:port`, in the hop's server name).
struct EdgeSplicer {
    signer: roll_edge::splice::Signer,
}

impl Splicer for EdgeSplicer {
    fn splice(&self, to: &Node, io: Box<dyn Io>, hop: Hop) -> BoxFuture<'static, std::io::Result<()>> {
        let (signer, url) = (self.signer.clone(), to.url.clone());
        let edge_hop = roll_edge::splice::Hop { product: "kafka".into(), tenant: hop.stream, client: hop.peer, sni: hop.advertised.map(|(h, p)| format!("{h}:{p}")), hops: 0 };
        Box::pin(async move { roll_edge::splice::splice(io, &url, &signer, &edge_hop, &hop.first).await.map(drop) })
    }
}

/// The owner's side of a spliced Kafka connection (registered with the edge as `kafka`).
pub fn splice_handler(broker: Arc<stream_broker::Broker>) -> roll_edge::splice::Handler {
    Arc::new(move |io, hop| {
        let b = broker.clone();
        let advertised = hop.sni.as_deref().and_then(|s| s.rsplit_once(':')).and_then(|(h, p)| Some((h.to_string(), p.parse().ok()?)));
        Box::pin(async move { stream_broker::server::serve_spliced(b, io, Bytes::new(), hop.client, &hop.tenant, advertised).await })
    })
}
