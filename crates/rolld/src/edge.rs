//! roll cloud's edge in rolld (`ROLL_EDGE=1`; docs/cloud/README.md, "The edge"): the public
//! listeners, with TLS, SNI and Host routing and the NLB's PROXY protocol, in front of the
//! products this process runs. Without `ROLL_EDGE` nothing here runs and rolld is as it was.
//!
//! - HTTPS (8443, the NLB's 443) routes by Host (`roll_edge::http`).
//! - 5432, 6379, 9092, 9042, 5671, 8883, 61614 and 5551 end TLS and hand the stream, the client's
//!   address and the server name to the product's own connection handler.
//! - Certificates come from object storage (`system/certs/`, issued over ACME by whichever node
//!   holds the `acme` singleton), or from `ROLL_EDGE_CERT_DIR` (dev mode: a self-signed one is
//!   written there if it's empty).
//! - `GET /healthz` joins the internal API; draining starts on SIGTERM.

use crate::Args;
use axum::http::{HeaderMap, Uri, header};
use pg_storage::StorageConfig;
use roll_edge::certs::{CertManager, Singleton};
use roll_edge::http::{HttpsConfig, Routes, Target, Upstream};
use roll_edge::listener::{self, ListenOptions};
use roll_edge::{CertStore, Drain, Host, Hostnames};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

