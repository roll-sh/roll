//! Redis in rolld: the control and data planes, their listeners, and the node's place in a cluster
//! (`redis_control::cluster`).
//!
//! On a node of its own (the memory backend) nothing changes: the catalog is `redis.db` under
//! `--home` and every database is served here. On a cluster backend the catalog is the shared
//! system database (`ROLL_SYSTEM_DATABASE`, schema `redis`; roll code's database if that's unset),
//! each database is served by the holder of its lease, and every listener (rolld's own and the
//! edge's) hands connections and requests for a database to its owner.

use clap::Args;
use pg_storage::StorageConfig;
use redis_control::cluster::Front;
use std::net::SocketAddr;
use std::sync::Arc;

#[derive(Args, Clone, Debug)]
pub struct RedisArgs {
    /// The system database (Postgres URL): the Redis catalog and credentials in a cluster, schema
    /// `redis`. Default: roll code's database (`--code-database`).
    #[arg(id = "system_database", long = "system-database", env = "ROLL_SYSTEM_DATABASE", hide_env_values = true)]
    pub system_database: Option<String>,
}

pub struct Redis {
    pub control: Arc<redis_control::Control>,
    pub front: Arc<Front>,
    /// The listener options of rolld's own RESP port.
    opts: redis_server::net::ListenOptions,
}

/// The shared catalog of a cluster (`None` on a node of its own: SQLite under `--home`).
fn catalog(args: &crate::Args, clustered: bool) -> anyhow::Result<Option<redis_control::store::Store>> {
    if !clustered {
        return Ok(None);
    }
    let url = args.redis_args.system_database.clone().or_else(|| args.code.code_database.clone()).filter(|u| !u.is_empty());
    let Some(url) = url else {
        anyhow::bail!("Redis in a cluster needs the system database (ROLL_SYSTEM_DATABASE): every node must see the same catalog");
    };
    let opts = code_sql::PgOptions { max_connections: 8, ca: args.code.code_db_ca.clone(), ..Default::default() };
    Ok(Some(redis_control::store::Store::open(code_sql::Db::connect(&url, "redis", opts)?)?))
}

impl Redis {
    /// Start the control plane. `clustered`: the node is part of a cluster (not the memory
    /// backend); it serves databases only once [`Redis::join`] is called.
    pub async fn start(args: &crate::Args, storage: &StorageConfig, clustered: bool) -> anyhow::Result<Redis> {
        let home = std::path::absolute(&args.home)?.join("redis");
        let mut rcfg = redis_control::RedisConfig::local(home.clone());
        rcfg.node_name = args.node.clone();
        rcfg.public_host = args.public_host.clone();
        rcfg.resp_port = args.redis.port();
        rcfg.http_port = args.redis_http.port();
        if args.edge.enabled {
            rcfg.domain = Some(args.edge.hostnames().product_domain("redis"));
            rcfg.region = args.edge.region.clone();
        }
        let tls = match (&args.redis_tls_cert, &args.redis_tls_key) {
            (Some(c), Some(k)) => Some(redis_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?),
            _ => None,
        };
        rcfg.tls = tls.is_some();
        if clustered && matches!(storage, StorageConfig::Local(_)) {
            anyhow::bail!("Redis in a cluster needs S3 (--s3-bucket): the other nodes can't read a local directory");
        }
        let objects = redis_server::persist::Objects::new(storage)?;
        let control = match catalog(args, clustered)? {
            Some(store) => redis_control::Control::with_store(rcfg, objects, store).await?,
            None => redis_control::Control::new(rcfg, objects).await?,
        };
        // Spliced connections show the edge's address (or rolld's own port).
        let public = if args.edge.enabled { args.edge.redis_listen } else { args.redis };
        let secret = cluster_secret().unwrap_or_else(|| roll_cluster::ClusterConfig::random_node_id() + "-redis-local-only");
        let front = Front::new(control.clone(), &secret, public)?;
        Ok(Redis { control, front, opts: redis_server::net::ListenOptions { tls, proxy_protocol: args.redis_proxy_protocol } })
    }

    /// Serve databases while this node holds their leases (a cluster node; nothing on the
    /// memory backend, which owns everything).
    pub fn join(&self, cluster: &Arc<roll_cluster::Cluster>) {
        if cluster.backend_name() != "memory" {
            tracing::info!(node = %cluster.node_id(), "redis: databases are served by the holders of their leases");
            self.control.join(cluster.clone());
        }
    }

    /// rolld's own RESP listener.
    pub async fn serve_resp(&self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        match self.control.cluster() {
            Some(_) => self.front.clone().serve_listener(listener, self.opts.clone()).await,
            None => redis_server::net::serve(listener, self.control.node.clone(), self.opts.clone()).await,
        }
    }

    /// rolld's own REST listener (Upstash's protocol).
    pub async fn serve_rest(&self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        axum::serve(listener, self.rest_router().into_make_service_with_connect_info::<SocketAddr>()).await?;
        Ok(())
    }

    /// The REST data plane; in a cluster, a request runs on its database's owner.
    pub fn rest_router(&self) -> axum::Router {
        redis_control::cluster::rest_router(self.control.clone())
    }

    /// The internal route other nodes splice connections to (`/internal/edge/splice`), when the
    /// edge (which serves it otherwise) is off.
    pub fn internal(&self, router: axum::Router, edge_on: bool) -> anyhow::Result<axum::Router> {
        if self.control.cluster().is_none() {
            return Ok(router);
        }
        // REST requests other nodes forward to this one (their database is here).
        let router = router.merge(redis_control::cluster::internal_router(self.control.clone()));
        let (Some(secret), false) = (cluster_secret(), edge_on) else { return Ok(router) };
        let handlers = roll_edge::splice::Handlers::default();
        handlers.register(redis_control::cluster::PRODUCT, self.front.handler());
        Ok(router.merge(roll_edge::splice::router(roll_edge::splice::Signer::new(&secret)?, handlers)))
    }

    /// Make every acknowledged write durable before the process exits (the databases a drain
    /// handed over are flushed already).
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.control.shipper.flush().await?;
        self.control.shipper.write_manifests(true).await
    }
}

fn cluster_secret() -> Option<String> {
    ["ROLL_CLUSTER_SECRET", "ROLL_CODE_CLUSTER_SECRET"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}
