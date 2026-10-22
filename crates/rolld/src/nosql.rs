//! roll nosql inside rolld: the control plane (`/v1/nosql`), the CQL listeners (plain and
//! ScyllaDB shard-aware, optionally TLS with SNI routing and the PROXY protocol), Alternator (the
//! DynamoDB API, over HTTP and optionally HTTPS, routed by `Host` or by the SigV4 access key
//! `role@db`) and ScyllaDB's REST API. Databases live in the shared object storage under
//! `nosql/`.

use clap::Args;
use nosql_control::{Control, NosqlConfig};
use nosql_alternator::WriteIsolation;
use nosql_server::{Directory, ListenOptions};
use pg_storage::StorageConfig;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Args, Clone, Debug)]
pub struct NosqlArgs {
    /// Disable roll nosql.
    #[arg(long = "no-nosql", env = "ROLL_NOSQL_DISABLED", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub nosql_disabled: bool,
    /// Run only roll nosql (no Postgres control plane, proxy, Redis, streams, tasks or queues).
    #[arg(long = "nosql-only", env = "ROLL_NOSQL_ONLY", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub nosql_only: bool,
    /// CQL listener (drivers, cqlsh).
    #[arg(long = "nosql-cql", env = "ROLL_NOSQL_CQL", default_value = "127.0.0.1:59042")]
    pub nosql_cql: SocketAddr,
    /// ScyllaDB's shard-aware CQL port (advertised to ScyllaDB drivers).
    #[arg(long = "nosql-cql-shard-aware", env = "ROLL_NOSQL_CQL_SHARD_AWARE", default_value = "127.0.0.1:59043")]
    pub nosql_cql_shard_aware: SocketAddr,
    /// Alternator (DynamoDB API) over HTTP.
    #[arg(long = "nosql-alternator", env = "ROLL_NOSQL_ALTERNATOR", default_value = "127.0.0.1:58000")]
    pub nosql_alternator: SocketAddr,
    /// Alternator over HTTPS (with --nosql-tls-cert / --nosql-tls-key).
    #[arg(long = "nosql-alternator-https", env = "ROLL_NOSQL_ALTERNATOR_HTTPS", requires = "nosql_tls_cert")]
    pub nosql_alternator_https: Option<SocketAddr>,
    /// DNS suffix of Alternator host names (`alternator.example.com` → `<id>.alternator.example.com`).
    /// Defaults to `alternator.<domain>` when --nosql-domain is `nosql.<domain>`.
    #[arg(long = "nosql-alternator-domain", env = "ROLL_NOSQL_ALTERNATOR_DOMAIN")]
    pub nosql_alternator_domain: Option<String>,
    /// How often Alternator deletes expired items (`alternator_ttl_period_in_seconds`).
    #[arg(long = "nosql-alternator-ttl-period", env = "ROLL_NOSQL_ALTERNATOR_TTL_PERIOD", default_value_t = 86_400.0)]
    pub nosql_alternator_ttl_period: f64,
    /// ScyllaDB's REST API (`nodetool`).
    #[arg(long = "nosql-rest", env = "ROLL_NOSQL_REST", default_value = "127.0.0.1:59100")]
    pub nosql_rest: SocketAddr,
    /// TLS certificate chain and key (PEM) for the CQL ports; the TLS server name
    /// (`<db>.<domain>`) selects the database.
    #[arg(long = "nosql-tls-cert", env = "ROLL_NOSQL_TLS_CERT", requires = "nosql_tls_key")]
    pub nosql_tls_cert: Option<PathBuf>,
    #[arg(long = "nosql-tls-key", env = "ROLL_NOSQL_TLS_KEY", requires = "nosql_tls_cert")]
    pub nosql_tls_key: Option<PathBuf>,
    /// CA certificates (PEM) that client certificates must chain to (`client_encryption_options`'
    /// truststore); the certificate authenticators read the verified certificate.
    #[arg(long = "nosql-tls-client-ca", env = "ROLL_NOSQL_TLS_CLIENT_CA", requires = "nosql_tls_cert")]
    pub nosql_tls_client_ca: Option<PathBuf>,
    /// Client certificates on the CQL TLS ports (`require_client_auth`): `false` (not asked
    /// for), `optional` (verified when sent) or `true` (required). Needs --nosql-tls-client-ca.
    #[arg(long = "nosql-tls-client-auth", env = "ROLL_NOSQL_TLS_CLIENT_AUTH", default_value = "false", value_parser = ["false", "optional", "true", "none", "required"])]
    pub nosql_tls_client_auth: String,
    /// Serve TLS and plain-text clients on the same CQL ports (Cassandra's
    /// `client_encryption_options.optional`).
    #[arg(long = "nosql-tls-optional", env = "ROLL_NOSQL_TLS_OPTIONAL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub nosql_tls_optional: bool,
    /// A dedicated TLS CQL port (`native_transport_port_ssl`): --nosql-cql stays plain text.
    #[arg(long = "nosql-cql-tls", env = "ROLL_NOSQL_CQL_TLS", requires = "nosql_tls_cert")]
    pub nosql_cql_tls: Option<SocketAddr>,
    /// A dedicated TLS shard-aware port (`native_shard_aware_transport_port_ssl`):
    /// --nosql-cql-shard-aware stays plain text.
    #[arg(long = "nosql-cql-shard-aware-tls", env = "ROLL_NOSQL_CQL_SHARD_AWARE_TLS", requires = "nosql_tls_cert")]
    pub nosql_cql_shard_aware_tls: Option<SocketAddr>,
    /// Expect PROXY protocol headers on the CQL ports (behind an AWS NLB or HAProxy).
    #[arg(long = "nosql-proxy-protocol", env = "ROLL_NOSQL_PROXY_PROTOCOL", value_parser = clap::builder::BoolishValueParser::new(), default_value_t = false, num_args = 0..=1, default_missing_value = "true")]
    pub nosql_proxy_protocol: bool,
    /// SNI suffix of database host names (`nosql.example.com` → `<id>.nosql.example.com`).
    #[arg(long = "nosql-domain", env = "ROLL_NOSQL_DOMAIN")]
    pub nosql_domain: Option<String>,
    /// Single mode: connections that name no database use this one (created when missing,
    /// with `AllowAllAuthenticator` like the reference images).
    #[arg(long = "nosql-single", env = "ROLL_NOSQL_SINGLE")]
    pub nosql_single: Option<String>,
    /// Dialect of new databases, single mode's included: `scylla` or `cassandra`.
    #[arg(long = "nosql-dialect", env = "ROLL_NOSQL_DIALECT", default_value = "scylla", value_parser = ["scylla", "cassandra"])]
    pub nosql_dialect: String,
    /// Datacenter and rack of new databases (what `system.local` and drivers see).
    #[arg(long = "nosql-datacenter", env = "ROLL_NOSQL_DATACENTER", default_value = "datacenter1")]
    pub nosql_datacenter: String,
    #[arg(long = "nosql-rack", env = "ROLL_NOSQL_RACK", default_value = "rack1")]
    pub nosql_rack: String,
    /// Single mode: the cluster name of the database when it is created (default: its name;
    /// may be empty, as ScyllaDB's is by default).
    #[arg(long = "nosql-cluster-name", env = "ROLL_NOSQL_CLUSTER_NAME")]
    pub nosql_cluster_name: Option<String>,
    /// Single mode: the initial superuser's password when the database is created (default:
    /// random; the superuser is `cassandra`).
    #[arg(long = "nosql-single-password", env = "ROLL_NOSQL_SINGLE_PASSWORD", hide_env_values = true)]
    pub nosql_single_password: Option<String>,
    /// Single mode: the database's settings as KEY=VALUE (`authenticator=PasswordAuthenticator`,
    /// `user_defined_functions_enabled=true`, …; comma-separated in the environment). When
    /// given, they replace the stored settings at every start.
    #[arg(long = "nosql-single-setting", env = "ROLL_NOSQL_SINGLE_SETTINGS", value_name = "KEY=VALUE", value_delimiter = ',')]
    pub nosql_single_settings: Vec<String>,
    /// Single mode: more settings as one JSON object, applied after --nosql-single-setting (for
    /// values with commas, such as `auth_certificate_role_queries`).
    #[arg(long = "nosql-single-settings-json", env = "ROLL_NOSQL_SINGLE_SETTINGS_JSON")]
    pub nosql_single_settings_json: Option<String>,
    /// Single mode: seed a new database from a branch before serving: `<api url>/<branch>` (a
    /// roll API and a database name or id) or a live cluster (`cql://user:pass@host:9042`).
    /// Runs once, when the database has never been seeded.
    #[arg(long = "nosql-from", env = "ROLL_NOSQL_FROM")]
    pub nosql_from: Option<String>,
    /// Bearer token for the `--nosql-from` API.
    #[arg(long = "nosql-from-token", env = "ROLL_NOSQL_FROM_TOKEN", hide_env_values = true)]
    pub nosql_from_token: Option<String>,
    /// Keyspaces to copy with `--nosql-from` (default: every non-system keyspace).
    #[arg(long = "nosql-from-keyspaces", env = "ROLL_NOSQL_FROM_KEYSPACES", value_delimiter = ',')]
    pub nosql_from_keyspaces: Vec<String>,
    /// Alternator's default write isolation (`--alternator-write-isolation`): always_use_lwt,
    /// only_rmw_uses_lwt, forbid_rmw or unsafe_rmw.
    #[arg(long = "nosql-alternator-write-isolation", env = "ROLL_NOSQL_ALTERNATOR_WRITE_ISOLATION")]
    pub nosql_alternator_write_isolation: Option<String>,
    /// ScyllaDB's Prometheus endpoint (`/metrics`, ScyllaDB's port 9180) for the single-mode
    /// database. Off by default.
    #[arg(long = "nosql-metrics", env = "ROLL_NOSQL_METRICS")]
    pub nosql_metrics: Option<SocketAddr>,
    /// Memtables of every database together (`512MiB`, `1GiB`, bytes): past half of it the
    /// largest are flushed; at it, writes of the databases holding more than their share wait
    /// (and time out after 2 s, as on a reference whose memtables are full).
    #[arg(long = "nosql-memtable-budget", env = "ROLL_NOSQL_MEMTABLE_BUDGET", default_value = "512MiB", value_parser = parse_size)]
    pub nosql_memtable_budget: u64,
    /// A database's memtable cap unless its `memtable_space` setting (its plan) sets one.
    /// Default: the whole budget in single mode, half of it otherwise.
    #[arg(long = "nosql-memtable-db-cap", env = "ROLL_NOSQL_MEMTABLE_DB_CAP", value_parser = parse_size)]
    pub nosql_memtable_db_cap: Option<u64>,
    /// Decoded SSTable blocks kept in memory, shared by every database.
    #[arg(long = "nosql-cache-memory", env = "ROLL_NOSQL_CACHE_MEMORY", default_value = "256MiB", value_parser = parse_size)]
    pub nosql_cache_memory: u64,
    /// Stored SSTable blocks kept on local disk (under `--home`), shared by every database. Only
    /// with S3 storage: local storage is already on disk. 0 disables it.
    #[arg(long = "nosql-cache-disk", env = "ROLL_NOSQL_CACHE_DISK", default_value = "4GiB", value_parser = parse_size)]
    pub nosql_cache_disk: u64,
    /// Memtables written out at once (Cassandra's `memtable_flush_writers`).
    #[arg(long = "nosql-flush-concurrency", env = "ROLL_NOSQL_FLUSH_CONCURRENCY", default_value_t = 2)]
    pub nosql_flush_concurrency: usize,
    /// Compactions running at once (Cassandra's `concurrent_compactors`).
    #[arg(long = "nosql-compaction-concurrency", env = "ROLL_NOSQL_COMPACTION_CONCURRENCY", default_value_t = 2)]
    pub nosql_compaction_concurrency: usize,
}