#[derive(clap::Args, Clone, Debug)]
pub struct EdgeArgs {
    /// Serve the public listeners: TLS, SNI and Host routing, PROXY protocol (roll cloud).
    #[arg(long = "edge", env = "ROLL_EDGE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub enabled: bool,
    /// The product's domain: `<id>.pg.<domain>`, `api.<domain>`, …
    #[arg(long = "edge-domain", env = "ROLL_EDGE_DOMAIN", default_value = "roll.sh")]
    pub domain: String,
    /// The user-content domain: the S3 API, public buckets, previews.
    #[arg(long = "edge-site-domain", env = "ROLL_EDGE_SITE_DOMAIN", default_value = "roll.site")]
    pub site_domain: String,
    /// This node's region: directly connected names carry it (`<id>.pg.<region>.<domain>`), and
    /// only this region's are routed.
    #[arg(long = "edge-region", env = "ROLL_EDGE_REGION", default_value = "us-east-1")]
    pub region: String,
    /// Every connection starts with a PROXY protocol header (behind the NLB).
    #[arg(long = "edge-proxy-protocol", env = "ROLL_EDGE_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub proxy_protocol: bool,
    #[arg(long = "edge-https", env = "ROLL_EDGE_HTTPS", default_value = "0.0.0.0:8443")]
    pub https_listen: SocketAddr,
    #[arg(long = "edge-pg", env = "ROLL_EDGE_PG", default_value = "0.0.0.0:5432")]
    pub pg_listen: SocketAddr,
    #[arg(long = "edge-redis", env = "ROLL_EDGE_REDIS", default_value = "0.0.0.0:6379")]
    pub redis_listen: SocketAddr,
    #[arg(long = "edge-kafka", env = "ROLL_EDGE_KAFKA", default_value = "0.0.0.0:9092")]
    pub kafka_listen: SocketAddr,
    #[arg(long = "edge-nosql", env = "ROLL_EDGE_NOSQL", default_value = "0.0.0.0:9042")]
    pub nosql_listen: SocketAddr,
    #[arg(long = "edge-amqps", env = "ROLL_EDGE_AMQPS", default_value = "0.0.0.0:5671")]
    pub amqps_listen: SocketAddr,
    #[arg(long = "edge-mqtts", env = "ROLL_EDGE_MQTTS", default_value = "0.0.0.0:8883")]
    pub mqtts_listen: SocketAddr,
    #[arg(long = "edge-stomps", env = "ROLL_EDGE_STOMPS", default_value = "0.0.0.0:61614")]
    pub stomps_listen: SocketAddr,
    #[arg(long = "edge-mq-stream", env = "ROLL_EDGE_MQ_STREAM", default_value = "0.0.0.0:5551")]
    pub mq_stream_listen: SocketAddr,
    /// Dev mode: serve the `<name>.crt`/`<name>.key` pairs in this directory (a self-signed
    /// certificate for every edge hostname is written when it has none) instead of ACME's.
    #[arg(long = "edge-cert-dir", env = "ROLL_EDGE_CERT_DIR")]
    pub cert_dir: Option<PathBuf>,
    /// How often to look for new certificates in object storage (and renew them), in seconds.
    #[arg(long = "edge-cert-poll", env = "ROLL_EDGE_CERT_POLL_S", default_value_t = 600)]
    pub cert_poll_s: u64,
    /// The ACME directory to order certificates from (Let's Encrypt's in the cloud). Unset, this
    /// node only loads certificates others issued.
    #[arg(long = "acme-directory", env = "ROLL_ACME_DIRECTORY")]
    pub acme_directory: Option<String>,
    #[arg(long = "acme-email", env = "ROLL_ACME_EMAIL")]
    pub acme_email: Option<String>,
    /// A PEM root for the ACME server's own HTTPS (a test CA such as Pebble's).
    #[arg(long = "acme-ca-file", env = "ROLL_ACME_CA_FILE")]
    pub acme_ca_file: Option<PathBuf>,
    /// Seconds between publishing the challenge records and asking the CA to check them.
    #[arg(long = "acme-propagation", env = "ROLL_ACME_PROPAGATION_S", default_value_t = 20)]
    pub acme_propagation_s: u64,
    /// DNS edit on both zones, for DNS-01.
    #[arg(long = "cloudflare-api-token", env = "ROLL_CLOUDFLARE_API_TOKEN", hide_env_values = true)]
    pub cloudflare_api_token: Option<String>,
    #[arg(long = "cloudflare-api", env = "ROLL_CLOUDFLARE_API", default_value = roll_edge::acme::Cloudflare::API)]
    pub cloudflare_api: String,
    /// Replaces Cloudflare's published ranges (whose `CF-Connecting-IP` is believed).
    #[arg(long = "edge-cloudflare-ips", env = "ROLL_EDGE_CLOUDFLARE_IPS")]
    pub cloudflare_ips: Option<String>,
    /// Authenticated origin pulls: Cloudflare-proxied hostnames (`roll.sh`, `api.roll.sh`,
    /// `*.roll.site`) are served only to Cloudflare's origin-pull client certificate. Default:
    /// on, except in dev mode (`ROLL_EDGE_CERT_DIR`).
    #[arg(long = "edge-origin-pull", env = "ROLL_EDGE_ORIGIN_PULL", value_parser = clap::builder::BoolishValueParser::new(), num_args = 0..=1, default_missing_value = "true")]
    pub origin_pull: Option<bool>,
    /// A PEM bundle replacing the origin-pull CA built in (Cloudflare's).
    #[arg(long = "edge-origin-pull-ca", env = "ROLL_EDGE_ORIGIN_PULL_CA")]
    pub origin_pull_ca: Option<PathBuf>,
    /// Seconds a stopping node keeps its public listeners serving after `/healthz` starts failing,
    /// so the NLB takes it out first (health check every 5 s, 2 failures, deregistration).
    #[arg(long = "edge-drain-delay", env = "ROLL_EDGE_DRAIN_DELAY", default_value_t = 15)]
    pub drain_delay_s: u64,
    /// The dashboard (`roll.sh`), proxied with WebSockets: `http://127.0.0.1:3000` in the task.
    #[arg(long = "dashboard-upstream", env = "ROLL_DASHBOARD_UPSTREAM")]
    pub dashboard_upstream: Option<String>,
}

impl EdgeArgs {
    pub fn hostnames(&self) -> Hostnames {
        Hostnames::new(&self.domain, &self.site_domain, &self.region)
    }
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Point the products' own hostname settings at the edge's names, where the operator didn't set
/// them: virtual-host S3, Alternator's Host routing, previews, roll code's public URLs. Moves
/// rolld's own Kafka listener off the edge's Kafka port when they collide.
pub fn configure(args: &mut Args) {
    if !args.edge.enabled {
        return;
    }
    let hosts = args.edge.hostnames();
    args.s3_domain.get_or_insert_with(|| hosts.s3_domain());
    args.nosql.nosql_alternator_domain.get_or_insert_with(|| hosts.product_domain("nosql"));
    if !env_set("ROLL_CODE_PREVIEW_DOMAIN") {
        args.code_preview_domain = hosts.site_domain.clone();
    }
    args.code_public_url.get_or_insert_with(|| format!("https://api.{}", hosts.domain));
    args.code_git_url.get_or_insert_with(|| format!("https://{}", hosts.domain));
    if !env_set("ROLL_WEB_URL") {
        args.web_url = format!("https://{}", hosts.domain);
    }
    if args.kafka.port() == args.edge.kafka_listen.port() {
        let moved = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19092);
        info!(from = %args.kafka, to = %moved, "edge: rolld's own Kafka listener moves off the edge's port");
        args.kafka = moved;
    }
}

