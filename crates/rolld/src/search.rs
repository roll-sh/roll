//! roll search inside rolld: the Elasticsearch REST listener and the `/v1/search` control API.
//!
//! The node keeps segments, the WAL and cluster metadata in the same object storage as the rest
//! of rolld (`--s3-bucket`, or a directory under `--home`); local disk under
//! `<home>/search/cache` is only a cache. Documents are durable: writes are acknowledged once
//! the WAL holds them (local disk, or object storage with `--search-sync-objects`), and a
//! cluster that unloads is flushed first.
//!
//! **Fleet mode** (`--search-meta postgres://…`, with `--search-only` on AWS): the node joins a
//! fleet sharing one catalog. It takes a cluster's ownership lease before serving it,
//! heartbeats its addresses for `search-proxy`, acknowledges writes once they are in object
//! storage (optionally an S3 Express One Zone bucket for the WAL, `--search-wal-bucket`), and on
//! SIGTERM drains: it publishes `draining`, refuses new requests (`node_closed_exception`, which
//! the proxy resends to the next owner), lets those in flight finish, flushes and hands each
//! cluster's lease back, then leaves. See `docs/search/deploy-aws.md`.
//!
//! With TLS on the REST port (`--search-tls-cert`, or `--search-tls-self-signed` behind the
//! proxies), a `--search-only` node serves its control API with the same certificate too, and
//! publishes that in its record (`Beat::api_tls`), so proxies and other nodes reach both over
//! TLS pinned to it. The full rolld shares its API port with other services and keeps it plain.