/// A size: bytes, or a number with a unit (`512MiB`, `512M`, `1GiB`, `1G`, `64KiB`).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (n, unit) = t.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("bad size {s:?} (bytes, or a number with KiB, MiB or GiB)"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        _ => return Err(format!("bad size unit in {s:?} (KiB, MiB, GiB or TiB)")),
    };
    n.checked_mul(mult).ok_or_else(|| format!("size {s:?} is too large"))
}

/// The node's storage budgets from the flags.
fn storage_config(args: &NosqlArgs, home: &Path, storage: &StorageConfig) -> nosql_storage::store::StoreConfig {
    let mut c = nosql_storage::store::StoreConfig::default();
    let single = args.nosql_single.as_deref().is_some_and(|s| !s.is_empty());
    c.memtable_budget_bytes = args.nosql_memtable_budget.max(1 << 20);
    c.memtable_db_bytes = args.nosql_memtable_db_cap.unwrap_or(if single { c.memtable_budget_bytes } else { c.memtable_budget_bytes / 2 }).max(1 << 20);
    c.cache.memory_bytes = args.nosql_cache_memory;
    if matches!(storage, StorageConfig::S3 { .. }) && args.nosql_cache_disk > 0 {
        c.cache.disk_dir = Some(home.join("nosql").join("cache"));
        c.cache.disk_bytes = args.nosql_cache_disk;
    }
    c.flush_concurrency = args.nosql_flush_concurrency.max(1);
    c.compaction_concurrency = args.nosql_compaction_concurrency.max(1);
    c
}