/// What the edge serves, from the products this process runs (`None`: off, `ROLL_PRODUCTS`; its
/// listener isn't bound and its hostnames answer 404).
pub struct Products<'a> {
    /// The API router (every product's HTTP API, roll code, previews).
    pub api: axum::Router,
    pub pg: Option<Arc<pg_proxy::Proxy<pg_control::control::ProxyRouter>>>,
    pub redis: Option<&'a crate::redis::Redis>,
    pub streams: Option<Arc<stream_broker::Broker>>,
    pub tasks: Option<Arc<tasks::Tasks>>,
    /// The S3 API (requests run on the metadata's owner).
    pub storage: Option<axum::Router>,
    pub nosql: Option<&'a crate::nosql::Nosql>,
    pub queues: Option<&'a crate::queues::Queues>,
}

/// The edge, its listeners bound.
pub struct Edge {
    drain: Drain,
    internal: axum::Router,
    serve: futures::future::BoxFuture<'static, anyhow::Result<()>>,
}

async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("edge: bind {addr}: {e}"))
}

/// A listener's address as a URL this process can reach.
fn local_url(addr: SocketAddr) -> String {
    let ip = match addr.ip() {
        ip if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, addr.port()))
}

type Served = futures::future::BoxFuture<'static, anyhow::Result<()>>;

fn io<F: std::future::Future<Output = std::io::Result<()>> + Send + 'static>(f: F) -> Served {
    Box::pin(async move { f.await.map_err(anyhow::Error::from) })
}


/// The fleet's one-node-at-a-time lock for the edge's jobs (certificate issuance): the cluster
/// layer's singleton, or always held on a node on its own (the memory backend owns everything).
pub struct ClusterSingleton(pub Arc<roll_cluster::Cluster>);

impl Singleton for ClusterSingleton {
    fn holds<'a>(&'a self, name: &'a str) -> futures::future::BoxFuture<'a, bool> {
        let (cluster, name) = (self.0.clone(), name.to_string());
        // `singleton` may wait on the backend: keep it off the async workers.
        Box::pin(async move { tokio::task::spawn_blocking(move || cluster.singleton(&name)).await.unwrap_or(false) })
    }
}

