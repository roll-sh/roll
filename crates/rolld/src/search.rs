//! roll search inside rolld: the Elasticsearch REST listener and the `/v1/search` control API.
//!
//! The node keeps segments, the WAL and cluster metadata in the same object storage as the rest
//! of rolld (`--s3-bucket`, or a directory under `--home`); local disk under
//! `<home>/search/cache` is only a cache. Documents are durable: writes are acknowledged once
//! the WAL holds them (local disk, or object storage with `--search-sync-objects`), and a
//! cluster that unloads is flushed first.
//!
//! On a cluster (a roll-cluster backend other than memory) every search cluster is served by the
//! holder of its lease `search:cluster:<name>` (`search_control::cluster`): the catalog is in the
//! system database (`ROLL_SYSTEM_DATABASE`, schema `search`), writes are acknowledged once their
//! WAL batch is in object storage, and this node's REST port runs each request on its cluster's
//! owner. `/v1/search` is served on rolld's API port with every other product's routes, behind
//! its operator gate (auth.rs).

use clap::Args;
use pg_storage::StorageConfig;
use search_control::{Control, SearchConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// `--search-request-memory-max`: refused at startup unless it's a byte size, a percentage above
/// 0 and at most 100, or -1.
fn request_memory_max(v: &str) -> Result<String, String> {
    search_control::request_max_limit(v, 1 << 30).map(|_| v.trim().to_string())
}

#[derive(Args, Clone, Debug)]
pub struct SearchArgs {
    /// Disable roll search.
    #[arg(long = "no-search", env = "ROLL_SEARCH_DISABLED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_disabled: bool,
    /// Run only roll search (`ROLL_PRODUCTS=search`).
    #[arg(long = "search-only", env = "ROLL_SEARCH_ONLY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_only: bool,
    /// Elasticsearch REST (HTTP/HTTPS) listener.
    #[arg(long = "search-http", env = "ROLL_SEARCH_HTTP", default_value = "127.0.0.1:59200")]
    pub search_http: SocketAddr,
    /// Serve HTTPS with this PEM certificate chain (needs --search-tls-key). The TLS server name
    /// can select the cluster (`<cluster>.<--search-domain>`). A single node only: on a cluster
    /// TLS ends at the edge.
    #[arg(long = "search-tls-cert", env = "ROLL_SEARCH_TLS_CERT", requires = "search_tls_key")]
    pub search_tls_cert: Option<PathBuf>,
    #[arg(long = "search-tls-key", env = "ROLL_SEARCH_TLS_KEY", requires = "search_tls_cert")]
    pub search_tls_key: Option<PathBuf>,
    /// Clusters are reached at `<cluster>.<domain>` (e.g. `search.example.com`): SNI or `Host`
    /// routing, and the host in cluster URLs.
    #[arg(long = "search-domain", env = "ROLL_SEARCH_DOMAIN")]
    pub search_domain: Option<String>,
    /// The URL clients reach clusters at when something sits in front of this node (a load
    /// balancer): cluster URLs use its scheme, host and port.
    #[arg(long = "search-public-url", env = "ROLL_SEARCH_PUBLIC_URL")]
    pub search_public_url: Option<String>,
    /// Unload clusters idle this long, in seconds (a cluster's own idle timeout wins).
    #[arg(long = "search-idle-unload", env = "ROLL_SEARCH_IDLE_UNLOAD", default_value_t = 300)]
    pub search_idle_unload_s: u64,
    /// Local disk cache for segments, in bytes.
    #[arg(long = "search-cache-bytes", env = "ROLL_SEARCH_CACHE_BYTES", default_value_t = 10 << 30)]
    pub search_cache_bytes: u64,
    /// Memory loaded clusters and their requests may hold together, in MiB (0: no budget for
    /// loaded clusters, and half of the host's memory for both). Past it in resident memory the
    /// least recently used idle clusters unload; a request (aggregations, ES|QL, kept results)
    /// that would take residents and requests past it fails with `circuit_breaking_exception`.
    #[arg(long = "search-memory-mb", env = "ROLL_SEARCH_MEMORY_MB", default_value_t = 0)]
    pub search_memory_mb: u64,
    /// The most one cluster's `request` circuit breaker may allow, whatever its
    /// `indices.breaker.request.limit`: a byte size (`512mb`) or a percentage of the node's memory
    /// for requests (default `25%`; `-1`: no maximum).
    #[arg(long = "search-request-memory-max", env = "ROLL_SEARCH_REQUEST_MEMORY_MAX", default_value = "25%", value_parser = request_memory_max)]
    pub search_request_memory_max: String,
    /// Memory one Painless script execution may hold, in MiB (0: no limit). Tenants can't
    /// raise it; past it the script fails with a `painless_error`.
    #[arg(long = "search-script-memory-mb", env = "ROLL_SEARCH_SCRIPT_MEMORY_MB", default_value_t = 64)]
    pub search_script_memory_mb: u64,
    /// Request body bytes all clusters together may have in flight on this node, in MiB (0: no
    /// node budget). Each cluster's own budget (twice its `max_body_mb`) is charged to it too;
    /// past it, requests get 429.
    #[arg(long = "search-max-in-flight-body-mb", env = "ROLL_SEARCH_MAX_IN_FLIGHT_BODY_MB", default_value_t = 4096)]
    pub search_max_in_flight_body_mb: u64,
    /// Without ROLL_SECRET_KEY, keep the secrets clients give search clusters (inference keys,
    /// watch passwords, connector secrets, repository credentials) in the clear instead of
    /// refusing them. Local development only.
    #[arg(long = "search-allow-unsealed-secrets", env = "ROLL_SEARCH_ALLOW_UNSEALED_SECRETS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_allow_unsealed_secrets: bool,
    /// Acknowledge writes once the WAL is in object storage (for nodes with ephemeral disks;
    /// always on in a cluster).
    #[arg(long = "search-sync-objects", env = "ROLL_SEARCH_SYNC_OBJECTS", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_sync_objects: bool,
    /// Keep search data in this directory instead of rolld's object storage (tests).
    #[arg(long = "search-storage-dir", env = "ROLL_SEARCH_STORAGE_DIR")]
    pub search_storage_dir: Option<PathBuf>,
    /// Keep the WAL in this S3 Express One Zone directory bucket (`name--azid--x-s3`): acks in
    /// single-digit milliseconds instead of S3 Standard's tens.
    #[arg(long = "search-wal-bucket", env = "ROLL_SEARCH_WAL_BUCKET")]
    pub search_wal_bucket: Option<String>,
    /// Expect PROXY protocol headers on the REST port (behind an AWS NLB or HAProxy).
    #[arg(long = "search-proxy-protocol", env = "ROLL_SEARCH_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_proxy_protocol: bool,
    /// The header a trusted proxy in front of the REST port puts the client's address in (e.g.
    /// `CF-Connecting-IP`), for clusters' IP allowlists. Believed only from a peer in
    /// --search-trusted-proxies; never `X-Forwarded-For`, which clients write themselves.
    #[arg(long = "search-client-ip-header", env = "ROLL_SEARCH_CLIENT_IP_HEADER")]
    pub search_client_ip_header: Option<String>,
    /// Comma-separated CIDRs of the proxies whose --search-client-ip-header is believed (the
    /// peer is the PROXY header's source with --search-proxy-protocol).
    #[arg(long = "search-trusted-proxies", env = "ROLL_SEARCH_TRUSTED_PROXIES")]
    pub search_trusted_proxies: Option<String>,
    /// Point-in-time branching window, in seconds (0 disables it).
    #[arg(long = "search-history", env = "ROLL_SEARCH_HISTORY", default_value_t = 86_400)]
    pub search_history_s: u64,
    /// Collect unreferenced segments and WAL files every this many seconds (0: never). In a
    /// cluster, one node at a time runs it.
    #[arg(long = "search-gc-interval", env = "ROLL_SEARCH_GC_INTERVAL", default_value_t = 3600)]
    pub search_gc_interval_s: u64,
    /// Days a deleted cluster's storage is kept, so `POST /v1/search/clusters/{id}/restore`
    /// can bring it back (billed as storage meanwhile); 0 deletes at once.
    #[arg(long = "search-deleted-retention-days", env = "ROLL_SEARCH_DELETED_RETENTION_DAYS", default_value_t = 7)]
    pub search_deleted_retention_days: u64,
    /// The name of every cluster's default snapshot repository, on roll's own object storage
    /// (empty: none).
    #[arg(long = "search-default-repository", env = "ROLL_SEARCH_DEFAULT_REPOSITORY", default_value = "found-snapshots")]
    pub search_default_repository: String,
    /// Let `s3` snapshot repositories reach private addresses (local development with MinIO).
    #[arg(long = "search-repository-allow-private", env = "ROLL_SEARCH_REPOSITORY_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_repository_allow_private: bool,
    /// Let connectors reach databases and buckets at private addresses (local development only:
    /// a tenant could otherwise point one at the node's own network).
    #[arg(long = "search-connectors-allow-private", env = "ROLL_SEARCH_CONNECTORS_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_connectors_allow_private: bool,
    /// Let inference endpoints reach private addresses (local development with a model server
    /// on this machine, tests).
    #[arg(long = "search-inference-allow-private", env = "ROLL_SEARCH_INFERENCE_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_inference_allow_private: bool,
    /// Let reindex from remote (and the dashboard's migration) reach private addresses (local
    /// development with an Elasticsearch on this machine, tests).
    #[arg(long = "search-reindex-allow-private", env = "ROLL_SEARCH_REINDEX_ALLOW_PRIVATE", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_reindex_allow_private: bool,
    /// PEM files of certificate authorities reindex from remote trusts besides the platform's
    /// (Elasticsearch's `reindex.ssl.certificate_authorities`; comma-separated): a remote
    /// cluster with a private CA.
    #[arg(long = "search-reindex-ca", env = "ROLL_SEARCH_REINDEX_CA")]
    pub search_reindex_ca: Option<String>,
    /// How long one reindex from remote task may run before it stops (an Elasticsearch time
    /// value: `24h`, `90m`), so a remote that answers slowly forever can't hold a task.
    #[arg(long = "search-reindex-max-duration", env = "ROLL_SEARCH_REINDEX_MAX_DURATION", default_value = "24h")]
    pub search_reindex_max_duration: String,
    /// Networks no outbound call of roll search reaches, whatever name leads there (CIDRs or
    /// addresses, comma-separated): reindex from remote and migration, connectors, inference
    /// services, watches and snapshot repositories. The fleet's own networks go here; this
    /// node's public addresses are added on their own. `--search-reindex-deny` and
    /// `ROLL_SEARCH_REINDEX_DENY` are its old names.
    #[arg(long = "search-egress-deny", alias = "search-reindex-deny", env = "ROLL_SEARCH_EGRESS_DENY")]
    pub search_egress_deny: Option<String>,
    /// The longest one request that searches may run on this node, in seconds (0: no cap),
    /// counted once for all its shards and phases. A request's own larger `timeout` is lowered
    /// to it; past it a search answers with what it collected, `timed_out: true`
    /// (Elasticsearch has no such cap). Unset: 60 on a daemon that serves
    /// others (ROLL_ADMIN_TOKEN set), none on an open one.
    #[arg(long = "search-max-time", env = "ROLL_SEARCH_MAX_TIME")]
    pub search_max_time_s: Option<u64>,
    /// Search clusters a signed-in person may own, branches included (0: no cap). The operator
    /// and admins aren't capped.
    #[arg(long = "search-max-clusters-per-person", env = "ROLL_SEARCH_MAX_CLUSTERS_PER_PERSON", default_value_t = 50)]
    pub search_max_clusters_per_person: u32,
    /// Of those, clusters that never unload (`idle_timeout_s` 0) a person may own (0: no cap).
    #[arg(long = "search-max-always-on-per-person", env = "ROLL_SEARCH_MAX_ALWAYS_ON_PER_PERSON", default_value_t = 2)]
    pub search_max_always_on_per_person: u32,
    /// Days each search cluster's security audit log keeps its entries.
    #[arg(long = "search-audit-retention-days", env = "ROLL_SEARCH_AUDIT_RETENTION_DAYS", default_value_t = 7)]
    pub search_audit_retention_days: u64,
    /// MiB of audit entries kept per search cluster (compressed); past it the oldest go.
    #[arg(long = "search-audit-max-mb", env = "ROLL_SEARCH_AUDIT_MAX_MB", default_value_t = 32)]
    pub search_audit_max_mb: u64,
    /// The embedding model behind the `.multilingual-e5-small-elasticsearch` inference endpoint
    /// (and endpoints of the `elasticsearch` service with that model): `e5` runs
    /// multilingual-e5-small on this node's CPUs; `off` answers as Elasticsearch without ML
    /// nodes.
    #[arg(long = "search-embed-model", env = "ROLL_SEARCH_EMBED_MODEL", default_value = "e5", value_parser = ["e5", "off"])]
    pub search_embed_model: String,
    /// Where the model's files are fetched from, once, into `<home>/search/models` (each file at
    /// `<url>/<file>`, checked against the pinned SHA-256 digests). Default: the pinned revision
    /// on Hugging Face.
    #[arg(long = "search-embed-url", env = "ROLL_SEARCH_EMBED_URL")]
    pub search_embed_url: Option<String>,
    /// Read the model's files (`model.safetensors`, `tokenizer.json` of the pinned revision)
    /// from this directory instead of fetching them (air-gapped nodes).
    #[arg(long = "search-embed-dir", env = "ROLL_SEARCH_EMBED_DIR")]
    pub search_embed_dir: Option<PathBuf>,
    /// Embedding worker threads (0: a third of the CPUs, or of --search-embed-cpus).
    #[arg(long = "search-embed-workers", env = "ROLL_SEARCH_EMBED_WORKERS", default_value_t = 0)]
    pub search_embed_workers: usize,
    /// CPUs the embedding workers are pinned to (`0-5,18-23`; empty: not pinned).
    #[arg(long = "search-embed-cpus", env = "ROLL_SEARCH_EMBED_CPUS", default_value = "")]
    pub search_embed_cpus: String,
    /// Drop the model from memory after this many seconds without inputs.
    #[arg(long = "search-embed-idle", env = "ROLL_SEARCH_EMBED_IDLE", default_value_t = 300)]
    pub search_embed_idle_s: u64,
    /// Inputs, and MiB of them, one cluster may have queued for the model; past either a call
    /// gets 429.
    #[arg(long = "search-embed-queue", env = "ROLL_SEARCH_EMBED_QUEUE", default_value_t = 4096)]
    pub search_embed_queue: usize,
    #[arg(long = "search-embed-queue-mb", env = "ROLL_SEARCH_EMBED_QUEUE_MB", default_value_t = 64)]
    pub search_embed_queue_mb: usize,
    /// MiB of inputs every cluster's queues may hold together on the node; past it a call gets
    /// 429.
    #[arg(long = "search-embed-node-queue-mb", env = "ROLL_SEARCH_EMBED_NODE_QUEUE_MB", default_value_t = 512)]
    pub search_embed_node_queue_mb: usize,
    /// Inputs one inference call may carry (each input is cut to the start its 512 tokens come
    /// from, so its length isn't capped).
    #[arg(long = "search-embed-max-inputs", env = "ROLL_SEARCH_EMBED_MAX_INPUTS", default_value_t = 2048)]
    pub search_embed_max_inputs: usize,
    /// Publish Elasticsearch's test cluster features (to run the YAML REST suites against
    /// clusters created through the API). Test deployments only.
    #[arg(long = "search-test-features", env = "ROLL_SEARCH_TEST_FEATURES", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub search_test_features: bool,
}

/// The node's embedding pool from the `--search-embed-*` flags (`None`: the model is off).
fn embed_config(home: &Path, args: &SearchArgs) -> anyhow::Result<Option<search_server::embed::EmbedConfig>> {
    use search_server::embed::{EmbedConfig, ModelSource};
    if args.search_embed_model == "off" {
        return Ok(None);
    }
    let source = match (&args.search_embed_dir, args.search_embed_url.as_deref().filter(|u| !u.is_empty())) {
        (Some(_), Some(_)) => anyhow::bail!("--search-embed-dir and --search-embed-url exclude each other"),
        (Some(d), None) => ModelSource::Dir(d.clone()),
        (None, Some(u)) => ModelSource::Fetch { base: u.trim_end_matches('/').to_string() },
        (None, None) => ModelSource::default_fetch(),
    };
    let mut cfg = EmbedConfig::new(source, home.join("search").join("models"));
    cfg.workers = args.search_embed_workers;
    cfg.cpus = search_server::embed::parse_cpus(&args.search_embed_cpus)?;
    cfg.idle_unload = Duration::from_secs(args.search_embed_idle_s.max(1));
    cfg.queue_inputs = args.search_embed_queue.max(1);
    cfg.max_inputs = args.search_embed_max_inputs.max(1);
    cfg.queue_bytes = args.search_embed_queue_mb.max(1) << 20;
    cfg.node_queue_bytes = args.search_embed_node_queue_mb.max(1) << 20;
    Ok(Some(cfg))
}

pub struct Search {
    pub control: Arc<Control>,
    /// The roll-cluster fleet clusters are served in (`None`: this node alone owns them all).
    cluster: Option<Arc<roll_cluster::Cluster>>,
    listener: tokio::sync::Mutex<Option<tokio::net::TcpListener>>,
    opts: search_server::ListenOptions,
    stop: search_server::Shutdown,
}

/// `--search-max-time` on a daemon that serves others.
const DEFAULT_MAX_TIME_S: u64 = 60;

/// How long a stop may take to flush what's still loaded (a drain has handed the rest over).
const SHUTDOWN: Duration = Duration::from_secs(60);

impl Search {
    /// `cluster`: the node's roll-cluster handle. On a cluster backend other than memory every
    /// cluster is served by the holder of its lease (`search_control::cluster`), with the catalog
    /// in the system database and acknowledgements once writes are in object storage.
    #[allow(clippy::too_many_arguments)]
    /// `open`: the daemon enforces no operator token (one person's own node).
    pub async fn start(home: &Path, args: &SearchArgs, storage: &StorageConfig, public_host: &str, node: &str, cluster: &Arc<roll_cluster::Cluster>, system_database: Option<&str>, db_connections: usize, public: Option<&crate::edge::Public>, secret_key: Option<&str>, open: bool) -> anyhow::Result<Self> {
        let mut cfg = SearchConfig::local(home.join("search"));
        let fleet = (cluster.backend_name() != "memory").then(|| cluster.clone());
        cfg.node_name = node.to_string();
        if fleet.is_some() {
            anyhow::ensure!(!matches!(storage, StorageConfig::Local(_)) || args.search_storage_dir.is_some(), "search in a cluster needs S3 (--s3-bucket): the other nodes can't read a local directory");
            let Some(db) = system_database.filter(|u| !u.is_empty()) else {
                anyhow::bail!("search in a cluster needs the system database (ROLL_SYSTEM_DATABASE): every node must see the same catalog");
            };
            cfg.catalog = Some(db.to_string());
            cfg.catalog_connections = db_connections;
            // Another node replays this one's WAL after a takeover.
            cfg.sync_objects = true;
            // WAL objects are named by the node: the cluster's node id is unique per process.
            cfg.node_name = cluster.node_id().to_string();
        }
        cfg.public_host = public_host.to_string();
        cfg.http_port = args.search_http.port();
        cfg.domain = args.search_domain.clone().filter(|d| !d.is_empty());
        cfg.public_url = args.search_public_url.clone().filter(|u| !u.is_empty());
        cfg.idle_unload = Duration::from_secs(args.search_idle_unload_s.max(1));
        cfg.defaults.idle_timeout_s = args.search_idle_unload_s as i64;
        cfg.cache_bytes = args.search_cache_bytes;
        cfg.memory_budget = (args.search_memory_mb > 0).then_some(args.search_memory_mb << 20);
        // every cluster's script engine is made after this
        search_painless::set_default_max_execution_bytes(args.search_script_memory_mb << 20);
        cfg.max_in_flight_bytes = (args.search_max_in_flight_body_mb > 0).then(|| usize::try_from(args.search_max_in_flight_body_mb << 20).unwrap_or(usize::MAX));
        cfg.sync_objects |= args.search_sync_objects;
        cfg.history_retention = Duration::from_secs(args.search_history_s);
        cfg.test_features = args.search_test_features;
        cfg.gc_interval = (args.search_gc_interval_s > 0).then(|| Duration::from_secs(args.search_gc_interval_s));
        cfg.deleted_retention = Duration::from_secs(args.search_deleted_retention_days.saturating_mul(86_400));
        cfg.default_repository = Some(args.search_default_repository.trim().to_string()).filter(|n| !n.is_empty());
        cfg.person_caps = search_control::store::Caps { clusters: args.search_max_clusters_per_person.into(), always_on: args.search_max_always_on_per_person.into() };
        if args.search_connectors_allow_private {
            cfg.node_settings.push((search_control::CONNECTORS_ALLOW_PRIVATE.to_string(), "true".to_string()));
        }
        cfg.audit.retention = Duration::from_secs(args.search_audit_retention_days.max(1) * 86_400);
        cfg.audit.max_bytes = args.search_audit_max_mb.max(1) << 20;
        cfg.node_settings.push((search_control::SEARCH_REQUEST_MAX_LIMIT.to_string(), args.search_request_memory_max.trim().to_string()));
        cfg.embed = embed_config(home, args)?;
        if args.search_repository_allow_private {
            cfg.node_settings.push((search_control::S3_ALLOW_PRIVATE.to_string(), "true".to_string()));
        }
        if args.search_inference_allow_private {
            cfg.node_settings.push((search_control::INFERENCE_ALLOW_PRIVATE.to_string(), "true".to_string()));
        }
        if args.search_reindex_allow_private {
            cfg.node_settings.push((search_control::REINDEX_ALLOW_PRIVATE.to_string(), "true".to_string()));
        }
        if let Some(ca) = args.search_reindex_ca.as_deref().filter(|c| !c.trim().is_empty()) {
            cfg.node_settings.push(("reindex.ssl.certificate_authorities".to_string(), ca.to_string()));
        }
        search_control::check_time_value(&args.search_reindex_max_duration, "--search-reindex-max-duration")?;
        cfg.node_settings.push((search_control::REINDEX_MAX_DURATION.to_string(), args.search_reindex_max_duration.clone()));
        match args.search_max_time_s.unwrap_or(if open { 0 } else { DEFAULT_MAX_TIME_S }) {
            0 => {}
            s => cfg.node_settings.push((search_control::SEARCH_MAX_TIME.to_string(), format!("{s}s"))),
        }
        // Watch passwords, connector and Fleet secrets are sealed in cluster state under the
        // deployment's key. Without one, watch and Fleet secrets are stored as given (as on
        // Elasticsearch), and a connector roll runs refuses its credentials.
        cfg.sealer = secret_key.and_then(search_control::Sealer::from_secret);
        cfg.unsealed_secrets = args.search_allow_unsealed_secrets;
        // Behind the edge, cluster URLs name `<cluster>.search.<region>.<domain>` over HTTPS.
        if let Some(p) = public {
            p.search(&mut cfg);
        }
        // Reindex from remote never reads this node's own endpoints: a tenant's whitelist naming
        // them would read other tenants' clusters through roll (fleet nodes' addresses are
        // private, refused unless private hosts are allowed). The addresses below cover every
        // other outbound call too (search_es::egress).
        let mut blocked = vec![format!("{}:*", public_host.to_ascii_lowercase())];
        if let Some(d) = &cfg.domain {
            blocked.push(format!("{d}:*"));
            blocked.push(format!("*.{d}:*"));
        }
        if let Some(h) = cfg.public_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string)) {
            blocked.push(format!("{h}:*"));
        }
        cfg.node_settings.push(("reindex.remote.blocklist".to_string(), blocked.join(",")));
        // The same endpoints by address, checked on the address a connection uses: a name
        // spelled another way, an IP literal or any DNS name pointing here leads nowhere.
        // Private addresses are refused anyway unless private hosts are allowed (development).
        let given = args.search_egress_deny.clone().or_else(|| std::env::var("ROLL_SEARCH_REINDEX_DENY").ok());
        let mut deny: Vec<String> = given.as_deref().map(|d| d.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
        for n in &deny {
            anyhow::ensure!(search_control::egress_deny_net_ok(n), "--search-egress-deny: [{n}] isn't an address or network");
        }
        let mut names: Vec<String> = vec![public_host.to_string()];
        names.extend(cfg.domain.clone());
        // A name under the clusters' wildcard: where `<anything>.<domain>` leads.
        names.extend(cfg.domain.as_deref().map(|d| format!("roll-reindex-probe.{d}")));
        names.extend(cfg.public_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string)));
        // Resolved again on every connection (every 30s at most), so the check follows them.
        cfg.node_settings.push((search_control::EGRESS_DENY_NAMES.to_string(), names.join(",")));
        // And their addresses now, for literals (which never reach the resolver).
        for name in names {
            let name = name.trim_start_matches('[').trim_end_matches(']').to_string();
            let addrs: Vec<std::net::IpAddr> = match name.parse::<std::net::IpAddr>() {
                Ok(ip) => vec![ip],
                Err(_) => tokio::net::lookup_host((name.as_str(), 0)).await.map(|a| a.map(|s| s.ip()).collect()).unwrap_or_default(),
            };
            deny.extend(addrs.into_iter().filter(|ip| !roll_net::private_ip(*ip)).map(|ip| ip.to_string()));
        }
        deny.sort();
        deny.dedup();
        if !deny.is_empty() {
            cfg.node_settings.push((search_control::EGRESS_DENY_ADDRESSES.to_string(), deny.join(",")));
        }
        let client = search_server::ClientAddresses::new(args.search_client_ip_header.as_deref(), args.search_trusted_proxies.as_deref())?;
        let mut opts = search_server::ListenOptions { proxy_protocol: args.search_proxy_protocol, client, ..Default::default() };
        if let (Some(c), Some(k)) = (&args.search_tls_cert, &args.search_tls_key) {
            anyhow::ensure!(fleet.is_none(), "--search-tls-cert is for a single node: in a cluster TLS ends at the edge");
            opts.tls = Some(search_server::net::tls_acceptor(&std::fs::read(c)?, &std::fs::read(k)?)?);
        }
        cfg.tls = opts.tls.is_some();
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
        if let Some(c) = &fleet {
            control.join(c.clone())?;
        }
        let listener = crate::listen::bind(args.search_http).map_err(|e| anyhow::anyhow!("bind {}: {e}", args.search_http))?;
        info!(addr = %args.search_http, tls = control.cfg.tls, domain = ?control.cfg.domain, storage = %control.objects().description, clustered = fleet.is_some(), "search listening");
        Ok(Search { control, cluster: fleet, listener: tokio::sync::Mutex::new(Some(listener)), opts, stop: search_server::Shutdown::new() })
    }

    pub fn router(&self) -> axum::Router {
        search_control::api::router(self.control.clone())
    }

    /// The Elasticsearch REST API for the edge's `<cluster>.search.…` hostnames: each request
    /// runs on its cluster's owner (here on a node alone).
    pub fn rest_router(&self) -> axum::Router {
        search_control::cluster::rest_router(self.control.clone())
    }

    /// The internal route REST requests other nodes forward come in on (their cluster is here).
    pub fn internal(&self, router: axum::Router) -> axum::Router {
        match &self.cluster {
            Some(c) => router.merge(search_control::cluster::internal_router(self.control.clone(), c.clone())),
            None => router,
        }
    }

    /// Serves the REST listener until it fails or [`Search::shutdown`] stops it.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let listener = self.listener.lock().await.take().ok_or_else(|| anyhow::anyhow!("search listener already serving"))?;
        // On a cluster each request runs on its cluster's owner: the front names the cluster,
        // decides the client's address (for IP allowlists) and dispatches it, the address
        // signed (`search_control::cluster`).
        if self.cluster.is_some() {
            let stop = self.stop.clone();
            search_control::cluster::serve_front(listener, self.control.clone(), &self.opts, async move { stop.wait().await }).await?;
            return Ok(());
        }
        search_server::serve_until(listener, self.control.node.clone(), self.opts.clone(), self.stop.clone()).await
    }

    /// Unloads every cluster still loaded here (flushing it and giving its lease back),
    /// accounts usage and makes the WAL durable, then stops the REST listener.
    pub async fn shutdown(&self) {
        if let Err(e) = self.control.shutdown_within(SHUTDOWN).await {
            tracing::warn!(error = %e, "search shutdown");
        }
        self.stop.trigger();
    }
}