pub struct Nosql {
    pub control: Arc<Control>,
    args: NosqlArgs,
    /// Server authentication only (Alternator's HTTPS).
    tls: Option<tokio_rustls::TlsAcceptor>,
    /// The CQL ports' TLS (with client certificate verification when configured).
    cql_tls: Option<tokio_rustls::TlsAcceptor>,
}

impl Nosql {
    pub async fn start(home: &Path, args: NosqlArgs, public_host: &str, node: &str, storage: &StorageConfig) -> anyhow::Result<Self> {
        let client_auth = nosql_server::net::ClientAuth::parse(&args.nosql_tls_client_auth).unwrap_or_default();
        anyhow::ensure!(
            client_auth == nosql_server::net::ClientAuth::None || (args.nosql_tls_cert.is_some() && args.nosql_tls_client_ca.is_some()),
            "--nosql-tls-client-auth {} needs --nosql-tls-cert, --nosql-tls-key and --nosql-tls-client-ca",
            args.nosql_tls_client_auth
        );
        let (tls, cql_tls) = match (&args.nosql_tls_cert, &args.nosql_tls_key) {
            (Some(c), Some(k)) => {
                let (cert, key) = (std::fs::read(c)?, std::fs::read(k)?);
                let ca = args.nosql_tls_client_ca.as_ref().map(std::fs::read).transpose()?;
                let cql = nosql_server::net::tls_acceptor_with_clients(&cert, &key, ca.as_deref(), client_auth)?;
                (Some(nosql_server::net::tls_acceptor(&cert, &key)?), Some(cql))
            }
            _ => (None, None),
        };
        let dedicated = args.nosql_cql_tls.is_some() || args.nosql_cql_shard_aware_tls.is_some();
        let mut cfg = NosqlConfig::local(home.join("nosql"));
        cfg.storage = storage_config(&args, home, storage);
        cfg.node_name = node.to_string();
        cfg.public_host = public_host.to_string();
        cfg.cql_port = args.nosql_cql.port();
        cfg.cql_shard_aware_port = Some(args.nosql_cql_shard_aware.port());
        cfg.alternator_port = Some(args.nosql_alternator.port());
        cfg.alternator_https_port = args.nosql_alternator_https.map(|a| a.port());
        cfg.alternator_domain = alternator_domain(&args);
        cfg.rest_port = Some(args.nosql_rest.port());
        // With dedicated TLS ports, the main ones (and the endpoints the API shows) are plain.
        cfg.tls = tls.is_some() && !dedicated;
        cfg.cql_shard_aware_port_ssl = args.nosql_cql_shard_aware_tls.map(|a| a.port());
        cfg.domain = args.nosql_domain.clone().filter(|d| !d.is_empty());
        cfg.single = args.nosql_single.clone().filter(|s| !s.is_empty());
        cfg.defaults.dialect = args.nosql_dialect.clone();
        cfg.defaults.datacenter = args.nosql_datacenter.clone();
        cfg.defaults.rack = args.nosql_rack.clone();
        cfg.single_cluster_name = args.nosql_cluster_name.clone();
        cfg.single_password = args.nosql_single_password.clone().filter(|s| !s.is_empty());
        if !args.nosql_single_settings.is_empty() {
            cfg.single_settings = Some(settings_of(&args.nosql_single_settings)?);
        }
        if let Some(json) = args.nosql_single_settings_json.as_deref().filter(|j| !j.trim().is_empty()) {
            let more: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(json).map_err(|e| anyhow::anyhow!("--nosql-single-settings-json: expected a JSON object: {e}"))?;
            cfg.single_settings.get_or_insert_with(Default::default).extend(more);
        }
        if let Some(w) = &args.nosql_alternator_write_isolation {
            anyhow::ensure!(WriteIsolation::parse(w).is_some(), "unknown Alternator write isolation {w:?} (always_use_lwt, only_rmw_uses_lwt, forbid_rmw or unsafe_rmw)");
        }
        let control = Control::new(cfg, pg_storage::Objects::new(storage)?).await?;
        info!(storage = %control.objects().description, single = ?control.single_id, "roll nosql started");
        if let (Some(id), Some(from)) = (control.single_id.clone(), args.nosql_from.as_deref().filter(|f| !f.is_empty())) {
            seed(&control, &id, from, args.nosql_from_token.as_deref(), &args.nosql_from_keyspaces).await?;
        }
        Ok(Nosql { control, args, tls, cql_tls })
    }

