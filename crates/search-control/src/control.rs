//! The roll search control plane: clusters and their settings, credentials, branches (now or
//! at a point in time), unloading, the request console, metering and the cost estimate.
//!
//! Serving is done by a [`search_server::Node`] built here: its [`Catalog`] is this control
//! plane's store ([`SqlCatalog`]), its metadata and shards live in a [`search_store::Store`] on
//! the node's object storage (documents are durable: WAL group commits, segments, manifests),
//! and its lifecycle hooks feed the [`Meter`]. A cluster's name is its identity on the node (the
//! Basic auth user, the host label under `--search-domain`, `cluster_name`, its key in object
//! storage), so names are immutable and limited to DNS labels; the API also accepts the
//! generated id.
//!
//! The password is returned when a cluster is created and when it is rotated, and never
//! otherwise. The node compares presented secrets in constant time against the catalog's.
//!
//! Changing a cluster (password, settings, delete) takes effect on the node before the call
//! returns, also when a request is waking the cluster at that moment ([`crate::wake`]): every
//! change is serialized with applying it to the loaded instance (`specs`), and waits for wakes
//! that may have read the old description. Limits and credentials apply live (the node resizes
//! a cluster's permits in place); what the engine fixes when a cluster opens (its dialect and
//! the `destructive_requires_name` default) changes by a reload, done as soon as no request
//! runs.
//!
//! **On a fleet** ([`Control::join`], `crate::cluster`): every node shares the catalog (the
//! system database), each cluster is served by the holder of its roll-cluster lease, calls about
//! a cluster run on its owner, and writes are acknowledged once in object storage. Garbage
//! collection runs on one node at a time (`singleton:search-gc`).