use clap::Args;
use pg_storage::StorageConfig;
use search_control::{Control, FleetConfig, SearchConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

#[derive(Args, Clone, Debug)]
pub struct SearchArgs {
    /// Disable roll search.
    #[arg(long = "no-search", env = "ROLL_SEARCH_DISABLED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_disabled: bool,
    /// Elasticsearch REST (HTTP/HTTPS) listener.
    #[arg(long = "search-http", env = "ROLL_SEARCH_HTTP", default_value = "127.0.0.1:59200")]
    pub search_http: SocketAddr,
    /// Serve HTTPS with this PEM certificate chain (needs --search-tls-key). The TLS server name
    /// can select the cluster (`<cluster>.<--search-domain>`).
    #[arg(long = "search-tls-cert", env = "ROLL_SEARCH_TLS_CERT", requires = "search_tls_key")]
    pub search_tls_cert: Option<PathBuf>,
    #[arg(long = "search-tls-key", env = "ROLL_SEARCH_TLS_KEY", requires = "search_tls_cert")]
    pub search_tls_key: Option<PathBuf>,
    /// Serve HTTPS with a certificate made at boot (fleet nodes behind search-proxy: the
    /// heartbeat publishes its SHA-256 and the proxies accept only it).
    #[arg(long = "search-tls-self-signed", env = "ROLL_SEARCH_TLS_SELF_SIGNED", conflicts_with = "search_tls_cert", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_tls_self_signed: bool,
    /// Clusters are reached at `<cluster>.<domain>` (e.g. `search.example.com`): SNI or `Host`
    /// routing, and the host in cluster URLs.
    #[arg(long = "search-domain", env = "ROLL_SEARCH_DOMAIN")]
    pub search_domain: Option<String>,
    /// The URL clients reach clusters at when something sits in front of this node (a load
    /// balancer or a Lambda function URL): cluster URLs use its scheme, host and port.
    #[arg(long = "search-public-url", env = "ROLL_SEARCH_PUBLIC_URL")]
    pub search_public_url: Option<String>,
    /// Unload clusters idle this long, in seconds (a cluster's own idle timeout wins).
    #[arg(long = "search-idle-unload", env = "ROLL_SEARCH_IDLE_UNLOAD", default_value_t = 300)]
    pub search_idle_unload_s: u64,
    /// Local disk cache for segments, in bytes.
    #[arg(long = "search-cache-bytes", env = "ROLL_SEARCH_CACHE_BYTES", default_value_t = 10 << 30)]
    pub search_cache_bytes: u64,
    /// Memory all loaded clusters may hold together, in MiB (0: no budget). Past it the least
    /// recently used idle clusters unload.
    #[arg(long = "search-memory-mb", env = "ROLL_SEARCH_MEMORY_MB", default_value_t = 0)]
    pub search_memory_mb: u64,
    /// Acknowledge writes once the WAL is in object storage (for nodes with ephemeral disks;
    /// always on in fleet mode).
    #[arg(long = "search-sync-objects", env = "ROLL_SEARCH_SYNC_OBJECTS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_sync_objects: bool,
    /// Keep search data in this directory instead of rolld's object storage (several nodes of
    /// one host sharing a disk; tests).
    #[arg(long = "search-storage-dir", env = "ROLL_SEARCH_STORAGE_DIR")]
    pub search_storage_dir: Option<PathBuf>,
    /// Keep the WAL in this S3 Express One Zone directory bucket (`name--azid--x-s3`): acks in
    /// single-digit milliseconds instead of S3 Standard's tens.
    #[arg(long = "search-wal-bucket", env = "ROLL_SEARCH_WAL_BUCKET")]
    pub search_wal_bucket: Option<String>,
    /// Expect PROXY protocol headers on the REST port (behind an AWS NLB or HAProxy).
    #[arg(long = "search-proxy-protocol", env = "ROLL_SEARCH_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_proxy_protocol: bool,
    /// Point-in-time branching window, in seconds (0 disables it).
    #[arg(long = "search-history", env = "ROLL_SEARCH_HISTORY", default_value_t = 86_400)]
    pub search_history_s: u64,
    /// Collect unreferenced segments and WAL files every this many seconds (0: never). In a
    /// fleet, one node at a time runs it.
    #[arg(long = "search-gc-interval", env = "ROLL_SEARCH_GC_INTERVAL", default_value_t = 3600)]
    pub search_gc_interval_s: u64,
    /// Publish Elasticsearch's test cluster features (to run the YAML REST suites against
    /// clusters created through the API). Test deployments only.
    #[arg(long = "search-test-features", env = "ROLL_SEARCH_TEST_FEATURES", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_test_features: bool,
    /// Require `Authorization: Bearer <token>` on the `/v1/search` API.
    #[arg(long = "search-api-token", env = "ROLL_SEARCH_API_TOKEN", hide_env_values = true)]
    pub search_api_token: Option<String>,
    /// Join a fleet whose catalog, leases and node records live here: `postgres://…` (roll
    /// postgres or RDS), or a SQLite file shared by the nodes of one host.
    #[arg(long = "search-meta", env = "ROLL_SEARCH_META")]
    pub search_meta: Option<String>,
    /// Password for the fleet database, when the URL has none (an RDS-managed secret).
    #[arg(long = "search-meta-password", env = "ROLL_SEARCH_META_PASSWORD", hide_env_values = true)]
    pub search_meta_password: Option<String>,
    /// More CA certificates (PEM) to verify the fleet Postgres with (the image ships the RDS bundle).
    #[arg(long = "search-meta-ca", env = "ROLL_SEARCH_META_CA")]
    pub search_meta_ca: Option<PathBuf>,
    /// This node's id in the fleet (default: the ECS task id, else `--node`). Unique per
    /// running node.
    #[arg(long = "search-node-id", env = "ROLL_SEARCH_NODE_ID")]
    pub search_node_id: Option<String>,
    /// Availability zone (default: from the ECS task metadata); proxies prefer nodes in theirs.
    #[arg(long = "search-az", env = "ROLL_SEARCH_AZ")]
    pub search_az: Option<String>,
    /// Host proxies and other nodes reach this node on (default: its primary address).
    #[arg(long = "search-advertise-host", env = "ROLL_SEARCH_ADVERTISE_HOST")]
    pub search_advertise_host: Option<String>,
    /// Seconds a stopping node may take to hand its clusters over (Spot gives 120).
    #[arg(long = "search-shutdown-timeout", env = "ROLL_SEARCH_SHUTDOWN_TIMEOUT", default_value_t = 90)]
    pub search_shutdown_timeout_s: u64,
}

pub struct Search {
    pub control: Arc<Control>,
    listener: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
    opts: search_server::ListenOptions,
    /// Serve the control API over TLS (a `--search-only` node with TLS).
    api_tls: Option<tokio_rustls::TlsAcceptor>,
    stop: search_server::Shutdown,
    timeout: Duration,
}

fn advertise(host: Option<&str>, bind: SocketAddr) -> String {
    match host.filter(|h| !h.is_empty()) {
        Some(h) => format!("{h}:{}", bind.port()),
        None => search_proxy::aws::advertise_for(bind).to_string(),
    }
}

impl Search {
    /// `own_api`: the API listener at `api` serves only search (`--search-only`), so it serves
    /// TLS when the REST listener does ([`Search::serve_api`]).
    pub async fn start(home: &Path, args: &SearchArgs, storage: &StorageConfig, public_host: &str, node: &str, api: SocketAddr, own_api: bool) -> anyhow::Result<Self> {
        let mut cfg = SearchConfig::local(home.join("search"));
        cfg.node_name = node.to_string();
        cfg.public_host = public_host.to_string();
        cfg.http_port = args.search_http.port();
        cfg.domain = args.search_domain.clone().filter(|d| !d.is_empty());
        cfg.public_url = args.search_public_url.clone().filter(|u| !u.is_empty());
        cfg.idle_unload = Duration::from_secs(args.search_idle_unload_s.max(1));
        cfg.defaults.idle_timeout_s = args.search_idle_unload_s as i64;
        cfg.cache_bytes = args.search_cache_bytes;
        cfg.memory_budget = (args.search_memory_mb > 0).then_some(args.search_memory_mb << 20);
        cfg.sync_objects = args.search_sync_objects;
        cfg.history_retention = Duration::from_secs(args.search_history_s);
        cfg.test_features = args.search_test_features;
        cfg.gc_interval = (args.search_gc_interval_s > 0).then(|| Duration::from_secs(args.search_gc_interval_s));
        // TLS first: in a fleet, the heartbeat publishes the certificate's pin.
        let host = args.search_advertise_host.as_deref();
        let tls = match (&args.search_tls_cert, &args.search_tls_key) {
            (Some(c), Some(k)) => Some((std::fs::read(c)?, std::fs::read(k)?)),
            _ if args.search_tls_self_signed => {
                let ip = advertise(host, args.search_http);
                let ip = ip.rsplit_once(':').map_or(ip.as_str(), |(h, _)| h).trim_matches(['[', ']']).to_string();
                let c = search_proxy::upstream::self_signed(&[ip, "localhost".into()])?;
                Some((c.cert_pem.into_bytes(), c.key_pem.into_bytes()))
            }
            _ => None,
        };
        let tls_pin = match &tls {
            Some((cert, _)) => Some(search_proxy::upstream::pem_fingerprint(cert)?),
            None => None,
        };
        cfg.api_token = args.search_api_token.clone().filter(|t| !t.is_empty());
        if let Some(meta) = args.search_meta.clone().filter(|m| !m.is_empty()) {
            let node_id = match args.search_node_id.clone().filter(|n| !n.is_empty()) {
                Some(n) => n,
                None => search_proxy::aws::ecs_task_id().await.unwrap_or_else(|| node.to_string()),
            };
            // WAL objects are named by the node: every running node needs its own name.
            cfg.node_name = node_id.clone();
            let az = match args.search_az.clone().filter(|a| !a.is_empty()) {
                Some(a) => Some(a),
                None => search_proxy::aws::ecs_availability_zone().await,
            };
            let mut f = FleetConfig::new(meta, node_id, advertise(host, args.search_http));
            f.meta_ca = args.search_meta_ca.clone();
            f.meta_password = args.search_meta_password.clone();
            f.az = az;
            f.advertise_api = Some(advertise(host, api));
            f.api_tls = own_api && tls_pin.is_some();
            f.tls_pin = tls_pin;
            cfg.fleet = Some(f);
        }
        let mut opts = search_server::ListenOptions { proxy_protocol: args.search_proxy_protocol, ..Default::default() };
        if let Some((cert, key)) = &tls {
            opts.tls = Some(search_server::net::tls_acceptor(cert, key)?);
        }
        cfg.tls = opts.tls.is_some();
        let api_tls = if own_api { opts.tls.clone() } else { None };
        let wal = match args.search_wal_bucket.as_deref().filter(|b| !b.is_empty()) {
            Some(bucket) => {
                let region = match storage {
                    StorageConfig::S3 { region, .. } => region.clone(),
                    StorageConfig::Local(_) => std::env::var("AWS_REGION").ok(),
                };
                Some(search_store::Objects::new(&StorageConfig::S3 { bucket: bucket.to_string(), region, endpoint: None, prefix: Some("search-wal".into()) })?)
            }
            None => None,
        };
        let objects = match &args.search_storage_dir {
            Some(dir) => search_store::Objects::new(&StorageConfig::Local(dir.clone()))?,
            None => search_store::Objects::new(storage)?,
        };
        let control = Control::open(cfg, objects, wal).await?;
        let listener = tokio::net::TcpListener::bind(args.search_http).await.map_err(|e| anyhow::anyhow!("bind {}: {e}", args.search_http))?;
        info!(
            addr = %args.search_http,
            tls = control.cfg.tls,
            domain = ?control.cfg.domain,
            storage = %control.objects().description,
            fleet = control.fleet.as_ref().map(|f| f.node_id.as_str()),
            "search listening"
        );
        Ok(Search {
            control,
            listener: tokio::sync::Mutex::new(Some(listener)),
            opts,
            api_tls,
            stop: search_server::Shutdown::new(),
            timeout: Duration::from_secs(args.search_shutdown_timeout_s.max(1)),
        })
    }

    pub fn router(&self) -> axum::Router {
        search_control::api::router(self.control.clone())
    }

    /// Serves the control API on its own listener (`--search-only`): TLS with the REST
    /// listener's certificate when that serves TLS (and plain HTTP too while the fleet still
    /// has a node from before `api_tls`), else plain HTTP.
    pub async fn serve_api(&self, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
        match &self.api_tls {
            Some(acceptor) => axum::serve(search_control::api::TlsListener::for_fleet(listener, acceptor.clone(), self.control.clone())?, self.router()).await?,
            None => axum::serve(listener, self.router()).await?,
        }
        Ok(())
    }

    /// Whether [`Search::serve_api`] serves TLS.
    pub fn api_tls(&self) -> bool {
        self.api_tls.is_some()
    }

    /// Serves the REST listener until it fails or [`Search::shutdown`] has handed everything
    /// over. In a fleet, announces the node once it accepts.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = self.listener.lock().await.take().ok_or_else(|| anyhow::anyhow!("search listener already serving"))?;
        let serving = search_server::serve_until(listener, self.control.node.clone(), self.opts.clone(), self.stop.clone());
        let mut serving = std::pin::pin!(serving);
        // The listener is bound, so proxies may route here as soon as the record appears.
        tokio::select! {
            r = &mut serving => return r,
            r = self.control.announce() => r?,
        }
        serving.await
    }

    /// Drains: unloads every cluster (flushing and handing its lease back) while the listener
    /// still answers, accounts usage, makes the WAL durable, leaves the fleet, then stops
    /// accepting and lets connections finish. Returns when the connections still open must be
    /// given up: the whole stop, from the call to that instant, takes at most
    /// `--search-shutdown-timeout` ([`stop_phases`]), so it fits the orchestrator's kill
    /// deadline set from it (ECS `StopTimeout`, Spot's two minutes).
    pub async fn shutdown(&self) -> tokio::time::Instant {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let (drain, _) = stop_phases(self.timeout);
        let stop = self.control.shutdown_within(drain);
        match tokio::time::timeout_at(deadline, stop).await {
            Ok(Err(e)) => tracing::warn!(error = %e, "search shutdown"),
            Ok(Ok(())) => {}
            Err(_) => tracing::warn!("search shutdown: leaving the fleet ran out of time"),
        }
        self.stop.trigger();
        deadline
    }
}