    pub fn router(&self) -> axum::Router {
        nosql_control::api::router(self.control.clone())
    }

    /// Run every listener until one fails.
    pub async fn serve(&self) -> anyhow::Result<()> {
        let a = &self.args;
        let dir: Arc<dyn Directory> = self.control.node.clone();
        let cql = bind(a.nosql_cql).await?;
        let shard_aware = bind(a.nosql_cql_shard_aware).await?;
        let rest = bind(a.nosql_rest).await?;
        let alternator = bind(a.nosql_alternator).await?;
        let alternator_https = match (a.nosql_alternator_https, &self.tls) {
            (Some(addr), Some(tls)) => Some((bind(addr).await?, tls.clone())),
            _ => None,
        };
        info!(cql = %a.nosql_cql, shard_aware = %a.nosql_cql_shard_aware, alternator = %a.nosql_alternator, alternator_https = ?a.nosql_alternator_https, rest = %a.nosql_rest, tls = self.tls.is_some(), "nosql listening");
        let dedicated = a.nosql_cql_tls.is_some() || a.nosql_cql_shard_aware_tls.is_some();
        let tls_opts = ListenOptions { tls: self.cql_tls.clone(), proxy_protocol: a.nosql_proxy_protocol, tls_optional: a.nosql_tls_optional, ..Default::default() };
        let opts = if dedicated { ListenOptions { proxy_protocol: a.nosql_proxy_protocol, ..Default::default() } } else { tls_opts.clone() };
        let cql_tls = match a.nosql_cql_tls {
            Some(addr) => Some(bind(addr).await?),
            None => None,
        };
        let shard_aware_tls = match a.nosql_cql_shard_aware_tls {
            Some(addr) => Some(bind(addr).await?),
            None => None,
        };
        let mut alt = nosql_server::alternator::AlternatorOptions {
            ttl_period: std::time::Duration::from_secs_f64(a.nosql_alternator_ttl_period.clamp(0.01, 1e9)),
            ..Default::default()
        };
        alt.config.local_nodes[0].address = self.control.cfg.public_host.clone();
        if let Some(w) = a.nosql_alternator_write_isolation.as_deref().and_then(WriteIsolation::parse) {
            alt.config.write_isolation = w;
        }
        let metrics = match a.nosql_metrics {
            Some(addr) => Some(bind(addr).await?),
            None => None,
        };
        if self.control.single_id.is_some() {
            self.announce_ready();
        }
        let metrics = async {
            match metrics {
                Some(l) => axum::serve(l, nosql_server::metrics::router(dir.clone(), Some(alt.clone()))).await.map_err(anyhow::Error::from),
                None => std::future::pending().await,
            }
        };
        let https = async {
            match alternator_https {
                Some((l, tls)) => nosql_server::alternator::serve_tls(l, tls, dir.clone(), alt.clone()).await,
                None => std::future::pending().await,
            }
        };
        let serve_on = |l: Option<TcpListener>, o: ListenOptions| {
            let dir = dir.clone();
            async move {
                match l {
                    Some(l) => nosql_server::net::serve(l, dir, o).await,
                    None => std::future::pending().await,
                }
            }
        };
        tokio::select! {
            r = serve_on(cql_tls, tls_opts.clone()) => r,
            r = serve_on(shard_aware_tls, ListenOptions { shard_aware: true, ..tls_opts.clone() }) => r,
            r = nosql_server::net::serve(cql, dir.clone(), opts.clone()) => r,
            r = nosql_server::net::serve(shard_aware, dir.clone(), ListenOptions { shard_aware: true, ..opts }) => r,
            r = nosql_server::alternator::serve(alternator, dir.clone(), alt.clone()) => r.map_err(Into::into),
            r = https => r.map_err(Into::into),
            r = nosql_server::rest::serve(rest, dir.clone()) => r,
            r = metrics => r,
        }
    }