use crate::meter::{Meter, MeterHooks, engine_figures, index_figures, now_ms};
use crate::pricing::{Cost, Pricing};
use crate::store::{SearchCluster, Store, UsageTotals};
pub use crate::wake::SqlCatalog;
use crate::wake::{ControlHooks, GuardedStores, LOAD_WAIT, Wakes};
use base64::Engine;
use bytes::Bytes;
use parking_lot::Mutex;
use search_es::{Dialect, EsRequest, EsResponse};
use search_server::node::Loaded;
use search_server::{ClusterSpec, Credential, Node, NodeConfig, ObjectStores};
use search_store::{Objects, StoreConfig, WalConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

#[derive(thiserror::Error, Debug)]
pub enum ControlError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    /// A request body over the limit (HTTP 413).
    #[error("{0}")]
    TooLarge(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ControlError>;

#[derive(Clone, Debug)]
pub struct SearchConfig {
    /// `search.db` and the local cache (`cache/`).
    pub home: PathBuf,
    /// This node's id in object storage (names its WAL objects) and `GET /` `name`.
    pub node_name: String,
    pub region: String,
    /// Host and port written into cluster URLs.
    pub public_host: String,
    pub http_port: u16,
    pub tls: bool,
    /// Where clients reach the clusters when that isn't this node's listener (a load balancer
    /// in front of a fleet): its scheme, host and port go into
    /// cluster URLs instead of `public_host`, `http_port` and `tls`.
    pub public_url: Option<String>,
    /// With a domain, clusters are reached at `<name>.<domain>` (TLS SNI or `Host` routing).
    pub domain: Option<String>,
    /// Default idle time before a cluster unloads (a cluster's own `idle_timeout_s` wins).
    pub idle_unload: Duration,
    /// Local disk cache budget for segments.
    pub cache_bytes: u64,
    /// Acknowledge writes once the WAL is in object storage (ephemeral disks; always on in a
    /// fleet).
    pub sync_objects: bool,
    /// Point-in-time branching window: commit points and cluster states stay this long.
    pub history_retention: Duration,
    /// Garbage collection of unreferenced segments and WAL files.
    pub gc_interval: Option<Duration>,
    /// How often the node wakes sleeping clusters whose background jobs came due
    /// ([`Control::start_job_wakes`]; `None`: never).
    pub job_wake_interval: Option<Duration>,
    /// How often loaded clusters are metered.
    pub metering_interval: Duration,
    /// Publish Elasticsearch's test cluster features, so the YAML REST suites can run against
    /// clusters made through the API (`search-yaml run --cluster-hook`). Test deployments only.
    pub test_features: bool,
    /// How often a loaded cluster's figures (docs, stored bytes) are re-read from storage.
    pub stats_interval: Duration,
    /// Memory all loaded clusters may hold together (`None`: no node budget).
    pub memory_budget: Option<u64>,
    /// The catalog in a Postgres database every node of a fleet shares (rolld's system
    /// database; `None`: `search.db` under `home`).
    pub catalog: Option<String>,
    /// Connections the catalog's Postgres pool may open (rolld's share of the system database).
    pub catalog_connections: usize,
    pub defaults: Defaults,
    pub pricing: Pricing,
}

#[derive(Clone, Debug, Serialize)]
pub struct Defaults {
    pub flavor: String,
    pub idle_timeout_s: i64,
    pub max_in_flight: i64,
    pub max_body_mb: i64,
    pub destructive_requires_name: bool,
}

impl SearchConfig {
    pub fn local(home: PathBuf) -> Self {
        Self {
            home,
            node_name: "local".into(),
            region: "local".into(),
            public_host: "127.0.0.1".into(),
            http_port: 59200,
            tls: false,
            public_url: None,
            domain: None,
            idle_unload: Duration::from_secs(300),
            cache_bytes: 10 << 30,
            sync_objects: false,
            history_retention: Duration::from_secs(86_400),
            gc_interval: Some(Duration::from_secs(3600)),
            job_wake_interval: Some(Duration::from_secs(15)),
            metering_interval: Duration::from_secs(60),
            test_features: false,
            stats_interval: Duration::from_secs(600),
            memory_budget: None,
            catalog: None,
            catalog_connections: 8,
            defaults: Defaults {
                flavor: "elasticsearch".into(),
                idle_timeout_s: 300,
                max_in_flight: 64,
                max_body_mb: 100,
                // Elasticsearch's default.
                destructive_requires_name: true,
            },
            pricing: Pricing::default(),
        }
    }
}

/// Settings a client may set on create and change later.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ClusterOptions {
    /// `elasticsearch` or `opensearch`.
    pub flavor: Option<String>,
    /// Seconds without requests before unloading; 0 keeps the cluster loaded.
    pub idle_timeout_s: Option<i64>,
    pub max_in_flight: Option<i64>,
    pub max_body_mb: Option<i64>,
    pub destructive_requires_name: Option<bool>,
}

/// One REST request through the console.
#[derive(Clone, Debug, Deserialize)]
pub struct ConsoleRequest {
    pub method: String,
    /// `/index/_search?q=…`
    pub path: String,
    /// A JSON value is sent as JSON, byte for byte as the client wrote it (key order and
    /// number spelling matter: Elasticsearch stores `_source` exactly as sent); a string is sent
    /// as is (NDJSON for `_bulk`).
    #[serde(default)]
    pub body: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConsoleResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// The response body as the cluster sent it (UTF-8, lossy), so JSON keeps Elasticsearch's
    /// key order; empty for HEAD.
    pub body: String,
    /// The body is JSON (`Content-Type` says so).
    pub json: bool,
    pub took_ms: u64,
    /// The body was longer than [`CONSOLE_MAX_BODY`] and was cut (returned as text).
    pub truncated: bool,
}

/// Longest response body the console returns.
pub const CONSOLE_MAX_BODY: usize = 4 << 20;

/// What an "always on" idle timeout becomes on the node (a century).
const ALWAYS_ON: Duration = Duration::from_secs(100 * 365 * 86_400);

const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";

fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

pub fn new_id() -> String {
    format!("srch_{}", random_string(12))
}

pub fn now() -> i64 {
    now_ms() / 1000
}

/// Cluster names are DNS labels: they appear in host names (`<name>.<domain>`), Basic auth user
/// names and object keys. No `_` (Bearer tokens split at the last one) and no `@` or `:`.
pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn dialect(flavor: &str) -> Option<Dialect> {
    match flavor {
        "elasticsearch" => Some(Dialect::Elasticsearch),
        "opensearch" => Some(Dialect::OpenSearch),
        _ => None,
    }
}

/// What the node needs to serve a cluster.
pub fn spec_for(c: &SearchCluster) -> ClusterSpec {
    let mut spec = ClusterSpec::new(c.name.clone());
    spec.dialect = dialect(&c.flavor).unwrap_or(Dialect::Elasticsearch);
    spec.credentials = vec![Credential::password(c.password.clone())];
    spec.idle_unload = Some(if c.idle_timeout_s == 0 { ALWAYS_ON } else { Duration::from_secs(c.idle_timeout_s.max(1) as u64) });
    spec.max_in_flight = c.max_in_flight.max(1) as usize;
    spec.max_body_bytes = Some((c.max_body_mb.max(1) as usize) << 20);
    spec.destructive_requires_name = c.destructive_requires_name;
    spec
}

/// `target` as a loaded instance can take it now, and whether it needs a reload: the dialect
/// and the `destructive_requires_name` default are fixed when the engine opens the cluster.
/// Everything else (credentials, limits, permits) the node applies in place.
pub(crate) fn live_spec(mut target: ClusterSpec, l: &Loaded) -> (ClusterSpec, bool) {
    let reload = target.dialect != l.cluster.dialect || target.destructive_requires_name != l.cluster.config.destructive_requires_name;
    target.dialect = l.cluster.dialect;
    target.destructive_requires_name = l.cluster.config.destructive_requires_name;
    (target, reload)
}

/// Where clients reach cluster `name`: the front (`public_url`) when there is one, else this
/// node's listener; with a domain, the cluster's own host name under it.
fn connection_of(cfg: &SearchConfig, name: &str) -> Value {
    let front = cfg.public_url.as_deref().and_then(|u| reqwest::Url::parse(u).ok()).filter(|u| u.host_str().is_some());
    let (scheme, base_host, port) = match &front {
        Some(u) => (u.scheme(), u.host_str().unwrap_or_default().to_string(), u.port_or_known_default().unwrap_or(443)),
        None => (if cfg.tls { "https" } else { "http" }, cfg.public_host.clone(), cfg.http_port),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let port_part = if port == default_port { String::new() } else { format!(":{port}") };
    let host = match &cfg.domain {
        Some(d) => format!("{name}.{d}"),
        None => base_host,
    };
    json!({ "url": format!("{scheme}://{host}{port_part}"), "host": host, "port": port, "username": name })
}

/// What a link's credential may do: all of the indices' data and their templates, pipelines
/// and lifecycle policies, as an app uses a cluster; no security (users, roles, keys).
fn link_role() -> Value {
    json!({
        "cluster": ["monitor", "manage_index_templates", "manage_ingest_pipelines", "manage_ilm"],
        "indices": [{ "names": ["*"], "privileges": ["all"] }]
    })
}

/// [`link_role`] in OpenSearch Security's terms (the opensearch dialect): the same action
/// groups and actions (ISM for ILM), no `restapi:` permission.
fn os_link_role() -> Value {
    json!({
        "description": "roll: an app's link (data, templates, pipelines, ISM; no security)",
        "cluster_permissions": [
            "cluster_monitor",
            "cluster_composite_ops",
            "indices:data/read/scroll*",
            "cluster_manage_index_templates",
            "cluster_manage_pipelines",
            "cluster:admin/opendistro/ism/*"
        ],
        "index_permissions": [{ "index_patterns": ["*"], "allowed_actions": ["indices_all"] }]
    })
}

/// A link's credential ([`Control::mint_link`]): its variables (unprefixed) and what revoking
/// it takes.
#[derive(Clone, Debug)]
pub struct LinkCredential {
    pub vars: Vec<(String, String)>,
    pub kept: String,
}

/// `spec` with a secret nobody knows.
fn locked_spec(spec: &ClusterSpec) -> ClusterSpec {
    let mut s = spec.clone();
    s.credentials = vec![Credential::password(random_string(48))];
    s
}

#[derive(Serialize)]
pub struct Usage {
    pub cluster_id: String,
    pub days: i64,
    /// Hours of the window the cluster existed (what always-on services would bill).
    pub hours: f64,
    #[serde(flatten)]
    pub totals: UsageTotals,
    pub stored_bytes: i64,
    pub cost: Cost,
}


pub struct Control {
    pub cfg: SearchConfig,
    pub store: Arc<Store>,
    pub node: Arc<Node>,
    /// Segments, WAL and cluster metadata on object storage.
    pub data: search_store::Store,
    pub meter: Arc<Meter>,
    /// Wakes in progress and clusters being deleted ([`crate::wake`]).
    wakes: Arc<Wakes>,
    /// Serializes changes of a cluster's description with applying it to the node, so the node
    /// never ends up with an older one than the catalog's.
    specs: tokio::sync::Mutex<()>,
    /// Loaded clusters whose new settings need a reload (dialect, the
    /// `destructive_requires_name` default): unloaded once no request runs.
    reloads: Mutex<HashSet<String>>,
    reloader: AtomicBool,
    me: OnceLock<Weak<Control>>,
    /// When each loaded cluster's figures were last read from storage (ms).
    refreshed: Mutex<HashMap<String, i64>>,
    /// The roll-cluster fleet this node serves clusters in ([`Control::join`]).
    leases: OnceLock<Arc<crate::cluster::ClusterLeases>>,
}

impl Control {
    pub async fn new(cfg: SearchConfig, objects: Objects) -> anyhow::Result<Arc<Self>> {
        Self::open(cfg, objects, None).await
    }

    /// [`Control::new`] with the WAL in its own object store (S3 Express One Zone).
    pub async fn open(cfg: SearchConfig, objects: Objects, wal_objects: Option<Objects>) -> anyhow::Result<Arc<Self>> {
        Self::open_with(cfg, objects, wal_objects, None).await
    }

    /// [`Control::new`] on a catalog the caller made (nodes of a fleet in one process share one).
    pub async fn with_catalog(cfg: SearchConfig, objects: Objects, catalog: Arc<Store>) -> anyhow::Result<Arc<Self>> {
        Self::open_with(cfg, objects, None, Some(catalog)).await
    }

    async fn open_with(cfg: SearchConfig, objects: Objects, wal_objects: Option<Objects>, catalog: Option<Arc<Store>>) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.home)?;
        let store = match (catalog, &cfg.catalog) {
            (Some(c), _) => c,
            (None, Some(url)) => Arc::new(Store::postgres(url, code_sql::PgOptions { max_connections: cfg.catalog_connections.max(1), ..Default::default() })?),
            (None, None) => Arc::new(Store::open(&cfg.home.join("search.db"))?),
        };
        let scfg = StoreConfig {
            node: cfg.node_name.clone(),
            wal: WalConfig { sync_objects: cfg.sync_objects, ..WalConfig::default() },
            cache_bytes: cfg.cache_bytes,
            manifest_retain_ms: cfg.history_retention.as_millis() as u64,
            // Segments fetched from object storage are checked whole before the cache keeps them.
            verify: Some(search_server::persist::segment_verifier()),
            ..StoreConfig::default()
        };
        let data = match wal_objects {
            Some(w) => search_store::Store::open_with_wal_objects(objects, w, cfg.home.join("cache"), scfg).await?,
            None => search_store::Store::open(objects, cfg.home.join("cache"), scfg).await?,
        };
        let meter = Meter::new(store.clone());
        let wakes = Arc::new(Wakes::default());
        let hooks = Arc::new(ControlHooks { meter: MeterHooks(meter.clone()), wakes: wakes.clone(), control: OnceLock::new() });
        let node = Node::with_hooks(
            Arc::new(SqlCatalog { store: store.clone(), wakes: wakes.clone() }),
            Arc::new(GuardedStores { inner: ObjectStores::new(data.clone()), store: store.clone(), wakes: wakes.clone() }),
            NodeConfig {
                node_name: cfg.node_name.clone(),
                domain: cfg.domain.clone(),
                idle_unload: Some(cfg.idle_unload),
                test_features: cfg.test_features,
                memory_budget: cfg.memory_budget,
                ..NodeConfig::default()
            },
            hooks.clone(),
        );
        let control = Arc::new(Self {
            cfg,
            store,
            node,
            data,
            meter,
            wakes,
            specs: tokio::sync::Mutex::new(()),
            reloads: Mutex::new(HashSet::new()),
            reloader: AtomicBool::new(false),
            me: OnceLock::new(),
            refreshed: Mutex::new(HashMap::new()),
            leases: OnceLock::new(),
        });
        // Nothing can load before the node is handed out, so the hook always finds the control.
        let _ = hooks.control.set(Arc::downgrade(&control));
        let _ = control.me.set(Arc::downgrade(&control));
        control.start_metering();
        control.start_gc();
        control.start_always_on();
        control.start_job_wakes();
        Ok(control)
    }

    /// Take part in a roll-cluster fleet (docs/search/architecture.md, "On the fleet"):
    /// clusters are served by the holders of their leases from now on. Call once, before
    /// serving; the control must have been opened with `sync_objects` (another node replays
    /// this one's WAL after a takeover, so a write is acknowledged once it is in object storage).
    pub fn join(self: &Arc<Self>, cluster: Arc<roll_cluster::Cluster>) -> anyhow::Result<()> {
        anyhow::ensure!(self.cfg.sync_objects, "search on a fleet needs sync_objects: acknowledged writes must be in object storage");
        let leases = crate::cluster::ClusterLeases::new(cluster.clone());
        let node = Arc::downgrade(&self.node);
        leases.on_ending(Arc::new(move |name: String| {
            let node = node.clone();
            Box::pin(async move {
                match node.upgrade() {
                    Some(n) => n.unload(&name).await,
                    None => true,
                }
            })
        }));
        if self.leases.set(leases.clone()).is_err() {
            return Ok(());
        }
        self.node.set_leases(leases);
        tokio::spawn(crate::cluster::release_idle(Arc::downgrade(self)));
        tracing::info!(node = cluster.node_id(), backend = cluster.backend_name(), "roll search: clusters are served by their owners");
        Ok(())
    }

    /// On a fleet, records in the catalog that `name` is loaded here (or no longer is), so lists
    /// show where without reading leases. Best effort: a failure only makes lists less exact.
    pub(crate) async fn note_loaded(&self, name: &str, loaded: bool) {
        let Some(l) = self.cluster_leases() else { return };
        let me = l.cluster().node_id();
        if let Err(e) = self.store.set_loaded_on(name, loaded.then_some(me), me).await {
            tracing::debug!(cluster = name, error = %e, "recording where a search cluster is loaded failed");
        }
    }

    /// Records when a sleeping cluster must wake for its next background job ([`Self::start_job_wakes`]).
    pub(crate) async fn note_wake_at(&self, name: &str, at: Option<i64>) {
        if let Err(e) = self.store.set_wake_at(name, at).await {
            tracing::warn!(cluster = name, error = %e, "recording a search cluster's next job failed");
        }
    }

    /// Wakes sleeping clusters whose background jobs (a started transform's trigger, an ISM
    /// job) came due, as Elasticsearch runs them on schedule: every `job_wake_interval`, the
    /// clusters the catalog says are due load (on a fleet, on the node their lease is placed
    /// on), run what is due, and sleep again after their idle timeout, billed as loaded time.
    /// Unloading records the next due time; a cluster with no jobs is never woken.
    fn start_job_wakes(&self) {
        let (Some(me), Some(every)) = (self.me.get().cloned(), self.cfg.job_wake_interval) else { return };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = me.upgrade() else { return };
                match c.store.due_wakes(crate::meter::now_ms()).await {
                    Ok(due) => {
                        for cluster in &due {
                            c.wake_for_jobs(cluster).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "listing search clusters with due jobs failed"),
                }
            }
        });
    }

    async fn wake_for_jobs(&self, c: &SearchCluster) {
        if self.node.get_loaded(&c.name).is_some() || self.wakes.is_deleting(&c.name) {
            return;
        }
        if let Some(l) = self.cluster_leases()
            && l.cluster().preferred(&crate::cluster::key(&c.name)).is_none_or(|n| n.id != l.cluster().node_id())
        {
            return;
        }
        if let Err(e) = self.node.open(spec_for(c)).await {
            // Tried again a minute on, not every pass.
            tracing::info!(cluster = c.name, error = %e, "waking a search cluster for its due jobs failed");
            self.note_wake_at(&c.name, Some(crate::meter::now_ms() + 60_000)).await;
        }
    }

    /// The fleet's leases, when this node is in one ([`Control::join`]).
    pub fn cluster_leases(&self) -> Option<&Arc<crate::cluster::ClusterLeases>> {
        self.leases.get()
    }

    pub fn objects(&self) -> &Objects {
        self.data.objects()
    }

    /// Stops the node for good (SIGTERM): unloads every cluster (flushing it and handing its
    /// lease back, so another node takes over at once), accounts usage and makes the WAL
    /// durable. On a fleet, rolld's drain has handed the clusters over before this runs.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown_within(Duration::from_secs(60)).await
    }

    pub async fn shutdown_within(&self, timeout: Duration) -> anyhow::Result<()> {
        let r = self.node.shutdown(timeout).await;
        if let Err(e) = self.sample().await {
            tracing::warn!(error = %e, "search metering failed");
        }
        r
    }

    // --- clusters -------------------------------------------------------------------------

    /// The id (`srch_…`) of the cluster `id_or_name` names, if it exists: one indexed catalog
    /// read, for resolvers that key rules by id (roll's access catalogue).
    pub async fn resolve_id(&self, id_or_name: &str) -> Option<String> {
        self.get(id_or_name).await.ok().map(|c| c.id)
    }

    pub async fn get(&self, id_or_name: &str) -> Result<SearchCluster> {
        if let Some(c) = self.store.get(id_or_name).await? {
            return Ok(c);
        }
        self.store.by_name(id_or_name).await?.ok_or_else(|| ControlError::NotFound(format!("no search cluster {id_or_name}")))
    }

    pub async fn list(&self) -> Result<Vec<SearchCluster>> {
        Ok(self.store.list().await?)
    }

    fn check(&self, c: &SearchCluster) -> Result<()> {
        if !valid_name(&c.name) {
            return Err(ControlError::Invalid("name: 1-63 lowercase letters, digits or '-', starting with a letter and not ending with '-'".into()));
        }
        if dialect(&c.flavor).is_none() {
            return Err(ControlError::Invalid(format!("unknown flavor {}: use elasticsearch or opensearch", c.flavor)));
        }
        if c.idle_timeout_s < 0 {
            return Err(ControlError::Invalid("idle_timeout_s can't be negative".into()));
        }
        if !(1..=4096).contains(&c.max_in_flight) {
            return Err(ControlError::Invalid("max_in_flight must be between 1 and 4096".into()));
        }
        // Elasticsearch caps http.max_content_length at 2 GB (Integer.MAX_VALUE bytes).
        if !(1..=2047).contains(&c.max_body_mb) {
            return Err(ControlError::Invalid("max_body_mb must be between 1 and 2047".into()));
        }
        Ok(())
    }

    fn apply(mut c: SearchCluster, o: ClusterOptions) -> SearchCluster {
        if let Some(v) = o.flavor {
            c.flavor = v.to_ascii_lowercase();
        }
        if let Some(v) = o.idle_timeout_s {
            c.idle_timeout_s = v;
        }
        if let Some(v) = o.max_in_flight {
            c.max_in_flight = v;
        }
        if let Some(v) = o.max_body_mb {
            c.max_body_mb = v;
        }
        if let Some(v) = o.destructive_requires_name {
            c.destructive_requires_name = v;
        }
        c
    }

    /// Fails when the name is taken, by a cluster or by data left in object storage.
    async fn check_free(&self, name: &str) -> Result<()> {
        if self.store.by_name(name).await?.is_some() || self.wakes.is_deleting(name) {
            return Err(ControlError::Conflict(format!("a search cluster named {name} already exists")));
        }
        if self.data.cluster_exists(name).await? {
            return Err(ControlError::Conflict(format!("object storage already holds data of a cluster named {name}")));
        }
        Ok(())
    }

    async fn insert(&self, c: &SearchCluster) -> Result<()> {
        if self.store.insert(c).await? {
            Ok(())
        } else {
            Err(ControlError::Conflict(format!("a search cluster named {} already exists", c.name)))
        }
    }

    fn fresh(&self, name: &str) -> SearchCluster {
        let d = &self.cfg.defaults;
        SearchCluster {
            id: new_id(),
            name: name.to_string(),
            region: self.cfg.region.clone(),
            flavor: d.flavor.clone(),
            password: random_string(32),
            idle_timeout_s: d.idle_timeout_s,
            max_in_flight: d.max_in_flight,
            max_body_mb: d.max_body_mb,
            destructive_requires_name: d.destructive_requires_name,
            parent_id: None,
            parent_at: None,
            created_at: now(),
            password_rotated_at: None,
            indices: 0,
            docs: 0,
            stored_bytes: 0,
            own_bytes: 0,
            loaded_on: None,
        }
    }

    /// A new, empty cluster. Nothing is written to object storage until it is first used.
    pub async fn create(&self, name: &str, o: ClusterOptions) -> Result<SearchCluster> {
        let c = Self::apply(self.fresh(name), o);
        self.check(&c)?;
        self.check_free(name).await?;
        self.insert(&c).await?;
        self.store.set_stats(&c.id, c.created_at, 0, 0, 0).await?;
        self.open_always_on(&c).await;
        Ok(c)
    }

    /// Loads a cluster that never unloads (`idle_timeout_s` 0) when it isn't loaded anywhere:
    /// it is up from the moment it exists (or the setting changes, or a node starts), not from
    /// its first request. A failure leaves it to wake with that request.
    async fn open_always_on(&self, c: &SearchCluster) {
        if c.idle_timeout_s != 0 || self.node.get_loaded(&c.name).is_some() || self.wakes.is_deleting(&c.name) {
            return;
        }
        // On a fleet, the node it's placed on loads it (taking its lease).
        if let Some(l) = self.cluster_leases()
            && l.cluster().preferred(&crate::cluster::key(&c.name)).is_none_or(|n| n.id != l.cluster().node_id())
        {
            return;
        }
        if let Err(e) = self.node.open(spec_for(c)).await {
            tracing::info!(cluster = c.name, error = %e, "loading an always-on cluster failed; its first request wakes it");
        }
    }

    /// [`Self::open_always_on`] for every always-on cluster of the catalog, in the background: when a
    /// node starts, and on a fleet again every few lease periods, so the clusters a stopped node
    /// held come back up on the nodes they're placed on now.
    fn start_always_on(&self) {
        let Some(me) = self.me.get().cloned() else { return };
        tokio::spawn(async move {
            loop {
                let Some(c) = me.upgrade() else { return };
                let every = c.cluster_leases().map(|l| l.cluster().config().lease_ttl.saturating_mul(3));
                match c.store.always_on().await {
                    Ok(all) => {
                        for cluster in &all {
                            c.open_always_on(cluster).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "listing always-on search clusters failed"),
                }
                drop(c);
                let Some(every) = every else { return };
                tokio::time::sleep(every).await;
            }
        });
    }

    /// A new cluster with `parent`'s indices, as they are now or as they were at `at` (Unix
    /// seconds, within the history window). Instant: the branch's commit points name the
    /// parent's segments, nothing is copied. Settings come from the parent unless given. In a
    /// fleet, call it on the node holding the parent (the API forwards there): a loaded parent
    /// is committed first, so the branch sees every acknowledged write.
    pub async fn branch(&self, parent: &str, name: &str, at: Option<i64>, o: ClusterOptions) -> Result<SearchCluster> {
        let p = self.get(parent).await?;
        let now = now();
        if let Some(t) = at {
            self.check_window(&p, t, now)?;
        }
        let fresh = self.fresh(name);
        // Settings and last-known figures from the parent; identity and secret of its own.
        let c = Self::apply(
            SearchCluster {
                id: fresh.id,
                name: fresh.name,
                region: fresh.region,
                password: fresh.password,
                created_at: fresh.created_at,
                password_rotated_at: None,
                parent_id: Some(p.id.clone()),
                parent_at: Some(at.unwrap_or(now).min(now)),
                ..p.clone()
            },
            o,
        );
        self.check(&c)?;
        self.check_free(name).await?;
        // A parent that was never written to has nothing to copy: the branch starts empty too.
        if self.data.cluster_exists(&p.name).await? {
            let r = match at {
                None => self.node.branch(&p.name, &c.name).await,
                Some(t) => self.data.branch(&p.name, &c.name, Some((t.max(0) as u64) * 1000)).await,
            };
            if let Err(e) = r {
                let msg = e.to_string();
                // Inside the window every state is retained, so no state at or before the
                // point means the parent was still empty then.
                if !(at.is_some() && msg.contains("has no retained state at or before")) {
                    return Err(if msg.contains("already exists") { ControlError::Conflict(msg) } else { ControlError::Internal(e) });
                }
            }
        }
        if let Err(e) = self.insert(&c).await {
            // Lost a race for the name: don't leave the branch's objects behind.
            if let Err(e2) = self.data.delete_cluster(&c.name).await {
                tracing::warn!(cluster = c.name, error = %e2, "removing an orphaned branch failed");
            }
            return Err(e);
        }
        self.store.set_stats(&c.id, now, c.indices, c.docs, c.stored_bytes).await?;
        // The parent's recorded figures may be a metering interval old: read the branch's own
        // from its commit points now, so it shows what it holds at once.
        let mut c = c;
        match self.refresh_stats(&c.name, now_ms()).await {
            Ok(()) => {
                if let Ok(Some(fresh)) = self.store.get(&c.id).await {
                    (c.indices, c.docs, c.stored_bytes) = (fresh.indices, fresh.docs, fresh.stored_bytes);
                }
            }
            Err(e) => {
                tracing::warn!(cluster = c.name, error = %e, "reading a new branch's figures failed");
                self.meter.mark_dirty(&c.name);
            }
        }
        Ok(c)
    }

    fn check_window(&self, p: &SearchCluster, at: i64, now: i64) -> Result<()> {
        if at > now + 1 {
            return Err(ControlError::Invalid("that point in time is in the future".into()));
        }
        let window = self.cfg.history_retention.as_secs() as i64;
        if window == 0 {
            return Err(ControlError::Invalid("point-in-time branches are disabled on this node (no history is retained)".into()));
        }
        let oldest = (now - window).max(p.created_at);
        if at < oldest {
            return Err(ControlError::Invalid(format!("outside the history window (earliest is {oldest})")));
        }
        Ok(())
    }

    /// Changes settings. Credentials, limits (`max_in_flight` included) and the idle timeout
    /// apply at once, also to a cluster that is waking up. A new flavor (dialect) or
    /// `destructive_requires_name` default needs a reload: the cluster is unloaded now if no
    /// request runs, or as soon as none does; until then it keeps its current ones.
    pub async fn update(&self, id: &str, o: ClusterOptions) -> Result<SearchCluster> {
        let c = {
            let _g = self.specs.lock().await;
            let c = Self::apply(self.get(id).await?, o);
            self.check(&c)?;
            self.store.update(&c).await?;
            self.apply_spec(&c.name).await;
            c
        };
        self.settle(&c.name).await;
        if self.reloads.lock().contains(&c.name) && self.try_reload(&c.name).await {
            self.reloads.lock().remove(&c.name);
        }
        self.open_always_on(&c).await;
        Ok(c)
    }

    /// A new password. Requests with the old one fail from now on, loaded, waking or not (on a
    /// fleet this runs on the cluster's owner).
    pub async fn rotate_password(&self, id: &str) -> Result<SearchCluster> {
        let c = {
            let _g = self.specs.lock().await;
            let mut c = self.get(id).await?;
            c.password = random_string(32);
            c.password_rotated_at = Some(now());
            self.store.update(&c).await?;
            self.apply_spec(&c.name).await;
            c
        };
        self.settle(&c.name).await;
        Ok(c)
    }

    /// Applies the catalog's description of `name` to its loaded instance, if any. Call with
    /// `specs` held (or through [`Self::apply_spec_now`]).
    ///
    /// What a reload would change stays as loaded (the dialect and the
    /// `destructive_requires_name` default are fixed when the cluster opens), so node-level
    /// answers keep matching the cluster; a reload is scheduled instead. A cluster that is
    /// being deleted, or is gone, gets an unguessable secret, and a gone one is evicted.
    async fn apply_spec(&self, name: &str) {
        let Some(l) = self.node.get_loaded(name) else {
            // A load in progress past its own catalog re-read takes this as it publishes (the
            // node parks it); with no load under way the node ignores it.
            if self.wakes.is_loading(name)
                && !self.wakes.is_deleting(name)
                && let Ok(Some(row)) = self.store.by_name(name).await
            {
                self.node.update_spec(spec_for(&row));
            }
            return;
        };
        let row = if self.wakes.is_deleting(name) {
            None
        } else {
            match self.store.by_name(name).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(cluster = name, error = %e, "reading a search cluster failed");
                    return;
                }
            }
        };
        match row {
            Some(row) => {
                let (spec, reload) = live_spec(spec_for(&row), &l);
                self.node.update_spec(spec);
                if reload {
                    self.schedule_reload(name);
                }
            }
            None => {
                self.node.update_spec(locked_spec(&l.spec()));
                if !self.wakes.is_deleting(name) {
                    self.schedule_reload(name);
                }
            }
        }
    }

    async fn apply_spec_now(&self, name: &str) {
        let _g = self.specs.lock().await;
        self.apply_spec(name).await;
    }

    /// Runs when a cluster has loaded (from the node's load hook): its instance was built from
    /// the description the waking request looked up, which a change may have overtaken.
    pub(crate) async fn reconcile(self: Arc<Self>, name: String, token: Option<u64>) {
        // The node publishes the instance right after the hook; give up if it unloads first.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut spins = 0u32;
        while self.node.get_loaded(&name).is_none() && self.meter.is_loaded(&name) && Instant::now() < deadline {
            if spins < 64 {
                tokio::task::yield_now().await;
            } else {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            spins += 1;
        }
        self.apply_spec_now(&name).await;
        if let Some(t) = token {
            self.wakes.finish(&name, t);
        }
    }

    /// Waits until no wake that may have read `name`'s previous description is still on its
    /// way (see [`crate::wake`]), then applies the current one.
    async fn settle(&self, name: &str) {
        let start = Instant::now();
        loop {
            let waiting = (self.wakes.is_loading(name) && start.elapsed() < LOAD_WAIT)
                || (self.wakes.looked_up_near(name, start) && start.elapsed() < crate::wake::LOOKUP_GRACE);
            if !waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.apply_spec_now(name).await;
    }

    fn schedule_reload(&self, name: &str) {
        if !self.reloads.lock().insert(name.to_string()) || self.reloader.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(me) = self.me.get().cloned() else { return };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let Some(c) = me.upgrade() else { return };
                let names: Vec<String> = c.reloads.lock().iter().cloned().collect();
                for name in names {
                    if c.try_reload(&name).await {
                        c.reloads.lock().remove(&name);
                    }
                }
                let reloads = c.reloads.lock();
                if reloads.is_empty() {
                    c.reloader.store(false, Ordering::SeqCst);
                    return;
                }
            }
        });
    }

    /// One attempt at a scheduled reload; `true` when nothing is left to do.
    async fn try_reload(&self, name: &str) -> bool {
        if self.wakes.is_deleting(name) {
            // The delete unloads it.
            return true;
        }
        let Some(l) = self.node.get_loaded(name) else {
            // Asleep: the next wake reads the catalog (a wake in progress is reconciled).
            return !self.wakes.is_loading(name);
        };
        let row = match self.store.by_name(name).await {
            Ok(r) => r,
            Err(_) => return false,
        };
        let needed = row.as_ref().is_none_or(|r| live_spec(spec_for(r), &l).1);
        drop(l);
        if !needed {
            return true;
        }
        if !self.node.unload(name).await {
            return false;
        }
        // An always-on cluster comes straight back with its new settings.
        if let Some(row) = row {
            self.open_always_on(&row).await;
        }
        true
    }

    /// Unloads `name`, waiting up to `wait` for its running requests and for a load in progress.
    async fn unload_retrying(&self, name: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if self.node.get_loaded(name).is_none() && !self.wakes.is_loading(name) {
                return true;
            }
            if self.node.get_loaded(name).is_some() && self.node.unload(name).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Deletes a cluster and its data. Branches keep working: segments they share are deleted
    /// by garbage collection only once nothing references them.
    pub async fn delete(&self, id: &str) -> Result<()> {
        let c = self.get(id).await?;
        {
            // From here the catalog hides the cluster, a wake can't open it, and a loaded (or
            // waking) instance checks against an unguessable secret.
            let _g = self.specs.lock().await;
            self.wakes.set_deleting(&c.name, true);
            if self.cluster_leases().is_some() {
                // Other nodes' lookups no longer find it (none can wake it: this node holds its
                // lease while the delete runs here).
                if let Err(e) = self.store.set_deleting(&c.id, true).await {
                    self.wakes.set_deleting(&c.name, false);
                    return Err(e.into());
                }
            }
            self.apply_spec(&c.name).await;
        }
        let r = self.delete_inner(&c).await;
        let _g = self.specs.lock().await;
        self.wakes.set_deleting(&c.name, false);
        match &r {
            Ok(()) => self.wakes.forget(&c.name),
            // Still there: give it its credentials back.
            Err(_) => {
                if self.cluster_leases().is_some()
                    && let Err(e) = self.store.set_deleting(&c.id, false).await
                {
                    tracing::warn!(cluster = c.name, error = %e, "un-hiding a search cluster after a failed delete failed");
                }
                self.apply_spec(&c.name).await;
            }
        }
        r
    }

    async fn delete_inner(&self, c: &SearchCluster) -> Result<()> {
        // A wake that read the catalog before the delete finishes loading (and is locked out by
        // its reconcile) before the cluster is unloaded; running requests are waited for.
        self.settle(&c.name).await;
        if !self.unload_retrying(&c.name, Duration::from_secs(30)).await {
            return Err(ControlError::Conflict(format!("cluster {} has requests running; retry", c.name)));
        }
        self.data.delete_cluster(&c.name).await?;
        self.store.delete(&c.id).await?;
        // Its lease goes for good: the next cluster with this name starts from nothing.
        if let Some(l) = self.cluster_leases() {
            l.cluster().retire(&crate::cluster::key(&c.name));
        }
        self.refreshed.lock().remove(&c.name);
        self.reloads.lock().remove(&c.name);
        Ok(())
    }

    /// Test seam: delays every cluster's metadata load, to widen the window of a wake.
    #[doc(hidden)]
    pub fn set_wake_delay(&self, d: Duration) {
        self.wakes.set_delay(d);
    }

    /// Scale to zero now. `false` when requests are running.
    pub async fn unload(&self, id: &str) -> Result<bool> {
        let c = self.get(id).await?;
        if self.node.get_loaded(&c.name).is_none() {
            return Ok(true);
        }
        let unloaded = self.node.unload(&c.name).await;
        if unloaded {
            // Asleep figures come from the commit points the unload just wrote: record them now
            // rather than at the next metering pass, so the cluster's page is right at once.
            if let Err(e) = self.refresh_stats(&c.name, now_ms()).await {
                tracing::warn!(cluster = c.name, error = %e, "refreshing search cluster figures failed");
                self.meter.mark_dirty(&c.name);
            }
        }
        Ok(unloaded)
    }

    /// Where clients connect.
    pub fn connection(&self, c: &SearchCluster) -> Value {
        connection_of(&self.cfg, &c.name)
    }

    /// Loaded or asleep, with live figures when loaded here. On a fleet a cluster's own page
    /// asks its owner (`crate::cluster::on_owner`); a list shows a cluster loaded on another
    /// live node as `loaded` there (`node`), from the catalog (no lease read per cluster).
    pub async fn status(&self, c: &SearchCluster) -> Value {
        match self.node.get_loaded(&c.name) {
            Some(l) => {
                let st = l.cluster.state();
                let mut v = json!({
                    "state": "loaded",
                    "in_flight": l.in_flight(),
                    "indices": st.metadata.indices.len(),
                    "cluster_uuid": st.metadata.cluster_uuid,
                });
                if let Some(l) = self.cluster_leases() {
                    v["node"] = json!(l.cluster().node_id());
                }
                v
            }
            None => {
                // Loaded on another live node, as it recorded (a node that died is no longer live).
                if let Some(l) = self.cluster_leases()
                    && let Some(n) = c.loaded_on.as_deref()
                    && n != l.cluster().node_id()
                    && l.cluster().nodes().iter().any(|x| x.id == n)
                {
                    return json!({ "state": "loaded", "node": n });
                }
                json!({ "state": "asleep" })
            }
        }
    }

    // --- REST through the node -------------------------------------------------------------

    /// Sends a request to the cluster as its owner (through authentication and admission, so
    /// limits apply and it wakes the cluster like any client).
    async fn call(&self, c: &SearchCluster, mut req: EsRequest) -> EsResponse {
        req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("host"));
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.name, c.password));
        req.headers.push(("Authorization".into(), format!("Basic {token}")));
        self.node.handle(req).await
    }

    /// One Elasticsearch request on cluster `id` as its owner narrowed to the index names of
    /// `scope` (the per-index routes of the API: the request reaches no other index, also through
    /// a bulk line's `_index` or an expression): the answer's status, content type and body,
    /// unchanged. Wakes the cluster.
    pub async fn es(&self, id: &str, method: &str, uri: &str, scope: &str, content_type: Option<&str>, body: bytes::Bytes) -> Result<(u16, String, bytes::Bytes)> {
        let c = self.get(id).await?;
        let mut req = EsRequest::new(method, uri);
        req.headers.push((search_es::xpack::security::INDEX_SCOPE_HEADER.into(), scope.to_string()));
        if let Some(ct) = content_type.filter(|_| !body.is_empty()) {
            req.headers.push(("Content-Type".into(), ct.to_string()));
        }
        req.body = body;
        let resp = self.call(&c, req).await;
        let ct = resp.header("content-type").unwrap_or("application/json").to_string();
        Ok((resp.status, ct, resp.body))
    }

    /// A credential for one link of an app's service to cluster `id` (apps' `Resources::link`):
    /// what a service needs to use the cluster's data, never the cluster's password and never
    /// its security (the [`link_role`]). An Elasticsearch-flavor cluster gets an API key with that
    /// role (`ELASTICSEARCH_URL` and `ELASTICSEARCH_API_KEY`, sent as `Authorization: ApiKey`).
    /// An OpenSearch-flavor cluster (whose clients don't send API keys) gets an internal user of
    /// its own with a role of its own ([`os_link_role`]), in `OPENSEARCH_URL` as
    /// `<user>@<cluster>:<password>@` (the node routes a Basic user named `user@cluster` to the
    /// cluster). `kept` is what [`Self::revoke_link`] takes.
    pub async fn mint_link(&self, id: &str, owner: &str) -> Result<LinkCredential> {
        let c = self.get(id).await?;
        let url = connection_of(&self.cfg, &c.name)["url"].as_str().unwrap_or_default().to_string();
        if c.flavor == "opensearch" {
            return self.mint_os_link(&c, &url).await;
        }
        let body = serde_json::to_vec(&json!({ "name": format!("link {owner}"), "role_descriptors": { "roll_link": link_role() } })).unwrap_or_default();
        let resp = self.call(&c, EsRequest::json("POST", "/_security/api_key", body)).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let (200, Some(key_id), Some(key)) = (resp.status, v["id"].as_str(), v["api_key"].as_str()) else {
            return Err(ControlError::Internal(anyhow::anyhow!("making a link key of cluster {} failed ({})", c.name, resp.status)));
        };
        let api_key = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}@{}:{key}", c.name));
        Ok(LinkCredential { vars: vec![("ELASTICSEARCH_URL".into(), url), ("ELASTICSEARCH_API_KEY".into(), api_key)], kept: format!("key:{key_id}") })
    }

    /// An OpenSearch-flavor cluster's link: a role and an internal user named
    /// `roll-link-<random>`, the user holding only that role.
    async fn mint_os_link(&self, c: &SearchCluster, url: &str) -> Result<LinkCredential> {
        let name = format!("roll-link-{}", random_string(12));
        let password = random_string(40);
        let api = "/_plugins/_security/api";
        let role = self.call(c, EsRequest::json("PUT", &format!("{api}/roles/{name}"), serde_json::to_vec(&os_link_role()).unwrap_or_default())).await;
        if role.status >= 300 {
            return Err(ControlError::Internal(anyhow::anyhow!("making a link role of cluster {} failed ({})", c.name, role.status)));
        }
        let user = json!({ "password": password, "opendistro_security_roles": [name], "description": "roll: an app's link" });
        let resp = self.call(c, EsRequest::json("PUT", &format!("{api}/internalusers/{name}"), serde_json::to_vec(&user).unwrap_or_default())).await;
        if resp.status >= 300 {
            let _ = self.call(c, EsRequest::new("DELETE", &format!("{api}/roles/{name}"))).await;
            return Err(ControlError::Internal(anyhow::anyhow!("making a link user of cluster {} failed ({})", c.name, resp.status)));
        }
        let mut u = reqwest::Url::parse(url).map_err(|e| ControlError::Internal(anyhow::anyhow!("cluster {} has no URL: {e}", c.name)))?;
        let userinfo = (u.set_username(&format!("{name}@{}", c.name)), u.set_password(Some(&password)));
        if userinfo != (Ok(()), Ok(())) {
            return Err(ControlError::Internal(anyhow::anyhow!("cluster {}'s URL takes no credentials", c.name)));
        }
        Ok(LinkCredential { vars: vec![("OPENSEARCH_URL".into(), u.to_string().trim_end_matches('/').to_string())], kept: format!("osuser:{name}") })
    }

    /// Revokes what [`Self::mint_link`] made (`kept`), at once. Gone already (the key, the
    /// user, the cluster) is done.
    pub async fn revoke_link(&self, id: &str, kept: &str) -> Result<()> {
        let c = match self.get(id).await {
            Ok(c) => c,
            Err(ControlError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        match kept.split_once(':') {
            Some(("key", key_id)) => match self.delete_key(&c.id, key_id).await {
                Ok(()) | Err(ControlError::NotFound(_)) => Ok(()),
                Err(e) => Err(e),
            },
            Some(("osuser", name)) => {
                let api = "/_plugins/_security/api";
                for path in [format!("{api}/internalusers/{name}"), format!("{api}/roles/{name}")] {
                    let r = self.call(&c, EsRequest::new("DELETE", &path)).await;
                    if r.status >= 300 && r.status != 404 {
                        return Err(ControlError::Internal(anyhow::anyhow!("removing link user {name} of cluster {} failed ({})", c.name, r.status)));
                    }
                }
                Ok(())
            }
            Some(("user", user)) => {
                let r = self.call(&c, EsRequest::new("DELETE", &format!("/_security/user/{}", urlencoding::encode(user)))).await;
                if r.status < 300 || r.status == 404 { Ok(()) } else { Err(ControlError::Internal(anyhow::anyhow!("removing link user {user} of cluster {} failed ({})", c.name, r.status))) }
            }
            _ => Err(ControlError::Invalid(format!("not a link credential: {kept:?}"))),
        }
    }

    /// A new API key of cluster `id` (the cluster's own `_security/api_key`), for a client that
    /// mustn't share the cluster's password: `api_key` is `base64(<key id>@<cluster>:<key>)`,
    /// sent as `Authorization: ApiKey …`, and works on any host (the `@<cluster>` names the
    /// cluster). The key is in this answer only. `expires_in_s`: it stops working after that.
    /// `access`: `admin` (the default: everything the cluster's own user may do) or `data` (a
    /// link's role: data, templates, pipelines and lifecycle policies, no security).
    pub async fn create_key(&self, id: &str, name: &str, expires_in_s: Option<i64>, access: Option<&str>) -> Result<Value> {
        let c = self.get(id).await?;
        if name.trim().is_empty() || name.len() > 1024 {
            return Err(ControlError::Invalid("name: 1 to 1024 characters".into()));
        }
        if c.flavor == "opensearch" {
            return Err(ControlError::Invalid(format!("the OpenSearch-flavor cluster {} has no API keys: OpenSearch clients sign in with a user", c.name)));
        }
        let mut body = json!({ "name": name });
        match access.unwrap_or("admin") {
            "admin" => {}
            "data" => body["role_descriptors"] = json!({ "roll_link": link_role() }),
            a => return Err(ControlError::Invalid(format!("access must be admin or data, not {a:?}"))),
        }
        if let Some(s) = expires_in_s {
            if s <= 0 {
                return Err(ControlError::Invalid("expires_in_s must be positive".into()));
            }
            body["expiration"] = json!(format!("{s}s"));
        }
        let resp = self.call(&c, EsRequest::json("POST", "/_security/api_key", serde_json::to_vec(&body).unwrap_or_default())).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if resp.status != 200 {
            return Err(ControlError::Internal(anyhow::anyhow!("creating an API key of cluster {} failed ({}): {v}", c.name, resp.status)));
        }
        let (Some(key_id), Some(key)) = (v["id"].as_str(), v["api_key"].as_str()) else {
            return Err(ControlError::Internal(anyhow::anyhow!("the cluster answered without a key: {v}")));
        };
        let api_key = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}@{}:{key}", c.name));
        let mut out = json!({ "id": key_id, "name": name, "api_key": api_key, "url": connection_of(&self.cfg, &c.name)["url"] });
        if let Some(ms) = v["expiration"].as_i64() {
            out["expires_at"] = json!(ms / 1000);
        }
        Ok(out)
    }

    /// Revokes API key `key_id` of cluster `id` (`NotFound` when it doesn't exist or was
    /// revoked already).
    pub async fn delete_key(&self, id: &str, key_id: &str) -> Result<()> {
        let c = self.get(id).await?;
        let body = serde_json::to_vec(&json!({ "ids": [key_id] })).unwrap_or_default();
        let resp = self.call(&c, EsRequest::json("DELETE", "/_security/api_key", body)).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let revoked = v["invalidated_api_keys"].as_array().is_some_and(|a| a.iter().any(|k| k.as_str() == Some(key_id)));
        match (resp.status, revoked) {
            (200, true) => Ok(()),
            (200 | 404, false) => Err(ControlError::NotFound(format!("no API key {key_id} on cluster {}", c.name))),
            (s, _) => Err(ControlError::Internal(anyhow::anyhow!("revoking an API key of cluster {} failed ({s}): {v}", c.name))),
        }
    }

    /// The cluster's indices (`_cat/indices`): health, status, docs, size. Wakes the cluster.
    pub async fn indices(&self, id: &str) -> Result<Vec<Value>> {
        let c = self.get(id).await?;
        let resp = self.call(&c, EsRequest::new("GET", "/_cat/indices?format=json&bytes=b&s=index")).await;
        let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        if resp.status != 200 {
            let reason = body.pointer("/error/reason").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(&resp.body).into_owned());
            return Err(ControlError::Internal(anyhow::anyhow!("listing indices failed ({}): {reason}", resp.status)));
        }
        let num = |v: &Value| match v {
            Value::String(s) => s.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
            Value::Number(_) => v.clone(),
            _ => Value::Null,
        };
        let rows = body.as_array().cloned().unwrap_or_default();
        // The engine's live figures where the index is open (what `_stats` reports, unflushed
        // writes included); `_cat`'s otherwise.
        let loaded = self.node.get_loaded(&c.name);
        Ok(rows
            .iter()
            .map(|r| {
                let mut row = json!({
                    "index": r["index"],
                    "uuid": r["uuid"],
                    "health": r["health"],
                    "status": r["status"],
                    "primaries": num(&r["pri"]),
                    "replicas": num(&r["rep"]),
                    "docs": num(&r["docs.count"]),
                    "deleted_docs": num(&r["docs.deleted"]),
                    "size_bytes": num(&r["store.size"]),
                    "primary_size_bytes": num(&r["pri.store.size"]),
                });
                let live = loaded.as_ref().zip(r["uuid"].as_str()).and_then(|(l, uuid)| index_figures(&l.cluster, uuid));
                if let Some(f) = live {
                    // Every shard is a primary: there are no replica copies to add.
                    row["docs"] = f.docs.into();
                    row["deleted_docs"] = f.deleted_docs.into();
                    row["size_bytes"] = f.segment_bytes.into();
                    row["primary_size_bytes"] = f.segment_bytes.into();
                }
                row
            })
            .collect())
    }

    /// Runs one REST request (the dashboard console, `roll search request`). The node counts it
    /// for metering like any client's.
    pub async fn request(&self, id: &str, r: ConsoleRequest) -> Result<ConsoleResponse> {
        let c = self.get(id).await?;
        let method = r.method.trim().to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH") {
            return Err(ControlError::Invalid(format!("unsupported method {}", r.method)));
        }
        let path = r.path.trim();
        if !path.starts_with('/') {
            return Err(ControlError::Invalid("path must start with /".into()));
        }
        let body = console_body(r.body.as_deref())?;
        let mut req = EsRequest::new(&method, path);
        req.headers = r.headers.into_iter().filter(|(k, _)| !k.trim().is_empty()).collect();
        if !body.is_empty() && !req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            let ndjson = path.split('?').next().is_some_and(|p| p.ends_with("/_bulk") || p.ends_with("/_msearch") || p.ends_with("/_msearch/template"));
            req.headers.push(("Content-Type".into(), if ndjson { "application/x-ndjson" } else { "application/json" }.into()));
        }
        req.body = body;
        let started = std::time::Instant::now();
        let resp = self.call(&c, req).await;
        let took_ms = started.elapsed().as_millis() as u64;
        let is_json = resp.headers.iter().any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.contains("json"));
        let truncated = !resp.head && method != "HEAD" && resp.body.len() > CONSOLE_MAX_BODY;
        // HEAD answers carry the GET body for its length; clients never see it.
        let body = if resp.head || method == "HEAD" {
            String::new()
        } else {
            let mut end = resp.body.len().min(CONSOLE_MAX_BODY);
            // Cut on a character boundary.
            while end < resp.body.len() && end > 0 && (resp.body[end] & 0xC0) == 0x80 {
                end -= 1;
            }
            String::from_utf8_lossy(&resp.body[..end]).into_owned()
        };
        Ok(ConsoleResponse { status: resp.status, headers: resp.headers, body, json: is_json && !truncated, took_ms, truncated })
    }

    // --- metering ---------------------------------------------------------------------------

    pub async fn usage(&self, id: &str, days: i64) -> Result<Usage> {
        let c = self.get(id).await?;
        let now = now();
        let since = now - days * 86_400;
        let totals = self.store.usage_since(&c.id, since, now).await?;
        let hours = (now_ms() as f64 / 1000.0 - since.max(c.created_at) as f64).max(0.0) / 3600.0;
        let cost = self.cfg.pricing.cost(&totals, hours, c.stored_bytes);
        Ok(Usage { cluster_id: c.id, days, hours, totals, stored_bytes: c.stored_bytes, cost })
    }

    fn start_metering(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let every = self.cfg.metering_interval;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(c) = weak.upgrade() else { return };
                if let Err(e) = c.sample().await {
                    tracing::warn!(error = %e, "search metering failed");
                }
            }
        });
    }

    fn start_gc(self: &Arc<Self>) {
        let Some(every) = self.cfg.gc_interval else { return };
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = weak.upgrade() else { return };
                // One node of a fleet at a time.
                if let Some(cl) = c.cluster_leases().map(|l| l.cluster().clone())
                    && !tokio::task::spawn_blocking(move || cl.singleton("search-gc")).await.unwrap_or(false)
                {
                    continue;
                }
                let data = c.data.clone();
                drop(c);
                // `collect_garbage`'s future isn't provably `Send` (a higher-ranked lifetime in
                // search-store), so it runs on a blocking thread driven by this runtime.
                let handle = tokio::runtime::Handle::current();
                let r = tokio::task::spawn_blocking(move || handle.block_on(data.collect_garbage(every))).await;
                match r {
                    Ok(Ok(st)) => tracing::info!(stats = ?st, "search storage gc"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "search storage gc failed"),
                    Err(e) => tracing::warn!(error = %e, "search storage gc panicked"),
                }
            }
        });
    }

    /// One metering pass (also called by tests): takes the node's request counts, accounts
    /// loaded time and requests, and refreshes the figures of clusters that just loaded or
    /// unloaded, and of loaded ones every `stats_interval`.
    pub async fn sample(&self) -> anyhow::Result<()> {
        for (name, n) in self.node.take_request_counts() {
            self.meter.count_requests(&name, n.min(i64::MAX as u64) as i64);
        }
        let at = now_ms();
        for (name, forced) in self.meter.sample(at).await? {
            let due = forced || self.refreshed.lock().get(&name).is_none_or(|t| at - t >= self.cfg.stats_interval.as_millis() as i64);
            if !due {
                continue;
            }
            if let Err(e) = self.refresh_stats(&name, at).await {
                tracing::warn!(cluster = name, error = %e, "refreshing search cluster figures failed");
                self.meter.mark_dirty(&name);
            }
        }
        Ok(())
    }

    /// Re-reads a cluster's docs and stored bytes from its commit points.
    pub async fn refresh_stats(&self, name: &str, at_ms: i64) -> anyhow::Result<()> {
        let Some(c) = self.store.by_name(name).await? else { return Ok(()) };
        let u = self.data.cluster_usage(name).await?;
        let loaded = self.node.get_loaded(name);
        // Loaded with every index open, the engine counts the documents exactly (unflushed
        // writes included); otherwise the commit points do. Stored bytes are always the object
        // store's: they are what is billed.
        let docs = match loaded.as_ref().map(|l| engine_figures(&l.cluster)) {
            Some(f) if f.complete() => f.docs as i64,
            _ => u.docs as i64,
        };
        let indices = match &loaded {
            Some(l) => l.cluster.state().metadata.indices.len() as i64,
            None => match self.meter.take_indices(name) {
                Some(n) => n,
                // Asleep and not seen loaded (a fresh branch): count the stored state's indices.
                None => match self.data.read_cluster(name).await? {
                    Some(st) => stored_index_count(&st.json).unwrap_or(c.indices),
                    None => 0,
                },
            },
        };
        drop(loaded);
        self.store.set_stats(&c.id, at_ms / 1000, indices, docs, u.segment_bytes as i64).await?;
        if c.own_bytes != u.own_bytes as i64 {
            self.store.set_own_bytes(&c.id, u.own_bytes as i64).await?;
        }
        let mut refreshed = self.refreshed.lock();
        if self.node.get_loaded(name).is_some() {
            refreshed.insert(name.to_string(), at_ms);
        } else {
            refreshed.remove(name);
        }
        Ok(())
    }
}

