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
mod browser_keys;
pub use browser_keys::{NewBrowserKey, Revoked};

use crate::store::{Caps, CorsSettings, DeletedCluster, Inserted, IpRule, Over, Recorded, SearchCluster, Store, UsageTotals};
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
    /// Refused to this caller (HTTP 403): roll secrets wouldn't let them reveal a reference.
    #[error("{0}")]
    Forbidden(String),
    /// Too much of something runs at once (HTTP 429): try again later.
    #[error("{0}")]
    Busy(String),
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
    /// How long a deleted cluster's storage is kept for restoring (`--search-deleted-retention-days`;
    /// zero: deleted at once).
    pub deleted_retention: Duration,
    /// How often deleted clusters whose window ended are purged ([`Control::purge_deleted`];
    /// `None`: never in the background).
    pub purge_interval: Option<Duration>,
    /// How often the node wakes sleeping clusters whose background jobs came due
    /// ([`Control::start_job_wakes`]; `None`: never).
    pub job_wake_interval: Option<Duration>,
    /// How often loaded clusters are metered.
    pub metering_interval: Duration,
    /// Publish Elasticsearch's test cluster features, so the YAML REST suites can run against
    /// clusters made through the API (`search-yaml run --cluster-hook`). Test deployments only.
    pub test_features: bool,
    /// Node settings every cluster reports (Elasticsearch's `-E`, e.g. `path.repo`).
    pub node_settings: Vec<(String, String)>,
    /// How often a loaded cluster's figures (docs, stored bytes) are re-read from storage.
    pub stats_interval: Duration,
    /// Memory all loaded clusters may hold together (`None`: no node budget).
    pub memory_budget: Option<u64>,
    /// Memory documents being inverted may add to every cluster's indexing buffers at once
    /// (`search_server::NodeConfig::max_analysis_bytes`; `None`: derived from `memory_budget`).
    pub max_analysis_bytes: Option<usize>,
    /// Without a sealer, keep the secrets clients give in the clusters in the clear instead of
    /// refusing them (`search_es::ClusterConfig::unsealed_secrets`): local development only.
    pub unsealed_secrets: bool,
    /// Request body bytes all clusters together may have in flight on this node, on top of
    /// each cluster's own budget (twice its `max_body_mb`); past it, 429 (`None`: no node
    /// budget).
    pub max_in_flight_bytes: Option<usize>,
    /// The catalog in a Postgres database every node of a fleet shares (rolld's system
    /// database; `None`: `search.db` under `home`).
    pub catalog: Option<String>,
    /// Connections the catalog's Postgres pool may open (rolld's share of the system database).
    pub catalog_connections: usize,
    pub defaults: Defaults,
    pub pricing: Pricing,
    /// Seals the secrets clusters store (watch passwords, connector and Fleet secrets, `s3`
    /// repository credentials) and clusters' own passwords in the catalog under the deployment's
    /// key (rolld: `ROLL_SECRET_KEY`); `None`: stored as given.
    pub sealer: Option<Arc<search_es::seal::Sealer>>,
    /// The name every cluster's default snapshot repository has (`found-snapshots`, as on
    /// Elastic Cloud): an `s3` repository on roll's own object storage, so `_snapshot` and
    /// snapshot lifecycle policies work without registering one. `None`: no default repository.
    pub default_repository: Option<String>,
    /// What a signed-in person who isn't an admin may own (rolld:
    /// `--search-max-clusters-per-person`, `--search-max-always-on-per-person`; 0: no cap).
    pub person_caps: Caps,
    /// How long and how much each cluster's security audit log keeps (rolld:
    /// `--search-audit-retention-days`, `--search-audit-max-mb`).
    pub audit: search_server::audit::AuditConfig,
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
            deleted_retention: Duration::from_secs(7 * 86_400),
            purge_interval: Some(Duration::from_secs(600)),
            job_wake_interval: Some(Duration::from_secs(15)),
            metering_interval: Duration::from_secs(60),
            test_features: false,
            node_settings: Vec::new(),
            stats_interval: Duration::from_secs(600),
            memory_budget: None,
            max_in_flight_bytes: None,
            unsealed_secrets: false,
            max_analysis_bytes: None,
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
            sealer: None,
            default_repository: Some("found-snapshots".into()),
            person_caps: Caps { clusters: 50, always_on: 2 },
            audit: search_server::audit::AuditConfig::default(),
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
    /// Who may reach the data plane: CIDRs or addresses, each a string or `{cidr, description?}`
    /// (at most [`search_server::allow::MAX_ENTRIES`]); `[]` opens the cluster to anyone again.
    pub ip_allowlist: Option<Vec<IpRuleInput>>,
    /// Which web pages may call the cluster from a browser (`http.cors.*`); `null` or no
    /// `allow_origin` turns CORS off.
    #[serde(default, deserialize_with = "present")]
    pub cors: Option<Option<CorsSettings>>,
    /// Requests per minute all its browser keys may make together; `null`: the node's default.
    #[serde(default, deserialize_with = "present")]
    pub browser_per_minute: Option<Option<i64>>,
    /// The cluster's `reindex.remote.whitelist`: `host:port` patterns (`*` matches any run of
    /// characters) reindex from remote may read, at most [`MAX_WHITELIST`]; `[]`: none.
    pub reindex_whitelist: Option<Vec<String>>,
}

/// The most patterns a cluster's reindex whitelist holds.
pub const MAX_WHITELIST: usize = 64;