    /// The lines the reference images log once they serve CQL, which Testcontainers and
    /// docker-compose health checks wait for: Cassandra's `Starting listening for CQL clients`
    /// and `Startup complete`, ScyllaDB's `init - serving` and `initialization completed.`
    fn announce_ready(&self) {
        let a = &self.args;
        let dedicated = a.nosql_cql_tls.is_some() || a.nosql_cql_shard_aware_tls.is_some();
        let enc = if self.tls.is_some() && !dedicated { "encrypted" } else { "unencrypted" };
        info!("Starting listening for CQL clients on {} ({enc}, non-shard-aware)", a.nosql_cql);
        info!("Starting listening for CQL clients on {} ({enc}, shard-aware)", a.nosql_cql_shard_aware);
        if let Some(addr) = a.nosql_cql_tls {
            info!("Starting listening for CQL clients on {addr} (encrypted, non-shard-aware)");
        }
        if let Some(addr) = a.nosql_cql_shard_aware_tls {
            info!("Starting listening for CQL clients on {addr} (encrypted, shard-aware)");
        }
        info!("init - serving");
        info!("Startup complete");
        info!("init - roll nosql {} initialization completed.", env!("CARGO_PKG_VERSION"));
    }

    /// Make every acknowledged write durable (commit log shipped, manifests written).
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.control.storage.shipper.flush().await?;
        self.control.storage.write_manifests(false).await?;
        Ok(())
    }
}