/// The bytes a console request sends: nothing for no body or `null`, a string's contents, or
/// any other JSON value exactly as written.
fn console_body(raw: Option<&serde_json::value::RawValue>) -> Result<Bytes> {
    let Some(text) = raw.map(|r| r.get().trim()) else { return Ok(Bytes::new()) };
    if text == "null" {
        return Ok(Bytes::new());
    }
    if text.starts_with('"') {
        let s: String = serde_json::from_str(text).map_err(|e| ControlError::Invalid(format!("body: {e}")))?;
        return Ok(Bytes::from(s));
    }
    Ok(Bytes::copy_from_slice(text.as_bytes()))
}

/// The number of indices in a stored cluster state (`search-es`'s `ClusterState::to_json`).
fn stored_index_count(json: &[u8]) -> Option<i64> {
    let v: Value = serde_json::from_slice(json).ok()?;
    let v = v.get("state").unwrap_or(&v);
    Some(v.get("indices")?.as_object()?.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_name_the_front() {
        let mut cfg = SearchConfig::local(std::env::temp_dir());
        cfg.public_host = "10.0.0.5".into();
        cfg.http_port = 9200;
        assert_eq!(connection_of(&cfg, "app")["url"], "http://10.0.0.5:9200");
        cfg.tls = true;
        cfg.http_port = 443;
        assert_eq!(connection_of(&cfg, "app")["url"], "https://10.0.0.5");
        cfg.public_url = Some("https://abc.lambda-url.eu-west-1.on.aws/".into());
        let c = connection_of(&cfg, "app");
        assert_eq!((c["url"].as_str(), c["port"].as_u64(), c["username"].as_str()), (Some("https://abc.lambda-url.eu-west-1.on.aws"), Some(443), Some("app")));
        cfg.public_url = Some("https://search.example.com:9243".into());
        cfg.domain = Some("search.example.com".into());
        assert_eq!(connection_of(&cfg, "app")["url"], "https://app.search.example.com:9243");
        cfg.public_url = Some("not a url".into());
        cfg.domain = None;
        assert_eq!(connection_of(&cfg, "app")["url"], "https://10.0.0.5");
    }

    #[test]
    fn console_bodies_keep_their_bytes() {
        let raw = |t: &str| serde_json::from_str::<Box<serde_json::value::RawValue>>(t).unwrap();
        let body = |t: Option<&str>| console_body(t.map(raw).as_deref()).unwrap();
        assert_eq!(body(None), Bytes::new());
        assert_eq!(body(Some("null")), Bytes::new());
        // Key order, spacing and number spelling survive (serde_json's Value would sort keys).
        assert_eq!(body(Some(r#"{"z": 1.0, "a": [1e2]}"#)), Bytes::from_static(br#"{"z": 1.0, "a": [1e2]}"#));
        assert_eq!(body(Some(r#""{\"index\":{}}\n{}\n""#)), Bytes::from_static(b"{\"index\":{}}\n{}\n"));
        assert_eq!(body(Some("42")), Bytes::from_static(b"42"));
        // As it arrives in a request.
        let r: ConsoleRequest = serde_json::from_str(r#"{"method":"PUT","path":"/a/_doc/1","body":{"title":"x","price":1}}"#).unwrap();
        assert_eq!(console_body(r.body.as_deref()).unwrap(), Bytes::from_static(br#"{"title":"x","price":1}"#));
        let r: ConsoleRequest = serde_json::from_str(r#"{"method":"GET","path":"/"}"#).unwrap();
        assert!(r.body.is_none());
    }

    #[test]
    fn index_counts() {
        assert_eq!(stored_index_count(br#"{"format":1,"indices":{"a":{},"b":{}}}"#), Some(2));
        assert_eq!(stored_index_count(br#"{"state":{"indices":{}}}"#), Some(0));
        assert_eq!(stored_index_count(b"nope"), None);
    }

    #[test]
    fn names() {
        for ok in ["a", "app", "app-dev", "yaml-1234-w1-3", "a1", &"a".repeat(63)] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "1app", "-app", "app-", "App", "my_app", "a.b", "a@b", "a:b", &"a".repeat(64), "é"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn specs() {
        let c = SearchCluster {
            id: "srch_x".into(),
            name: "app".into(),
            region: "local".into(),
            flavor: "opensearch".into(),
            password: "pw".into(),
            idle_timeout_s: 0,
            max_in_flight: 3,
            max_body_mb: 2,
            destructive_requires_name: false,
            parent_id: None,
            parent_at: None,
            created_at: 0,
            password_rotated_at: None,
            indices: 0,
            docs: 0,
            stored_bytes: 0,
            own_bytes: 0,
            loaded_on: None,
        };
        let s = spec_for(&c);
        assert_eq!(s.name, "app");
        assert_eq!(s.dialect, Dialect::OpenSearch);
        assert_eq!(s.credentials.len(), 1);
        assert_eq!(s.credentials[0].secret, "pw");
        // "Always on" must not overflow the node's millisecond arithmetic.
        assert!(s.idle_unload.unwrap().as_millis() < u64::MAX as u128);
        assert_eq!((s.max_in_flight, s.max_body_bytes, s.destructive_requires_name), (3, Some(2 << 20), false));
        let s = spec_for(&SearchCluster { idle_timeout_s: 60, flavor: "elasticsearch".into(), ..c });
        assert_eq!(s.idle_unload, Some(Duration::from_secs(60)));
        assert_eq!(s.dialect, Dialect::Elasticsearch);
    }

    #[test]
    fn ids_and_secrets() {
        let id = new_id();
        assert!(id.starts_with("srch_") && id.len() == 17);
        assert_ne!(new_id(), id);
        let pw = random_string(32);
        assert_eq!(pw.len(), 32);
        // No '_' in secrets: Bearer tokens are `<cluster>_<secret>`, split at the last '_'.
        assert!(!pw.contains('_'));
    }
}