impl Edge {
    /// Bind every public listener and start loading certificates. `None` without `ROLL_EDGE`.
    pub async fn start(args: &Args, storage: &StorageConfig, p: Products<'_>, singleton: Arc<dyn Singleton>) -> anyhow::Result<Option<Edge>> {
        let e = &args.edge;
        if !e.enabled {
            return Ok(None);
        }
        roll_edge::install_crypto_provider();
        let hosts = e.hostnames();
        let drain = Drain::new();
        let store = CertStore::new();
        store.set_default_name(&hosts.domain);
        load_certificates(e, &hosts, storage, &store, singleton)?;
        let opts = ListenOptions { proxy_protocol: e.proxy_protocol, ..Default::default() };
        let plain = roll_edge::tls::server_config(store.clone(), &[]);

        let mut served: Vec<Served> = vec![];
        // HTTPS.
        let origin_pull = match e.origin_pull.unwrap_or(e.cert_dir.is_none()) {
            true => {
                let pem = match &e.origin_pull_ca {
                    Some(f) => std::fs::read_to_string(f).map_err(|err| anyhow::anyhow!("ROLL_EDGE_ORIGIN_PULL_CA {}: {err}", f.display()))?,
                    None => roll_edge::trusted::CLOUDFLARE_ORIGIN_PULL_CA.to_string(),
                };
                let roots = roll_edge::tls::pem_certificates(&pem)?;
                Some(roll_edge::tls::server_config_verifying_clients(store.clone(), roll_edge::http::ALPN, &roots)?)
            }
            false => None,
        };
        let https = HttpsConfig {
            hosts: hosts.clone(),
            cloudflare: match &e.cloudflare_ips {
                Some(list) => roll_edge::trusted::TrustedProxies::parse(list)?,
                None => roll_edge::trusted::TrustedProxies::cloudflare(),
            },
            listen: opts,
            origin_pull,
            observe: Some(Arc::new(crate::logs::EdgeAccess)),
        };
        let routes = HttpRoutes::new(args, &p)?;
        let l = bind(e.https_listen).await?;
        let tls = roll_edge::tls::server_config(store.clone(), roll_edge::http::ALPN);
        served.push(io(roll_edge::http::serve(l, tls, Arc::new(https), Arc::new(routes), drain.clone())));
        // Postgres.
        if let Some(pg) = &p.pg {
            let l = bind(e.pg_listen).await?;
            let tls = roll_edge::tls::server_config(store.clone(), &[roll_edge::pg::ALPN]);
            served.push(io(roll_edge::pg::serve(l, tls, opts, hosts.clone(), pg.clone())));
        }
        // Redis: the server name picks the database (in a cluster, spliced to its owner right
        // after the handshake).
        if let Some(r) = p.redis {
            let l = bind(e.redis_listen).await?;
            let front = r.front.clone();
            served.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
                let fd = { use std::os::fd::AsRawFd; tls.get_ref().0.as_raw_fd() as i64 };
                front.clone().serve(tls, a.peer, a.local, a.sni, fd)
            })));
        }
        // Kafka: one virtual broker per hostname: clients are told the name they connected with.
        if let Some(b) = &p.streams {
            let l = bind(e.kafka_listen).await?;
            let b = b.clone();
            served.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
                let advertised = a.sni.clone().map(|s| (s, a.local.port()));
                stream_broker::server::serve_connection(b.clone(), tls, a.peer, true, advertised)
            })));
        }
        // nosql (CQL): the server name picks the database.
        if let Some(n) = p.nosql {
            let l = bind(e.nosql_listen).await?;
            let dir: Arc<dyn nosql_server::Directory> = n.control.node.clone();
            served.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
                let details = nosql_server::net::tls_details(tls.get_ref().1);
                let accepted = nosql_server::conn::Accepted { peer: a.peer, sni: a.sni, shard_aware: false, tls: details, certificate: None };
                nosql_server::conn::serve(Box::new(tls), accepted, dir.clone())
            })));
        }
        if let Some(q) = p.queues {
            served.extend(queue_listeners(e, q, &plain, opts).await?);
        }
        info!(https = %e.https_listen, listeners = served.len(), domain = %hosts.domain, site = %hosts.site_domain, proxy_protocol = e.proxy_protocol, "edge listening");

        // Healthy once there's a certificate to serve; draining from SIGTERM on.
        let (d, s) = (drain.clone(), store.clone());
        tokio::spawn(async move {
            while s.is_empty() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            d.set_serving(true);
        });
        let _ = STOPPING.set((drain.clone(), Duration::from_secs(e.drain_delay_s)));

        let mut internal = drain.router();
        if let Some(secret) = ["ROLL_CLUSTER_SECRET", "ROLL_CODE_CLUSTER_SECRET"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())) {
            let handlers = roll_edge::splice::Handlers::default();
            // A connection another node read the startup packet of: the proxy, as if it came here.
            if let Some(pg) = p.pg.clone() {
                handlers.register(
                    "pg",
                    Arc::new(move |io, hop| {
                        let pg = pg.clone();
                        Box::pin(async move {
                            if let Err(e) = pg.serve_connection(io, Some(hop.tenant)).await {
                                tracing::debug!(error = %e, "edge: spliced postgres connection ended with an error");
                            }
                        })
                    }),
                );
            }
            // A Redis connection another node read the TLS handshake (or the first AUTH) of.
            if let Some(r) = p.redis {
                handlers.register(redis_control::cluster::PRODUCT, r.front.handler());
            }
            // A CQL connection whose database this node serves.
            if let Some(n) = p.nosql {
                handlers.register("cql", n.splice_handler());
            }
            internal = internal.merge(roll_edge::splice::router(roll_edge::splice::Signer::new(&secret)?, handlers));
        }
        let serve = Box::pin(async move { futures::future::select_all(served).await.0 });
        Ok(Some(Edge { drain, internal, serve }))
    }

    /// Shared with whatever else needs to know the node is stopping.
    #[allow(dead_code)] // the cluster layer's drain observes it when it's wired in
    pub fn drain(&self) -> Drain {
        self.drain.clone()
    }
}

/// The edge's drain and how long it keeps serving once health checks fail.
static STOPPING: std::sync::OnceLock<(Drain, Duration)> = std::sync::OnceLock::new();