/// The Alternator host suffix: the flag, else `alternator.<base>` for a `nosql.<base>` domain.
fn alternator_domain(a: &NosqlArgs) -> Option<String> {
    if let Some(d) = a.nosql_alternator_domain.as_deref().filter(|d| !d.is_empty()) {
        return Some(d.to_string());
    }
    let d = a.nosql_domain.as_deref().filter(|d| !d.is_empty())?;
    Some(match d.strip_prefix("nosql.") {
        Some(base) => format!("alternator.{base}"),
        None => format!("alternator.{d}"),
    })
}

/// `KEY=VALUE` settings; values are JSON when they parse (`true`, `8`), strings otherwise.
fn settings_of(items: &[String]) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut m = serde_json::Map::new();
    for it in items.iter().map(|i| i.trim()).filter(|i| !i.is_empty()) {
        let (k, v) = it.split_once('=').ok_or_else(|| anyhow::anyhow!("--nosql-single-setting: expected KEY=VALUE, got {it:?}"))?;
        let v = v.trim();
        m.insert(k.trim().to_string(), serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.to_string())));
    }
    Ok(m)
}

/// Seeds the single-mode database once (`--nosql-from`): a `cql://` import of the source, run
/// to completion before any listener opens, so the first client sees the data.
async fn seed(control: &Arc<Control>, id: &str, from: &str, token: Option<&str>, keyspaces: &[String]) -> anyhow::Result<()> {
    if !control.imports(id).map_err(|e| anyhow::anyhow!("{e}"))?.is_empty() {
        info!(from, "single-mode database already seeded");
        return Ok(());
    }
    let source = source_url(from, token).await?;
    let req = nosql_control::import::ImportReq { from: source, keyspaces: keyspaces.iter().filter(|k| !k.is_empty()).cloned().collect(), every: None };
    let imp = control.start_import(id, req).map_err(|e| anyhow::anyhow!("--nosql-from {from}: {e}"))?;
    info!(from, import = imp.id, "seeding the single-mode database");
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let Some(i) = control.imports(id).map_err(|e| anyhow::anyhow!("{e}"))?.into_iter().find(|i| i.id == imp.id) else { continue };
        match i.state.as_str() {
            "queued" | "running" => continue,
            "failed" => anyhow::bail!("--nosql-from {from}: {}", i.error.unwrap_or_default()),
            _ => {
                info!(from, rows = i.rows, tables = i.tables_done, "single-mode database seeded");
                return Ok(());
            }
        }
    }
}