/// How a stop of `total` splits: handing clusters over first, and a reserve after it for
/// metering, leaving the fleet and the last connections finishing (a quarter, at most 15 s).
pub fn stop_phases(total: Duration) -> (Duration, Duration) {
    let tail = (total / 4).min(Duration::from_secs(15));
    (total - tail, tail)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn a_stop_fits_its_timeout() {
        // The template's 100 s, inside ECS's 120 s StopTimeout.
        assert_eq!(super::stop_phases(Duration::from_secs(100)), (Duration::from_secs(85), Duration::from_secs(15)));
        assert_eq!(super::stop_phases(Duration::from_secs(20)), (Duration::from_secs(15), Duration::from_secs(5)));
        for s in [1, 2, 7, 60, 90, 3600] {
            let (drain, tail) = super::stop_phases(Duration::from_secs(s));
            assert_eq!(drain + tail, Duration::from_secs(s));
            assert!(drain >= tail, "{s}");
        }
    }

    #[test]
    fn advertised_hosts() {
        let bind: std::net::SocketAddr = "0.0.0.0:9200".parse().unwrap();
        assert_eq!(super::advertise(Some("10.0.1.5"), bind), "10.0.1.5:9200");
        assert!(super::advertise(None, bind).ends_with(":9200"));
        assert!(!super::advertise(None, bind).starts_with("0.0.0.0"));
        assert_eq!(super::advertise(None, "10.1.1.1:7070".parse().unwrap()), "10.1.1.1:7070");
    }
}