/// A reindex whitelist as given, checked: `host:port` patterns of letters, digits, `.`, `-`,
/// `_`, `*` and an IPv6 literal's brackets and colons, each at most 260 characters.
pub fn whitelist_patterns(given: Vec<String>) -> Result<Vec<String>> {
    if given.len() > MAX_WHITELIST {
        return Err(ControlError::Invalid(format!("reindex_whitelist: at most {MAX_WHITELIST} entries")));
    }
    let mut out: Vec<String> = Vec::with_capacity(given.len());
    for g in given {
        let p = g.trim().to_ascii_lowercase();
        let ok = p.len() <= 260
            && p.rsplit_once(':').is_some_and(|(h, port)| !h.is_empty() && !port.is_empty() && (port == "*" || port.parse::<u16>().is_ok()))
            && p.chars().all(|c| c.is_ascii_alphanumeric() || ".-_*:[]".contains(c));
        if !ok {
            return Err(ControlError::Invalid(format!("reindex_whitelist: [{}] isn't a host:port pattern", g.chars().take(80).collect::<String>())));
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// A field that was given, `null` included (`Some(None)`), apart from one left out (`None`).
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// CORS settings as given: checked as the node reads them and kept as browsers send origins
/// (`None`: off).
fn cors_settings(given: Option<CorsSettings>) -> Result<Option<CorsSettings>> {
    let Some(g) = given.filter(|g| !g.allow_origin.is_empty()) else { return Ok(None) };
    let cors = search_server::cors::Cors::new(&g.allow_origin, g.allow_methods.as_deref(), g.allow_headers.as_deref(), g.max_age).map_err(|e| ControlError::Invalid(format!("cors: {e}")))?;
    Ok(Some(CorsSettings { allow_origin: cors.origins().iter().map(|o| o.as_written()).collect(), ..g }))
}

/// An allowlist entry as given (roll apps' `allow_ips` take the same).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum IpRuleInput {
    Cidr(String),
    Rule(IpRuleFields),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IpRuleFields {
    pub cidr: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The most characters an allowlist entry's description may have.
const DESCRIPTION_CHARS: usize = 200;

/// An allowlist as given, checked and made canonical.
fn ip_rules(given: Vec<IpRuleInput>) -> Result<Vec<IpRule>> {
    if given.len() > search_server::allow::MAX_ENTRIES {
        return Err(ControlError::Invalid(format!("ip_allowlist: at most {} entries", search_server::allow::MAX_ENTRIES)));
    }
    given
        .into_iter()
        .map(|g| {
            let (cidr, description) = match g {
                IpRuleInput::Cidr(c) => (c, None),
                IpRuleInput::Rule(r) => (r.cidr, r.description),
            };
            let net = search_server::allow::parse_cidr(&cidr).map_err(|e| ControlError::Invalid(format!("ip_allowlist: {e}")))?;
            let description = description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
            if description.as_ref().is_some_and(|d| d.chars().count() > DESCRIPTION_CHARS || d.chars().any(char::is_control)) {
                return Err(ControlError::Invalid(format!("ip_allowlist: a description is one line of at most {DESCRIPTION_CHARS} characters")));
            }
            Ok(IpRule { cidr: net.to_string(), description })
        })
        .collect()
}

/// One REST request through the console, borrowed from the bytes it was read from (the body
/// is sent as a slice of them, never copied).
#[derive(Clone, Debug, Deserialize)]
pub struct ConsoleRequest<'a> {
    pub method: String,
    /// `/index/_search?q=…`
    pub path: String,
    /// A JSON value is sent as JSON, byte for byte as the client wrote it (key order and
    /// number spelling matter: Elasticsearch stores `_source` exactly as sent); a string is sent
    /// as is (NDJSON for `_bulk`).
    #[serde(default, borrow)]
    pub body: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl<'a> ConsoleRequest<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<ConsoleRequest<'a>> {
        serde_json::from_slice(bytes).map_err(|e| ControlError::Invalid(format!("invalid request: {e}")))
    }

    /// The bytes the request sends, out of `whole` (what [`ConsoleRequest::parse`] read):
    /// nothing for no body or `null`, a string's contents, or any other JSON value exactly as
    /// written, as a slice of `whole`.
    pub fn body_in(&self, whole: &Bytes) -> Result<Bytes> {
        let Some(text) = self.body.map(|r| r.get().trim()) else { return Ok(Bytes::new()) };
        if text == "null" {
            return Ok(Bytes::new());
        }
        if text.starts_with('"') {
            let s: String = serde_json::from_str(text).map_err(|e| ControlError::Invalid(format!("body: {e}")))?;
            return Ok(Bytes::from(s));
        }
        Ok(whole.slice_ref(text.as_bytes()))
    }

    /// Whether the request answers or sets a credential on cluster `c`, decided on the API the
    /// cluster's router picks for it ([`search_es::routes::handles_credentials`]).
    pub fn handles_credentials(&self, c: &SearchCluster) -> bool {
        search_es::routes::handles_credentials(dialect(&c.flavor).unwrap_or(Dialect::Elasticsearch), &self.method, self.path.trim())
    }
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

/// How long a restore's or purge's claim on a deleted cluster holds before another node may take
/// it over (the node that held it died).
const CLAIM_TTL: Duration = Duration::from_secs(15 * 60);

/// Most revokes a deleted cluster keeps for its restore ([`Control::delete_key`]).
const MAX_KEPT_REVOKES: i64 = 1000;

/// The name a deleted cluster's storage is kept under: never a cluster name (those have no `_`),
/// and new for every delete, so a copy from an earlier delete of the same cluster is never
/// mixed in.
fn kept_name(id: &str) -> String {
    format!("deleted_{id}_{}", random_string(8))
}

fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

/// The 409 for a write past a person's cap: the cap, their count, and how it's raised.
fn over(o: Over) -> ControlError {
    ControlError::Conflict(match o {
        Over::Clusters { cap, count } => format!(
            "you have {count} search clusters (branches included) and a person may have {cap}: delete one, or the operator can raise the cap with --search-max-clusters-per-person"
        ),
        Over::AlwaysOn { cap, count } => format!(
            "you have {count} always-on search clusters (idle_timeout_s 0) and a person may have {cap}: a cluster that sleeps when idle (idle_timeout_s above 0) doesn't count, or the operator can raise the cap with --search-max-always-on-per-person"
        ),
    })
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
    spec.ip_allowlist = allowlist_of(&c.ip_allowlist);
    // The cluster's own, even empty (`[]`: no remote), never the node setting's.
    spec.reindex_whitelist = Some(c.reindex_whitelist.clone());
    spec.browser_per_minute = c.browser_per_minute.and_then(|n| u64::try_from(n).ok());
    spec.cors = c.cors.as_ref().and_then(|g| match search_server::cors::Cors::new(&g.allow_origin, g.allow_methods.as_deref(), g.allow_headers.as_deref(), g.max_age) {
        Ok(c) => Some(Arc::new(c)),
        // Checked when written; one that no longer parses (a hand-edited catalog) allows nobody.
        Err(e) => {
            tracing::error!(cluster = c.name, error = e, "a cluster's CORS settings in the catalog don't parse: CORS is off");
            None
        }
    });
    spec.catalog_id = Some(c.id.clone());
    spec.owner = c.owner.clone();
    // Only until its first load has marked what it inherited: a branch made before marking
    // existed, or one already marked, runs its jobs as its own.
    spec.branch_of = c.parent_id.clone().filter(|_| c.inherit_pending);
    spec
}

/// The node's allowlist for the catalog's rules. They were checked when written; one that no
/// longer parses (a hand-edited catalog) is logged and closes the list rather than opening it.
fn allowlist_of(rules: &[IpRule]) -> search_server::IpAllowlist {
    let mut nets = Vec::with_capacity(rules.len());
    for r in rules {
        match search_server::allow::parse_cidr(&r.cidr) {
            Ok(n) => nets.push(n),
            Err(e) => tracing::error!(cidr = r.cidr, error = e, "an IP allowlist entry in the catalog doesn't parse: it admits nobody"),
        }
    }
    if nets.is_empty() && !rules.is_empty() {
        // 0.0.0.0 is never a client's address.
        nets.push(std::net::IpAddr::from([0, 0, 0, 0]).into());
    }
    search_server::IpAllowlist::new(nets)
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

/// The name prefix of the internal users the keys route makes on OpenSearch-flavor clusters.
const OS_KEY_PREFIX: &str = "roll-key-";

/// A link's credential ([`Control::mint_link`]): its variables (unprefixed) and what revoking
/// it takes.
#[derive(Clone)]
pub struct LinkCredential {
    pub vars: Vec<(String, String)>,
    pub kept: String,
}

impl std::fmt::Debug for LinkCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vars: Vec<_> = self.vars.iter().map(|(k, _)| (k.as_str(), "<redacted>")).collect();
        f.debug_struct("LinkCredential").field("vars", &vars).field("kept", &self.kept).finish()
    }
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

/// A metrics range: which ring it reads (`res`), the step of its points and how many.
struct MetricsRange {
    res: i64,
    step: i64,
    points: i64,
}

impl MetricsRange {
    fn parse(range: &str) -> Option<MetricsRange> {
        match range {
            // A minute a point.
            "1h" => Some(MetricsRange { res: 60, step: 60, points: 60 }),
            // 15 minutes a point, from the minute ring.
            "24h" => Some(MetricsRange { res: 60, step: 900, points: 96 }),
            // 2 hours a point, from the hour ring.
            "14d" => Some(MetricsRange { res: 3600, step: 7200, points: 168 }),
            _ => None,
        }
    }
}

/// One kind's (or every kind's) requests over `seconds`, as the metrics route answers them.
fn summary_json(k: &search_server::metrics::KindStats, seconds: i64) -> Value {
    let ms = |q: f64| k.latency.quantile(q).map(|us| (us / 1000.0 * 1000.0).round() / 1000.0);
    json!({
        "requests": k.requests(),
        "rate": k.requests() as f64 / seconds.max(1) as f64,
        "2xx": k.ok,
        "4xx": k.client_errors,
        "5xx": k.server_errors,
        "rejected": k.rejected,
        "p50_ms": ms(0.5),
        "p95_ms": ms(0.95),
        "p99_ms": ms(0.99),
    })
}

fn point_json(at: i64, s: &search_server::metrics::Stats, seconds: i64) -> Value {
    let mut v = json!({ "at": at, "all": summary_json(&s.all(), seconds) });
    for k in search_server::metrics::KINDS {
        v[k.name()] = summary_json(s.kind(k), seconds);
    }
    v
}

pub struct Control {
    pub cfg: SearchConfig,
    pub store: Arc<Store>,
    pub node: Arc<Node>,
    /// Segments, WAL and cluster metadata on object storage.
    pub data: search_store::Store,
    pub meter: Arc<Meter>,
    /// Wakes in progress and clusters being deleted ([`crate::wake`]).
    pub(crate) wakes: Arc<Wakes>,
    /// One query suggestions build at a time per index (`crate::suggestions`), by cluster id and
    /// index.
    pub(crate) suggest_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Serializes changes of a cluster's description with applying it to the node, so the node
    /// never ends up with an older one than the catalog's.
    specs: tokio::sync::Mutex<()>,
    /// Loaded clusters whose new settings need a reload (dialect, the
    /// `destructive_requires_name` default): unloaded once no request runs.
    reloads: Mutex<HashSet<String>>,
    reloader: AtomicBool,
    me: OnceLock<Weak<Control>>,
    /// When each loaded cluster's figures were last read from storage (ms), by catalog id.
    refreshed: Mutex<HashMap<String, i64>>,
    /// The roll-cluster fleet this node serves clusters in ([`Control::join`]).
    leases: OnceLock<Arc<crate::cluster::ClusterLeases>>,
    /// Resolves snapshot repositories' credential references ([`Control::set_secrets`]).
    secret_refs: Arc<crate::secret_refs::CatalogRefs>,
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
        search_es::search::check_max_time(&cfg.node_settings)?;
        std::fs::create_dir_all(&cfg.home)?;
        let store = match (catalog, &cfg.catalog) {
            (Some(c), _) => c,
            (None, Some(url)) => Arc::new(Store::postgres(url, code_sql::PgOptions { max_connections: cfg.catalog_connections.max(1), ..Default::default() })?),
            (None, None) => Arc::new(Store::open(&cfg.home.join("search.db"))?),
        };
        if let Some(s) = &cfg.sealer {
            store.seal_passwords(s.clone());
            // Remotes linked before the node had a key: sealed now, or they couldn't be opened.
            let n = store.seal_remote_credentials(|r| s.seal_text(&crate::remotes::kind(&r.cluster_id, &r.alias, &r.target_id), &r.credentials)).await?;
            if n > 0 {
                tracing::info!(remotes = n, "sealed remote credentials stored in the clear");
            }
        }
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
        let secret_refs = Arc::new(crate::secret_refs::CatalogRefs { store: store.clone(), secrets: OnceLock::new() });
        let links = Arc::new(crate::remotes::ControlLinks::new(cfg.domain.clone()));
        let node = Node::with_hooks(
            Arc::new(SqlCatalog { store: store.clone(), wakes: wakes.clone() }),
            Arc::new(GuardedStores { inner: ObjectStores::new(data.clone()), store: store.clone(), wakes: wakes.clone() }),
            NodeConfig {
                node_name: cfg.node_name.clone(),
                domain: cfg.domain.clone(),
                idle_unload: Some(cfg.idle_unload),
                test_features: cfg.test_features,
                node_settings: cfg.node_settings.clone(),
                memory_budget: cfg.memory_budget,
                max_in_flight_bytes: cfg.max_in_flight_bytes,
                sealer: cfg.sealer.clone(),
                unsealed_secrets: cfg.unsealed_secrets,
                max_analysis_bytes: cfg.max_analysis_bytes,
                default_repository: cfg.default_repository.clone(),
                links: Some(links.clone()),
                audit: cfg.audit.clone(),
                secret_refs: Some(search_es::secret_refs::Resolver(secret_refs.clone())),
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
            suggest_locks: Mutex::new(HashMap::new()),
            specs: tokio::sync::Mutex::new(()),
            reloads: Mutex::new(HashSet::new()),
            reloader: AtomicBool::new(false),
            me: OnceLock::new(),
            refreshed: Mutex::new(HashMap::new()),
            leases: OnceLock::new(),
            secret_refs,
        });
        // Nothing can load before the node is handed out, so the hook always finds the control.
        let _ = hooks.control.set(Arc::downgrade(&control));
        links.set(Arc::downgrade(&control));
        let _ = control.me.set(Arc::downgrade(&control));
        control.start_metering();
        control.start_gc();
        control.start_purges();
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
                c.wake_for_suggestions().await;
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

    /// Whether this node is the one to wake sleeping cluster `name` for its background work:
    /// always on a single node; on a fleet, the node its lease is placed on.
    pub(crate) fn wakes_here(&self, name: &str) -> bool {
        self.cluster_leases().is_none_or(|l| l.cluster().preferred(&crate::cluster::key(name)).is_some_and(|n| n.id == l.cluster().node_id()))
    }

    /// The fleet's leases, when this node is in one ([`Control::join`]).
    /// Lets snapshot repositories name roll secrets references (`roll://ENV/KEY`) as
    /// credentials, resolved by `secrets` ([`crate::secret_refs`]). Once; until then (and
    /// without it) references are refused.
    pub fn set_secrets(&self, secrets: Arc<dyn crate::secret_refs::ResourceSecrets>) {
        let _ = self.secret_refs.secrets.set(secrets);
    }

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

    fn apply(mut c: SearchCluster, o: ClusterOptions) -> Result<SearchCluster> {
        if let Some(v) = o.ip_allowlist {
            c.ip_allowlist = ip_rules(v)?;
        }
        if let Some(v) = o.reindex_whitelist {
            c.reindex_whitelist = whitelist_patterns(v)?;
        }
        if let Some(v) = o.cors {
            c.cors = cors_settings(v)?;
        }
        if let Some(v) = o.browser_per_minute {
            if v.is_some_and(|n| !(1..=search_server::browser::MAX_PER_MINUTE as i64).contains(&n)) {
                return Err(ControlError::Invalid(format!("browser_per_minute: 1 to {}", search_server::browser::MAX_PER_MINUTE)));
            }
            c.browser_per_minute = v;
        }
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
        Ok(c)
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

    async fn insert(&self, c: &SearchCluster, caps: Caps) -> Result<()> {
        match self.store.insert(c, caps).await? {
            Inserted::Yes => Ok(()),
            Inserted::Taken => Err(ControlError::Conflict(format!("a search cluster named {} already exists", c.name))),
            Inserted::Over(o) => Err(over(o)),
        }
    }

    /// Saves a cluster's settings, checking `caps` in the same transaction.
    async fn save(&self, c: &SearchCluster, caps: Caps) -> Result<()> {
        self.store.update(c, caps).await?.map_err(over)
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
            wake_at: None,
            owner: None,
            restored_at: None,
            ip_allowlist: Vec::new(),
            cors: None,
            browser_per_minute: None,
            inherit_pending: false,
            reindex_whitelist: Vec::new(),
        }
    }

    /// A new, empty cluster, `owner`'s (a signed-in person's `usr_…`; `None`: the operator's).
    /// Past one of `caps` for the owner it is refused (409). Nothing is written to object storage
    /// until it is first used.
    pub async fn create(&self, name: &str, o: ClusterOptions, owner: Option<&str>, caps: Caps) -> Result<SearchCluster> {
        let c = Self::apply(SearchCluster { owner: owner.map(str::to_string), ..self.fresh(name) }, o)?;
        self.check(&c)?;
        self.check_free(name).await?;
        self.insert(&c, caps).await?;
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
    /// parent's segments, nothing is copied. Settings and the owner come from the parent (settings
    /// unless given), and the branch counts toward the owner's `caps`. In a
    /// fleet, call it on the node holding the parent (the API forwards there): a loaded parent
    /// is committed first, so the branch sees every acknowledged write. `parent` is the cluster
    /// the caller was checked against: it is read again by its id, never its name, so a cluster
    /// that took the name meanwhile is never branched in its place.
    pub async fn branch(&self, parent: &SearchCluster, name: &str, at: Option<i64>, o: ClusterOptions, caps: Caps) -> Result<SearchCluster> {
        let p = self.store.get(&parent.id).await?.ok_or_else(|| ControlError::NotFound(format!("no search cluster {}", parent.id)))?;
        let now = now();
        if let Some(t) = at {
            self.check_window(&p, t, now)?;
        }
        let fresh = self.fresh(name);
        // Settings, owner and last-known figures from the parent; identity and secret of its own.
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
                // Its first load marks what it inherits (`search_es::branch`).
                inherit_pending: true,
                ..p.clone()
            },
            o,
        )?;
        self.check(&c)?;
        self.check_free(name).await?;
        // Refused by a cap before anything is copied (the insert's count, in its transaction,
        // still decides).
        if let Some(owner) = &c.owner
            && let Some(o) = self.store.check_caps(owner, c.idle_timeout_s, caps).await?
        {
            return Err(over(o));
        }
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
            if at.is_some()
                && let Err(e) = self.adopt_current_security(&p, &c).await
            {
                // Never left with security older than its parent's: a revoked key would work.
                if let Err(e2) = self.data.delete_cluster(&c.name).await {
                    tracing::warn!(cluster = c.name, error = %e2, "removing a branch whose security couldn't be brought up to date failed");
                }
                return Err(ControlError::Internal(e));
            }
        }
        if let Err(e) = self.insert(&c, caps).await {
            // Lost a race for the name, or the owner is at a cap: don't leave the branch's
            // objects behind.
            if let Err(e2) = self.data.delete_cluster(&c.name).await {
                tracing::warn!(cluster = c.name, error = %e2, "removing an orphaned branch failed");
            }
            return Err(e);
        }
        self.store.set_stats(&c.id, now, c.indices, c.docs, c.stored_bytes).await?;
        // The parent's recorded figures may be a metering interval old: read the branch's own
        // from its commit points now, so it shows what it holds at once.
        let mut c = c;
        match self.refresh_stats(&c.id, now_ms()).await {
            Ok(()) => {
                if let Ok(Some(fresh)) = self.store.get(&c.id).await {
                    (c.indices, c.docs, c.stored_bytes) = (fresh.indices, fresh.docs, fresh.stored_bytes);
                }
            }
            Err(e) => {
                tracing::warn!(cluster = c.name, error = %e, "reading a new branch's figures failed");
                self.meter.mark_dirty(&c.id);
            }
        }
        Ok(c)
    }

    /// A point-in-time branch `c` of `p` takes `p`'s security as it is now, not as it was at the
    /// point: keys revoked, tokens invalidated, users deleted and passwords changed since stay
    /// so (`search_es::xpack::security::adopt_current_security`). Written to the branch's
    /// stored state before it is in the catalog, so nothing can load the older one meanwhile.
    async fn adopt_current_security(&self, p: &SearchCluster, c: &SearchCluster) -> anyhow::Result<()> {
        let Some(branch) = self.data.read_cluster(&c.name).await? else { return Ok(()) };
        let Some(current) = self.data.read_cluster(&p.name).await? else { return Ok(()) };
        let dialect = dialect(&c.flavor).unwrap_or(Dialect::Elasticsearch);
        let json = search_es::xpack::security::adopt_current_security(&branch.json, &current.json, dialect, now_ms())?;
        self.data.write_cluster(&c.name, &search_store::ClusterState::new(json)).await?;
        Ok(())
    }

    fn check_window(&self, p: &SearchCluster, at: i64, now: i64) -> Result<()> {
        if at > now + 1 {
            return Err(ControlError::Invalid("that point in time is in the future".into()));
        }
        let window = self.cfg.history_retention.as_secs() as i64;
        if window == 0 {
            return Err(ControlError::Invalid("point-in-time branches are disabled on this node (no history is retained)".into()));
        }
        // A restored cluster's history starts at its restore.
        let oldest = (now - window).max(p.created_at).max(p.restored_at.unwrap_or(0));
        if at < oldest {
            return Err(ControlError::Invalid(format!("outside the history window (earliest is {oldest})")));
        }
        Ok(())
    }

    /// Changes settings. Credentials, limits (`max_in_flight` included) and the idle timeout
    /// apply at once, also to a cluster that is waking up. A new flavor (dialect) or
    /// `destructive_requires_name` default needs a reload: the cluster is unloaded now if no
    /// request runs, or as soon as none does; until then it keeps its current ones. Making the
    /// cluster always-on past the owner's `caps.always_on` is refused (409).
    pub async fn update(&self, id: &str, o: ClusterOptions, caps: Caps) -> Result<SearchCluster> {
        self.update_with(id, caps, move |c| {
            *c = Self::apply(c.clone(), o)?;
            Ok(())
        })
        .await
    }

    /// Changes cluster `id` with `f`, which reads and writes the row as it stands under the
    /// settings lock (a read-modify-write no concurrent update is lost to), then saves and
    /// applies it as [`Control::update`] does.
    pub async fn update_with(&self, id: &str, caps: Caps, f: impl FnOnce(&mut SearchCluster) -> Result<()>) -> Result<SearchCluster> {
        let c = {
            let _g = self.specs.lock().await;
            let mut c = self.get(id).await?;
            f(&mut c)?;
            self.check(&c)?;
            self.save(&c, caps).await?;
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
            self.save(&c, Caps::NONE).await?;
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
    pub(crate) async fn reconcile(self: Arc<Self>, name: String, id: String, token: Option<u64>) {
        // The node publishes the instance right after the hook; give up if it unloads first.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut spins = 0u32;
        while self.node.get_loaded(&name).is_none() && self.meter.is_loaded(&id) && Instant::now() < deadline {
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

    /// Deletes a cluster: it leaves lists and lookups and stops answering at once, and its name
    /// is free. With a retention window (`deleted_retention`) its storage is kept until the
    /// window ends, for [`Self::restore`]; without one, it goes now. Branches keep working:
    /// segments they share are deleted by garbage collection only once nothing references them.
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
        // Its audit entries still waiting are written now; the stored ones go with its storage
        // (or move with its kept copy, and back on a restore).
        self.node.audit.forget(&c.name).await;
        // So are its requests and observations not handed on yet: they are keyed by its id, so
        // a cluster that takes the name next never sees them, and nothing bills them now.
        self.node.forget_cluster(&c.id);
        if self.cfg.deleted_retention.is_zero() {
            self.data.delete_cluster(&c.name).await?;
            // Its remotes' keys roll made go with it (a restorable one keeps them for its restore).
            if let Err(e) = self.revoke_remote_keys_of(&c.id).await {
                tracing::warn!(cluster = c.id, error = %e, "revoking a deleted search cluster's remote keys failed");
            }
            self.store.delete(&c.id).await?;
        } else {
            self.keep_deleted(c).await?;
        }
        // Its lease goes for good: the next cluster with this name starts from nothing.
        if let Some(l) = self.cluster_leases() {
            l.cluster().retire(&crate::cluster::key(&c.name));
        }
        self.refreshed.lock().remove(&c.id);
        self.reloads.lock().remove(&c.name);
        Ok(())
    }

    /// Moves a deleted cluster's storage to a name of its own (a branch: new commit points that
    /// name the same segments, pins and state; nothing is copied), then flips its catalog row
    /// to deleted, which frees the name, then removes the commit points under the name. The flip
    /// is the commit point: before it the cluster is still there (and the copy is removed), after
    /// it the cluster is deleted, and storage left under the name keeps the name from being
    /// reused (`check_free`) rather than mixing into another cluster.
    async fn keep_deleted(&self, c: &SearchCluster) -> Result<()> {
        let kept = kept_name(&c.id);
        if self.data.cluster_exists(&c.name).await?
            && let Err(e) = self.data.branch(&c.name, &kept, None).await
        {
            if let Err(e2) = self.data.delete_cluster(&kept).await {
                tracing::warn!(cluster = c.name, kept, error = %e2, "removing a partial copy of a deleted search cluster failed");
            }
            return Err(ControlError::Internal(e));
        }
        let at = now();
        let purge_at = at + self.cfg.deleted_retention.as_secs() as i64;
        if !self.store.mark_deleted(&c.id, &kept, at, purge_at).await? {
            // Deleted meanwhile by another call: its copy is the one kept.
            let _ = self.data.delete_cluster(&kept).await;
            return Err(ControlError::NotFound(format!("no search cluster {}", c.id)));
        }
        if let Err(e) = self.data.move_audit(&c.name, &kept).await {
            tracing::warn!(cluster = c.name, kept, error = %e, "moving a deleted search cluster's audit entries to its kept copy failed");
        }
        if let Err(e) = self.data.delete_cluster(&c.name).await {
            // The cluster is deleted; its old commit points only block the name until removed.
            tracing::warn!(cluster = c.name, error = %e, "removing a deleted search cluster's commit points under its name failed; the name stays taken until they are");
        }
        Ok(())
    }

    /// Deleted clusters kept for restoring (of `owner` only, when given), newest first.
    pub async fn list_deleted(&self, owner: Option<&str>) -> Result<Vec<DeletedCluster>> {
        Ok(self.store.list_deleted(owner).await?)
    }

    /// The deleted cluster `id`, while it can be restored.
    pub async fn get_deleted(&self, id: &str) -> Result<DeletedCluster> {
        match self.store.get_deleted(id).await? {
            Some(d) if d.purge_at > now() => Ok(d),
            _ => Err(ControlError::NotFound(format!("no deleted search cluster {id}"))),
        }
    }

    /// Brings deleted cluster `id` back as it was (data, settings, owner, credentials), under
    /// its own name or `name`. Its storage moves back by a branch of the kept copy. The catalog
    /// claim makes it exclusive with a purge (and another restore) on any node, and reserves
    /// the name for the restore's duration.
    ///
    /// Under another name the password and API keys stay; clients use the new name as the
    /// Basic auth user, in an API key's id (`<key id>@<name>`) and as the host label.
    pub async fn restore(&self, id: &str, name: Option<&str>, caps: Caps) -> Result<SearchCluster> {
        let d = self.get_deleted(id).await?;
        let target = name.unwrap_or(&d.cluster.name).to_string();
        if !valid_name(&target) {
            return Err(ControlError::Invalid("name: 1-63 lowercase letters, digits or '-', starting with a letter and not ending with '-'".into()));
        }
        let taken = || {
            ControlError::Conflict(format!(
                "the name {target} is taken by another search cluster: restore under another name with {{\"name\": \"…\"}}"
            ))
        };
        if self.store.by_name(&target).await?.is_some() || self.wakes.is_deleting(&target) {
            return Err(taken());
        }
        let claim = random_string(16);
        let stale_before = now() - CLAIM_TTL.as_secs() as i64;
        let reserved = match self.store.claim_deleted(&d.cluster.id, &claim, Some(&target), now(), stale_before).await? {
            crate::store::Claim::Claimed { reserved } => reserved,
            crate::store::Claim::Taken => return Err(taken()),
            crate::store::Claim::Busy => return Err(ControlError::Conflict(format!("deleted search cluster {id} is being restored or purged; retry"))),
            crate::store::Claim::Gone => return Err(ControlError::NotFound(format!("no deleted search cluster {id}"))),
        };
        match self.restore_claimed(&d, &target, &reserved, &claim, caps).await {
            Ok(c) => Ok(c),
            Err(e) => {
                if let Err(e2) = self.store.release_claim(&d.cluster.id, &claim).await {
                    tracing::warn!(cluster = id, error = %e2, "releasing a failed restore's claim failed; it lapses");
                }
                Err(e)
            }
        }
    }

    async fn restore_claimed(&self, d: &DeletedCluster, target: &str, reserved: &str, claim: &str, caps: Caps) -> Result<SearchCluster> {
        // Storage under the reserved name can only be an earlier, crashed restore's: the name
        // was this row's since then, and was free of storage before.
        if reserved == target {
            if self.data.cluster_exists(target).await? {
                self.data.delete_cluster(target).await?;
            }
        } else if self.data.cluster_exists(target).await? {
            return Err(ControlError::Conflict(format!("object storage already holds data of a cluster named {target}")));
        }
        let had_data = self.data.cluster_exists(&d.stored_as).await?;
        if had_data && let Err(e) = self.data.branch(&d.stored_as, target, None).await {
            let _ = self.data.delete_cluster(target).await;
            return Err(ControlError::Internal(e));
        }
        match self.store.finish_restore(&d.cluster.id, claim, now(), caps).await? {
            Ok(true) => {}
            Ok(false) => {
                // The claim went stale and a purge took it.
                let _ = self.data.delete_cluster(target).await;
                return Err(ControlError::Conflict(format!("deleted search cluster {} was purged while restoring", d.cluster.id)));
            }
            Err(o) => {
                // The owner is at a cap: the cluster stays deleted (its kept copy is untouched).
                let _ = self.data.delete_cluster(target).await;
                return Err(over(o));
            }
        }
        // Credentials revoked while it was deleted, before anything else (the sweep retries a
        // failure).
        self.apply_revokes(&d.cluster.id).await;
        if let Err(e) = self.data.move_audit(&d.stored_as, target).await {
            tracing::warn!(cluster = target, kept = d.stored_as, error = %e, "moving a restored search cluster's audit entries back failed");
        }
        if had_data && let Err(e) = self.data.delete_cluster(&d.stored_as).await {
            tracing::warn!(cluster = target, kept = d.stored_as, error = %e, "removing a restored search cluster's kept copy failed");
        }
        let c = self.get(&d.cluster.id).await?;
        if let Err(e) = self.refresh_stats(&c.id, now_ms()).await {
            tracing::warn!(cluster = c.name, error = %e, "reading a restored search cluster's figures failed");
            self.meter.mark_dirty(&c.id);
        }
        self.open_always_on(&c).await;
        self.get(&c.id).await
    }

    /// Purges the deleted clusters whose window ended by `now` (Unix seconds): their kept
    /// storage goes the way a delete without a window removes it (`search_store::Store::delete_cluster`:
    /// commit points, state and pins; segments only once no branch references them, by garbage
    /// collection), then their rows. Each is claimed first, so a restore or another node's purge
    /// never runs on it at the same time. Returns how many were purged.
    pub async fn purge_deleted(&self, now: i64) -> Result<usize> {
        let mut purged = 0;
        for d in self.store.due_purges(now).await? {
            let claim = random_string(16);
            let stale_before = crate::control::now() - CLAIM_TTL.as_secs() as i64;
            match self.store.claim_deleted(&d.cluster.id, &claim, None, now, stale_before).await? {
                crate::store::Claim::Claimed { .. } => {}
                _ => continue,
            }
            // A crashed restore's reserved name may hold a copy of the kept storage (see
            // `restore_claimed`).
            let mut names = vec![d.stored_as.clone()];
            if d.reserved != d.stored_as {
                names.push(d.reserved.clone());
            }
            let mut ok = true;
            for n in &names {
                if let Err(e) = self.data.delete_cluster(n).await {
                    tracing::warn!(cluster = d.cluster.id, storage = n, error = %e, "purging a deleted search cluster failed; retried later");
                    ok = false;
                }
            }
            // Its remotes' keys roll made are revoked first: a purge that can't is retried.
            if ok && let Err(e) = self.revoke_remote_keys_of(&d.cluster.id).await {
                tracing::warn!(cluster = d.cluster.id, error = %e, "revoking a purged search cluster's remote keys failed; retried later");
                ok = false;
            }
            if ok && self.store.finish_purge(&d.cluster.id, &claim).await? {
                purged += 1;
            } else if let Err(e) = self.store.release_claim(&d.cluster.id, &claim).await {
                tracing::warn!(cluster = d.cluster.id, error = %e, "releasing a purge claim failed; it lapses");
            }
        }
        Ok(purged)
    }

    /// [`Self::purge_deleted`] every `purge_interval`, on one node of a fleet at a time
    /// (`singleton:search-purge`), as garbage collection is placed.
    fn start_purges(&self) {
        let (Some(me), Some(every)) = (self.me.get().cloned(), self.cfg.purge_interval) else { return };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(c) = me.upgrade() else { return };
                if let Some(cl) = c.cluster_leases().map(|l| l.cluster().clone())
                    && !tokio::task::spawn_blocking(move || cl.singleton("search-purge")).await.unwrap_or(false)
                {
                    continue;
                }
                match c.purge_deleted(now()).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(purged = n, "purged deleted search clusters"),
                    Err(e) => tracing::warn!(error = %e, "purging deleted search clusters failed"),
                }
                match c.store.clusters_with_revokes().await {
                    Ok(ids) => {
                        for id in ids {
                            c.apply_revokes(&id).await;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "listing restored search clusters' revokes failed"),
                }
            }
        });
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
            if let Err(e) = self.refresh_stats(&c.id, now_ms()).await {
                tracing::warn!(cluster = c.name, error = %e, "refreshing search cluster figures failed");
                self.meter.mark_dirty(&c.id);
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
                // Live documents from shard statistics (no search runs) once every index is
                // open; the metered `docs` lags until the next sample.
                let f = crate::meter::engine_figures(&l.cluster);
                if f.complete() {
                    v["docs"] = json!(f.docs);
                }
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
                // A background job (a watch, a transform, ILM, a rollup) wakes it when due.
                match c.wake_at {
                    Some(ms) => json!({ "state": "asleep", "wakes_at": ms.div_euclid(1000) }),
                    None => json!({ "state": "asleep" }),
                }
            }
        }
    }

    // --- REST through the node -------------------------------------------------------------

    /// Sends a request to the cluster as its owner (through authentication and admission, so
    /// limits apply and it wakes the cluster like any client). On a fleet it runs on the node that
    /// owns the cluster: here, or sent there (an in-process caller on any node, as apps' links).
    pub(crate) async fn call(&self, c: &SearchCluster, mut req: EsRequest) -> EsResponse {
        // Who wrote a repository's references is said only by `put_repository`, never by a
        // request relayed here (the console's headers are the caller's).
        req.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(search_es::secret_refs::MAKER_HEADER));
        self.call_as_roll(c, req).await
    }

    async fn call_as_roll(&self, c: &SearchCluster, mut req: EsRequest) -> EsResponse {
        req.headers.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("host") && !k.eq_ignore_ascii_case(search_es::remote::links::REMOTE_ACCESS_HEADER)
        });
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.name, c.password));
        req.headers.push(("Authorization".into(), format!("Basic {token}")));
        self.on_owner(&c.name, req).await
    }

    /// Runs `req` on cluster `name`'s owner node: here (waking it if it sleeps), or forwarded
    /// there signed as roll's own request.
    pub(crate) async fn on_owner(&self, name: &str, req: EsRequest) -> EsResponse {
        if self.node.get_loaded(name).is_none()
            && let Some(resp) = crate::cluster::to_owner(self, name, &req).await
        {
            return resp;
        }
        self.node.handle(req).await
    }

    /// `PUT _snapshot/{name}` on cluster `id` with `body` (Elasticsearch's), its roll secrets
    /// references (`roll://…` credential settings) written by `who`: roll secrets must let them
    /// reveal each one (else [`ControlError::Forbidden`]), and the repository keeps them as its
    /// maker, re-checked before every use ([`crate::secret_refs`]). The answer's status, content
    /// type and body, unchanged.
    pub async fn put_repository(&self, id: &str, name: &str, query: Option<&str>, body: bytes::Bytes, who: &crate::secret_refs::Who) -> Result<(u16, String, bytes::Bytes)> {
        let c = self.get(id).await?;
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ControlError::Invalid(format!("invalid body: {e}")))?;
        let ty = v["type"].as_str().unwrap_or_default();
        let settings = v["settings"].as_object();
        // Whether a setting is a reference is roll secrets' parser's to say: exactly `roll://…` is
        // one, anything written like one but not exactly is refused, never kept as a value.
        let mut refs: Vec<&str> = Vec::new();
        for k in search_es::api::snapshots::credential_settings(ty) {
            let Some(v) = settings.and_then(|s| s.get(*k)).and_then(serde_json::Value::as_str) else { continue };
            match crate::secret_refs::classify(v) {
                search_es::secret_refs::Setting::Value => {}
                search_es::secret_refs::Setting::Reference => refs.push(v),
                search_es::secret_refs::Setting::Malformed(why) => {
                    return Err(ControlError::Invalid(format!("[{k}] is written like a roll secrets reference but isn't one: {why}")));
                }
            }
        }
        let mut maker = None;
        if !refs.is_empty() {
            let (holder, secrets) = self.secret_refs.holder(&c.id, "").await.map_err(ControlError::Invalid)?;
            for r in refs {
                let by = secrets.may_write(who, &holder, r).await.map_err(|e| match e {
                    crate::secret_refs::Refused::Forbidden(m) => ControlError::Forbidden(format!("{r}: {m}")),
                    crate::secret_refs::Refused::Invalid(m) => ControlError::Invalid(format!("{r}: {m}")),
                })?;
                maker = Some(by);
            }
        }
        let uri = match query.filter(|q| !q.is_empty()) {
            Some(q) => format!("/_snapshot/{}?{q}", urlencoding::encode(name)),
            None => format!("/_snapshot/{}", urlencoding::encode(name)),
        };
        let mut req = EsRequest::json("PUT", &uri, body);
        if let Some(m) = maker {
            req.headers.push((search_es::secret_refs::MAKER_HEADER.into(), m));
        }
        let resp = self.call_as_roll(&c, req).await;
        let ct = resp.header("content-type").unwrap_or("application/json").to_string();
        Ok((resp.status, ct, resp.body))
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
        let (name, password) = self.os_user(c, "roll-link-", true, "roll: an app's link", None).await?;
        let mut u = reqwest::Url::parse(url).map_err(|e| ControlError::Internal(anyhow::anyhow!("cluster {} has no URL: {e}", c.name)))?;
        let userinfo = (u.set_username(&format!("{name}@{}", c.name)), u.set_password(Some(&password)));
        if userinfo != (Ok(()), Ok(())) {
            return Err(ControlError::Internal(anyhow::anyhow!("cluster {}'s URL takes no credentials", c.name)));
        }
        Ok(LinkCredential { vars: vec![("OPENSEARCH_URL".into(), u.to_string().trim_end_matches('/').to_string())], kept: format!("osuser:{name}") })
    }

    /// A new internal user of an OpenSearch-flavor cluster named `<prefix><random>`, with a random
    /// password: a role of its own with [`os_link_role`]'s permissions (`data`: no security REST
    /// API, the security index restricted), else `all_access`, which can use the security REST
    /// API like the cluster's own admin (an `admin` key is the cluster's password by another
    /// name, so it could also make a user without an expiry). `expires_ms`: it can't sign in from then on
    /// (`EXPIRES_ATTRIBUTE`, checked by the cluster). Returns its name and password.
    async fn os_user(&self, c: &SearchCluster, prefix: &str, data: bool, description: &str, expires_ms: Option<i64>) -> Result<(String, String)> {
        let name = format!("{prefix}{}", random_string(12));
        let password = random_string(40);
        let api = "/_plugins/_security/api";
        if data {
            let role = self.call(c, EsRequest::json("PUT", &format!("{api}/roles/{name}"), serde_json::to_vec(&os_link_role()).unwrap_or_default())).await;
            if role.status >= 300 {
                return Err(ControlError::Internal(anyhow::anyhow!("making a role of cluster {} failed ({})", c.name, role.status)));
            }
        }
        let mut user = json!({ "password": password, "opendistro_security_roles": [if data { name.as_str() } else { "all_access" }], "description": description });
        if let Some(ms) = expires_ms {
            user["attributes"] = json!({ search_es::xpack::security::opensearch::EXPIRES_ATTRIBUTE: ms.to_string() });
        }
        let resp = self.call(c, EsRequest::json("PUT", &format!("{api}/internalusers/{name}"), serde_json::to_vec(&user).unwrap_or_default())).await;
        if resp.status >= 300 {
            if data {
                let _ = self.call(c, EsRequest::new("DELETE", &format!("{api}/roles/{name}"))).await;
            }
            return Err(ControlError::Internal(anyhow::anyhow!("making a user of cluster {} failed ({})", c.name, resp.status)));
        }
        Ok((name, password))
    }

    /// Revokes what [`Self::mint_link`] made (`kept`), at once. Gone already (the key, the
    /// user, the cluster) is done. On a deleted cluster it is kept for the restore, which
    /// revokes it ([`Self::keep_revoke`]).
    pub async fn revoke_link(&self, id: &str, kept: &str) -> Result<()> {
        if !matches!(kept.split_once(':'), Some(("key" | "osuser" | "user", _))) {
            return Err(ControlError::Invalid(format!("not a link credential: {kept:?}")));
        }
        match self.get(id).await {
            Ok(c) => self.revoke_on(&c, kept).await,
            Err(ControlError::NotFound(_)) => self.keep_revoke(id, kept).await.map(drop),
            Err(e) => Err(e),
        }
    }

    /// A revoke (what [`Self::revoke_link`] takes) on deleted cluster `id`: a restore brings the
    /// cluster's security back as it was, so it is kept with the row and the restore applies it
    /// before it answers ([`Self::apply_revokes`]). `false`: `id` is no deleted cluster (purged,
    /// or never was).
    async fn keep_revoke(&self, id: &str, credential: &str) -> Result<bool> {
        if credential.len() > 512 {
            return Ok(false);
        }
        match self.store.record_revoke(id, credential, now(), MAX_KEPT_REVOKES).await? {
            Recorded::Yes => Ok(true),
            Recorded::Gone => Ok(false),
            Recorded::Full => Err(ControlError::Conflict(format!(
                "deleted search cluster {id} keeps {MAX_KEPT_REVOKES} revoked keys for its restore already: restore it, then revoke"
            ))),
            // Restored meanwhile: revoke it there. Hidden while a delete runs (a fleet's
            // `deleting`): retried, never "gone", or the restore would bring it back.
            Recorded::Live => match self.get(id).await {
                Ok(c) => self.revoke_on(&c, credential).await.map(|()| true),
                Err(ControlError::NotFound(_)) => Err(ControlError::Conflict(format!("search cluster {id} is being deleted; retry"))),
                Err(e) => Err(e),
            },
        }
    }

    /// Applies the revokes kept while cluster `id` was deleted, forgetting each once it is done
    /// (gone already counts). One that fails stays for the purge sweep to try again.
    pub(crate) async fn apply_revokes(&self, id: &str) {
        let pending = match self.store.revokes(id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(cluster = id, error = %e, "reading a restored search cluster's revokes failed");
                return;
            }
        };
        if pending.is_empty() {
            return;
        }
        let c = match self.get(id).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(cluster = id, error = %e, "revoking a restored search cluster's keys failed");
                return;
            }
        };
        for credential in pending {
            let r = match self.revoke_on(&c, &credential).await {
                Ok(()) => self.store.forget_revoke(id, &credential).await.map_err(ControlError::from),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                tracing::warn!(cluster = c.name, error = %e, "revoking a key removed while its search cluster was deleted failed; retried");
            }
        }
    }

    /// [`Self::revoke_link`] on live cluster `c`.
    async fn revoke_on(&self, c: &SearchCluster, kept: &str) -> Result<()> {
        match kept.split_once(':') {
            Some(("key", key_id)) => match self.delete_key_on(c, key_id).await {
                Ok(()) | Err(ControlError::NotFound(_)) => Ok(()),
                Err(e) => Err(e),
            },
            Some(("osuser", name)) => {
                let api = "/_plugins/_security/api";
                for path in [format!("{api}/internalusers/{name}"), format!("{api}/roles/{name}")] {
                    let r = self.call(c, EsRequest::new("DELETE", &path)).await;
                    if r.status >= 300 && r.status != 404 {
                        return Err(ControlError::Internal(anyhow::anyhow!("removing link user {name} of cluster {} failed ({})", c.name, r.status)));
                    }
                }
                Ok(())
            }
            Some(("user", user)) => {
                let r = self.call(c, EsRequest::new("DELETE", &format!("/_security/user/{}", urlencoding::encode(user)))).await;
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
        let data = match access.unwrap_or("admin") {
            "admin" => false,
            "data" => true,
            a => return Err(ControlError::Invalid(format!("access must be admin or data, not {a:?}"))),
        };
        if expires_in_s.is_some_and(|s| s <= 0) {
            return Err(ControlError::Invalid("expires_in_s must be positive".into()));
        }
        // OpenSearch clients sign in with a user: the key is an internal user of its own.
        if c.flavor == "opensearch" {
            let expires_ms = expires_in_s.map(|s| crate::meter::now_ms().saturating_add(s.saturating_mul(1000)));
            let (user, password) = self.os_user(&c, OS_KEY_PREFIX, data, &format!("roll key: {name}"), expires_ms).await?;
            let mut out = json!({ "id": user, "name": name, "access": if data { "data" } else { "admin" }, "username": format!("{user}@{}", c.name), "password": password, "url": connection_of(&self.cfg, &c.name)["url"] });
            if let Some(ms) = expires_ms {
                out["expires_at"] = json!(ms / 1000);
            }
            return Ok(out);
        }
        let mut body = json!({ "name": name });
        if data {
            body["role_descriptors"] = json!({ "roll_link": link_role() });
        }
        if let Some(s) = expires_in_s {
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
        // The access the key was made with: a client (roll secrets) checks it got what it asked.
        let mut out = json!({ "id": key_id, "name": name, "access": if data { "data" } else { "admin" }, "api_key": api_key, "url": connection_of(&self.cfg, &c.name)["url"] });
        if let Some(ms) = v["expiration"].as_i64() {
            out["expires_at"] = json!(ms / 1000);
        }
        Ok(out)
    }

    /// Revokes API key `key_id` of cluster `id` (`NotFound` when it doesn't exist or was
    /// revoked already). On a deleted cluster (by its id) it is kept for the restore, which
    /// revokes it ([`Self::keep_revoke`]): roll secrets unlinking meanwhile must not see "gone"
    /// and leave a key the restore brings back.
    pub async fn delete_key(&self, id: &str, key_id: &str) -> Result<()> {
        let c = match self.get(id).await {
            Ok(c) => c,
            Err(ControlError::NotFound(m)) => {
                let kept = match self.store.get_deleted(id).await? {
                    // Only what the route could revoke on the live cluster.
                    Some(d) if d.cluster.flavor != "opensearch" || key_id.starts_with(OS_KEY_PREFIX) => self.keep_revoke(id, &format!("key:{key_id}")).await?,
                    _ => false,
                };
                return if kept { Ok(()) } else { Err(ControlError::NotFound(m)) };
            }
            Err(e) => return Err(e),
        };
        self.delete_key_on(&c, key_id).await
    }

    async fn delete_key_on(&self, c: &SearchCluster, key_id: &str) -> Result<()> {
        if c.flavor == "opensearch" {
            // Only users the keys route made: never another internal user.
            let api = "/_plugins/_security/api";
            if !key_id.starts_with(OS_KEY_PREFIX) {
                return Err(ControlError::NotFound(format!("no API key {key_id} on cluster {}", c.name)));
            }
            let r = self.call(c, EsRequest::new("DELETE", &format!("{api}/internalusers/{}", urlencoding::encode(key_id)))).await;
            let _ = self.call(c, EsRequest::new("DELETE", &format!("{api}/roles/{}", urlencoding::encode(key_id)))).await;
            return match r.status {
                s if s < 300 => Ok(()),
                404 => Err(ControlError::NotFound(format!("no API key {key_id} on cluster {}", c.name))),
                s => Err(ControlError::Internal(anyhow::anyhow!("removing key user {key_id} of cluster {} failed ({s})", c.name))),
            };
        }
        let body = serde_json::to_vec(&json!({ "ids": [key_id] })).unwrap_or_default();
        let resp = self.call(c, EsRequest::json("DELETE", "/_security/api_key", body)).await;
        let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
        let revoked = v["invalidated_api_keys"].as_array().is_some_and(|a| a.iter().any(|k| k.as_str() == Some(key_id)));
        match (resp.status, revoked) {
            (200, true) => Ok(()),
            (200 | 404, false) => Err(ControlError::NotFound(format!("no API key {key_id} on cluster {}", c.name))),
            (s, _) => Err(ControlError::Internal(anyhow::anyhow!("revoking an API key of cluster {} failed ({s}): {v}", c.name))),
        }
    }

    /// The cluster's security audit entries matching `q`, newest first
    /// ([`search_server::audit`]). Never wakes the cluster: they are in object storage.
    pub async fn audit(&self, id: &str, q: &search_server::audit::AuditQuery) -> Result<Value> {
        let c = self.get(id).await?;
        let page = self.node.audit.read(&c.name, q).await?;
        let cfg = self.node.audit.config();
        Ok(json!({
            "flavor": c.flavor,
            "retention_days": cfg.retention.as_secs() / 86_400,
            "entries": page.entries,
            "more": page.more,
        }))
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
    /// Runs a console request (`method`, `path`, `headers` as [`ConsoleRequest`] has them) with
    /// `body` ([`ConsoleRequest::body_in`]).
    pub async fn request(&self, id: &str, method: &str, path: &str, headers: Vec<(String, String)>, body: Bytes) -> Result<ConsoleResponse> {
        let c = self.get(id).await?;
        let shown = method;
        let method = method.trim().to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH") {
            return Err(ControlError::Invalid(format!("unsupported method {shown}")));
        }
        let path = path.trim();
        if !path.starts_with('/') {
            return Err(ControlError::Invalid("path must start with /".into()));
        }
        let mut req = EsRequest::new(&method, path);
        req.headers = headers.into_iter().filter(|(k, _)| !k.trim().is_empty()).collect();
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
        // All keyed by catalog id, never by name (see `crate::meter`).
        for (id, n) in self.node.take_request_counts() {
            self.meter.count_requests(&id, n.min(i64::MAX as u64) as i64);
        }
        for (id, o) in self.node.take_observations() {
            // Lost if the catalog can't take them now: they are a picture, not a bill.
            if let Err(e) = self.meter.observe(&id, o).await {
                tracing::warn!(cluster = id, error = %e, "storing search metrics failed");
            }
        }
        let at = now_ms();
        for (id, forced) in self.meter.sample(at).await? {
            let due = forced || self.refreshed.lock().get(&id).is_none_or(|t| at - t >= self.cfg.stats_interval.as_millis() as i64);
            if !due {
                continue;
            }
            if let Err(e) = self.refresh_stats(&id, at).await {
                tracing::warn!(cluster = id, error = %e, "refreshing search cluster figures failed");
                self.meter.mark_dirty(&id);
            }
        }
        // After the analytics above are in the catalog.
        Box::pin(self.run_suggestions()).await;
        Ok(())
    }

    /// Request metrics of a cluster over `range` (`1h`, `24h` or `14d`): its buckets in the
    /// catalog plus what this node recorded since the last metering pass, in points of the
    /// range's step (only points with requests), and totals. See `docs/search/architecture.md`,
    /// "Observability".
    pub async fn metrics(&self, id: &str, range: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let Some(r) = MetricsRange::parse(range) else {
            return Err(ControlError::Invalid(format!("range must be 1h, 24h or 14d, not {range}")));
        };
        let now = now();
        let to = now - now.rem_euclid(r.step);
        let from = to - (r.points - 1) * r.step;
        let mut buckets: std::collections::BTreeMap<i64, search_server::metrics::Stats> = std::collections::BTreeMap::new();
        let mut add = |at: i64, s: &search_server::metrics::Stats| {
            if at >= from {
                buckets.entry(at - at.rem_euclid(r.step)).or_default().merge(s);
            }
        };
        for (at, s) in self.store.metrics_since(&c.id, r.res, from).await? {
            add(at, &s);
        }
        // Not flushed yet: what the cluster's owner (here) recorded since the last pass.
        for (minute, s) in self.node.peek_observations(&c.name, &c.id).minutes {
            add(minute.min(i64::MAX as u64 / 60) as i64 * 60, &s);
        }
        let mut total = search_server::metrics::Stats::default();
        let points: Vec<Value> = buckets
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(at, s)| {
                total.merge(s);
                point_json(*at, s, r.step)
            })
            .collect();
        Ok(json!({
            "range": range,
            "step_s": r.step,
            "from": from,
            "to": to + r.step,
            "points": points,
            "totals": point_json(from, &total, r.points * r.step),
        }))
    }

    /// A cluster's slow log, newest first: the catalog's entries and those this node hasn't
    /// handed on yet, at most [`search_es::slowlog::MAX_ENTRIES`].
    pub async fn slowlog(&self, id: &str) -> Result<Value> {
        let c = self.get(id).await?;
        let mut entries: Vec<Value> = self.node.peek_observations(&c.name, &c.id).slow.iter().rev().map(search_es::slowlog::Entry::to_json).collect();
        for row in self.store.slowlog(&c.id).await? {
            if entries.len() >= search_es::slowlog::MAX_ENTRIES {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&row) {
                entries.push(v);
            }
        }
        Ok(json!({ "entries": entries, "max_entries": search_es::slowlog::MAX_ENTRIES }))
    }

    /// Re-reads the docs and stored bytes of the cluster with id `id` from its commit points.
    pub async fn refresh_stats(&self, id: &str, at_ms: i64) -> anyhow::Result<()> {
        let Some(c) = self.store.get(id).await? else { return Ok(()) };
        let name = c.name.as_str();
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
            None => match self.meter.take_indices(id) {
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
            refreshed.insert(id.to_string(), at_ms);
        } else {
            refreshed.remove(id);
        }
        Ok(())
    }
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
        let body = |b: Option<&str>| {
            let whole = Bytes::from(match b {
                Some(b) => format!(r#"{{"method":"PUT","path":"/a","body":{b}}}"#),
                None => r#"{"method":"GET","path":"/"}"#.to_string(),
            });
            let r = ConsoleRequest::parse(&whole).unwrap();
            let sent = r.body_in(&whole).unwrap();
            // A JSON value is a slice of what was read, not a copy.
            if b.is_some_and(|b| !b.starts_with('"') && b != "null") {
                assert!(whole.as_ptr_range().contains(&sent.as_ptr()));
            }
            sent
        };
        assert_eq!(body(None), Bytes::new());
        assert_eq!(body(Some("null")), Bytes::new());
        // Key order, spacing and number spelling survive (serde_json's Value would sort keys).
        assert_eq!(body(Some(r#"{"z": 1.0, "a": [1e2]}"#)), Bytes::from_static(br#"{"z": 1.0, "a": [1e2]}"#));
        assert_eq!(body(Some(r#""{\"index\":{}}\n{}\n""#)), Bytes::from_static(b"{\"index\":{}}\n{}\n"));
        assert_eq!(body(Some("42")), Bytes::from_static(b"42"));
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
            wake_at: None,
            owner: None,
            restored_at: None,
            ip_allowlist: vec![IpRule { cidr: "203.0.113.0/24".into(), description: None }],
            cors: None,
            browser_per_minute: None,
            inherit_pending: false,
            reindex_whitelist: vec!["es.example.com:443".into()],
        };
        let s = spec_for(&c);
        assert!(s.ip_allowlist.contains("::ffff:203.0.113.9".parse().unwrap()) && !s.ip_allowlist.is_open());
        assert_eq!(s.reindex_whitelist, Some(vec!["es.example.com:443".to_string()]));
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