/// The `cql://` source of `--nosql-from`: given as is, or looked up from `<api url>/<branch>`
/// (`GET /v1/nosql/databases/<branch>`: its CQL endpoint, TLS server name and credentials).
async fn source_url(from: &str, token: Option<&str>) -> anyhow::Result<String> {
    if from.starts_with("cql://") {
        return Ok(from.to_string());
    }
    let (base, branch) = from.trim_end_matches('/').rsplit_once('/').ok_or_else(|| anyhow::anyhow!("--nosql-from {from}: expected <api url>/<branch> or cql://host:port"))?;
    anyhow::ensure!(base.starts_with("http://") || base.starts_with("https://"), "--nosql-from {from}: expected <api url>/<branch> or cql://host:port");
    let base = base.trim_end_matches("/v1/nosql/databases").trim_end_matches("/v1/nosql");
    let url = format!("{base}/v1/nosql/databases/{}", urlencoding::encode(branch));
    let mut req = reqwest::Client::new().get(&url);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| anyhow::anyhow!("--nosql-from: GET {url}: {e}"))?;
    let status = resp.status();
    let v: serde_json::Value = resp.json().await.map_err(|e| anyhow::anyhow!("--nosql-from: GET {url}: {status}: {e}"))?;
    anyhow::ensure!(status.is_success(), "--nosql-from: GET {url}: {status}: {}", v["error"].as_str().unwrap_or(""));
    let cql = v["endpoints"]["cql"].as_str().ok_or_else(|| anyhow::anyhow!("--nosql-from: {url} has no CQL endpoint"))?;
    let (host, port) = cql.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--nosql-from: bad CQL endpoint {cql:?}"))?;
    let host = v["server_name"].as_str().filter(|_| v["tls"] == true).unwrap_or(host);
    let user = v["credentials"]["username"].as_str().unwrap_or_default();
    let password = v["credentials"]["password"].as_str().unwrap_or_default();
    let tls = if v["tls"] == true { "?tls=1" } else { "" };
    Ok(format!("cql://{}:{}@{host}:{port}{tls}", urlencoding::encode(user), urlencoding::encode(password)))
}

async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        nosql: NosqlArgs,
    }

    fn args(a: &[&str]) -> NosqlArgs {
        Cli::try_parse_from(std::iter::once("rolld").chain(a.iter().copied())).unwrap().nosql
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512MiB"), Ok(512 << 20));
        assert_eq!(parse_size("2G"), Ok(2 << 30));
        assert_eq!(parse_size("64kb"), Ok(64 << 10));
        assert_eq!(parse_size(" 1024 "), Ok(1024));
        assert_eq!(parse_size("1TiB"), Ok(1 << 40));
        assert!(parse_size("lots").is_err());
        assert!(parse_size("5XB").unwrap_err().contains("bad size unit"));
        assert!(parse_size("99999999999999999999G").is_err());
    }

    #[test]
    fn budgets_from_flags() {
        let local = StorageConfig::Local(PathBuf::from("/tmp/x"));
        let s3 = StorageConfig::S3 { bucket: "b".into(), region: None, endpoint: None, prefix: None };
        let home = Path::new("/home/r");
        // Defaults: a multi-tenant node gives a database half the memtable budget, single
        // mode all of it; the disk cache only with S3.
        let c = storage_config(&args(&[]), home, &local);
        assert_eq!((c.memtable_budget_bytes, c.memtable_db_bytes, c.cache.memory_bytes), (512 << 20, 256 << 20, 256 << 20));
        assert_eq!((c.flush_concurrency, c.compaction_concurrency), (2, 2));
        assert!(c.cache.disk_dir.is_none());
        let c = storage_config(&args(&["--nosql-single", "dev"]), home, &s3);
        assert_eq!(c.memtable_db_bytes, 512 << 20);
        assert_eq!((c.cache.disk_dir.as_deref(), c.cache.disk_bytes), (Some(Path::new("/home/r/nosql/cache")), 4 << 30));
        let c = storage_config(
            &args(&[
                "--nosql-memtable-budget=1GiB",
                "--nosql-memtable-db-cap=128MiB",
                "--nosql-cache-memory=64MiB",
                "--nosql-cache-disk=0",
                "--nosql-flush-concurrency=4",
                "--nosql-compaction-concurrency=3",
            ]),
            home,
            &s3,
        );
        assert_eq!((c.memtable_budget_bytes, c.memtable_db_bytes, c.cache.memory_bytes), (1 << 30, 128 << 20, 64 << 20));
        assert_eq!((c.flush_concurrency, c.compaction_concurrency), (4, 3));
        assert!(c.cache.disk_dir.is_none());
        assert!(Cli::try_parse_from(["rolld", "--nosql-memtable-budget", "big"]).is_err());
    }
}