/// After the stop signal, before anything else stops: fail `/healthz` at once, keep the public
/// listeners serving while the NLB takes this node out, then close their connections. Returns
/// right away without the edge.
pub async fn stopping() {
    let Some((drain, delay)) = STOPPING.get() else { return };
    drain.start();
    info!(delay_s = delay.as_secs(), "edge: draining; health checks fail, listeners keep serving until the NLB lets go");
    tokio::time::sleep(*delay).await;
    drain.close();
    info!("edge: listeners closing");
}

/// The internal API's extra routes (`/healthz`, the splice hop) on top of `api`.
pub fn internal(api: axum::Router, edge: Option<&Edge>) -> axum::Router {
    match edge {
        Some(e) => api.merge(e.internal.clone()),
        None => api,
    }
}

/// Run the edge's listeners (forever, unless one fails); never resolves without the edge.
pub async fn serve(edge: Option<Edge>) -> anyhow::Result<()> {
    match edge {
        Some(e) => e.serve.await,
        None => std::future::pending().await,
    }
}

fn load_certificates(e: &EdgeArgs, hosts: &Hostnames, storage: &StorageConfig, store: &Arc<CertStore>, singleton: Arc<dyn Singleton>) -> anyhow::Result<()> {
    if let Some(dir) = &e.cert_dir {
        let mut d = roll_edge::tls::CertDir::new(dir);
        d.ensure_self_signed(&hosts.all_cert_names())?;
        d.reload(store)?;
        tokio::spawn(d.watch(store.clone(), Duration::from_secs(5)));
        return Ok(());
    }
    let secret = std::env::var("ROLL_SECRET_KEY").map_err(|_| anyhow::anyhow!("the edge needs ROLL_SECRET_KEY (stored certificates are sealed with it) or ROLL_EDGE_CERT_DIR"))?;
    let sealer = roll_edge::seal::Sealer::new(&secret)?;
    let objects = pg_storage::Objects::new_durable(storage)?;
    let issuer: Option<Arc<dyn roll_edge::acme::Issuer>> = match &e.acme_directory {
        Some(dir) => {
            let token = e.cloudflare_api_token.as_deref().ok_or_else(|| anyhow::anyhow!("ROLL_ACME_DIRECTORY needs ROLL_CLOUDFLARE_API_TOKEN (DNS-01)"))?;
            let dns = Arc::new(roll_edge::acme::Cloudflare::new(token, &e.cloudflare_api)?);
            let cfg = roll_edge::acme::AcmeConfig {
                email: e.acme_email.clone().filter(|m| !m.is_empty()),
                ca_file: e.acme_ca_file.clone(),
                propagation: Duration::from_secs(e.acme_propagation_s),
                ..roll_edge::acme::AcmeConfig::new(dir)
            };
            Some(Arc::new(roll_edge::acme::Acme::new(cfg, dns, objects.clone(), sealer.clone())))
        }
        None => {
            warn!("edge: no ROLL_ACME_DIRECTORY; serving only certificates other nodes issued");
            None
        }
    };
    let manager = Arc::new(CertManager::new(objects, sealer, store.clone(), hosts.cert_sets(), issuer, singleton));
    tokio::spawn(manager.run(Duration::from_secs(e.cert_poll_s.max(1))));
    Ok(())
}

async fn queue_listeners(e: &EdgeArgs, q: &crate::queues::Queues, plain: &Arc<tokio_rustls::rustls::ServerConfig>, opts: ListenOptions) -> anyhow::Result<Vec<Served>> {
    let mut out: Vec<Served> = vec![];
    let l = bind(e.amqps_listen).await?;
    let b = q.broker.clone();
    let amqp10 = mq_amqp10::handoff(mq_amqp10::Config { listen: e.amqps_listen, ..Default::default() });
    out.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
        let (b, amqp10) = (b.clone(), amqp10.clone());
        async move {
            if let Err(err) = mq_amqp::serve_connection(b, tls, a.peer, a.local, true, Some(amqp10)).await {
                tracing::debug!(peer = %a.peer, error = %err, "edge: amqps connection ended");
            }
        }
    })));
    let l = bind(e.mqtts_listen).await?;
    let (b, cfg) = (q.broker.clone(), Arc::new(mq_mqtt::Config { listen: e.mqtts_listen, ..Default::default() }));
    out.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
        let (b, cfg) = (b.clone(), cfg.clone());
        async move {
            if let Err(err) = mq_mqtt::serve_connection(b, tls, a.peer, a.local, true, cfg).await {
                tracing::debug!(peer = %a.peer, error = %err, "edge: mqtts connection ended");
            }
        }
    })));
    let l = bind(e.stomps_listen).await?;
    let (b, cfg) = (q.broker.clone(), Arc::new(mq_stomp::Config { listen: e.stomps_listen, ..Default::default() }));
    out.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
        let (b, cfg) = (b.clone(), cfg.clone());
        async move {
            if let Err(err) = mq_stomp::serve_connection(b, tls, a.peer, a.local, true, cfg).await {
                tracing::debug!(peer = %a.peer, error = %err, "edge: stomp TLS connection ended");
            }
        }
    })));
    let l = bind(e.mq_stream_listen).await?;
    let stream = q.stream.clone();
    out.push(io(listener::serve_tls(l, plain.clone(), opts, move |tls, a| {
        let stream = stream.clone();
        async move {
            if let Err(err) = stream.serve_connection(tls, a.peer, a.local, true).await {
                tracing::debug!(peer = %a.peer, error = %err, "edge: stream TLS connection ended");
            }
        }
    })));
    Ok(out)
}

/// The HTTPS listener's table: hostname → router or upstream.
struct HttpRoutes {
    api: axum::Router,
    dashboard: Option<Upstream>,
    redis: Option<axum::Router>,
    kafka: Option<axum::Router>,
    tasks: Option<axum::Router>,
    s3: Option<axum::Router>,
    alternator: Option<axum::Router>,
    /// Queues: web-STOMP, web-MQTT and the HTTP messaging API, on their own listeners.
    mq: Option<(Upstream, Upstream, Upstream)>,
}

impl HttpRoutes {
    fn new(args: &Args, p: &Products<'_>) -> anyhow::Result<Self> {
        let e = &args.edge;
        let mq = match p.queues {
            Some(_) => {
                let a = &args.queues;
                Some((Upstream::new(&local_url(a.web_stomp))?, Upstream::new(&local_url(a.web_mqtt))?, Upstream::new(&local_url(a.http))?))
            }
            None => None,
        };
        // Their access lines report the matched route (crate::logs).
        let logged = |r: axum::Router| r.layer(axum::middleware::from_fn(crate::logs::access));
        Ok(Self {
            api: p.api.clone(),
            dashboard: e.dashboard_upstream.as_deref().filter(|u| !u.is_empty()).map(Upstream::new).transpose()?,
            redis: p.redis.map(|r| logged(r.rest_router())),
            kafka: p.streams.clone().map(|b| logged(stream_broker::http::router(b))),
            tasks: p.tasks.clone().map(|t| logged(tasks::api::router(t))),
            s3: p.storage.clone().map(logged),
            alternator: p.nosql.map(|n| logged(n.alternator_router())),
            mq,
        })
    }
}

/// Which WebSocket protocol an upgrade asks for (`mqtt`, `v12.stomp`, …).
fn websocket_protocol(headers: &HeaderMap) -> Option<String> {
    headers.get(header::SEC_WEBSOCKET_PROTOCOL).and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase)
}

impl Routes for HttpRoutes {
    fn target(&self, host: &Host, uri: &Uri, headers: &HeaderMap) -> Target {
        let router = |r: &Option<axum::Router>| r.clone().map_or(Target::NotFound, Target::Router);
        match host {
            Host::Api | Host::Preview(_) => Target::Router(self.api.clone()),
            Host::Git => Target::Prefixed("/git", self.api.clone()),
            Host::Dashboard if roll_edge::hosts::is_git_path(uri.path()) => Target::Prefixed("/git", self.api.clone()),
            Host::Dashboard => self.dashboard.clone().map_or(Target::NotFound, Target::Proxy),
            Host::Redis(_) => router(&self.redis),
            Host::Kafka(_) => router(&self.kafka),
            Host::Tasks => router(&self.tasks),
            Host::S3 { .. } => router(&self.s3),
            Host::Nosql(_) => router(&self.alternator),
            Host::Mq(_) => match &self.mq {
                Some((stomp, mqtt, http)) => match websocket_protocol(headers) {
                    Some(p) if p.contains("mqtt") => Target::Proxy(mqtt.clone()),
                    Some(p) if p.contains("stomp") => Target::Proxy(stomp.clone()),
                    _ => Target::Proxy(http.clone()),
                },
                None => Target::NotFound,
            },
            // Public buckets and sites come through the CDN Worker, not straight here.
            Host::Pg(_) | Host::Site(_) | Host::Origin | Host::Unknown => Target::NotFound,
        }
    }
}
