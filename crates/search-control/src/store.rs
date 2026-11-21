//! Control-plane state for roll search, through `code-sql`: SQLite (`search.db`, one node) or
//! the system database's Postgres (schema `search`) that every node of a fleet shares. SQL is
//! written once, in SQLite's dialect.
//!
//! - `clusters`: one row per cluster (name, dialect, password, limits, branch origin) plus the
//!   last figures metering saw (indices, documents, stored bytes), so listing clusters never
//!   wakes them or touches object storage. A cluster being deleted keeps its row, flagged
//!   `deleting`, until its data is gone; lookups don't see it.
//!   A deleted cluster kept for restoring (`--search-deleted-retention-days`) keeps its row too,
//!   with `deleted_at`, `purge_at`, the name it had (`deleted_name`) and the name its storage is
//!   kept under (`stored_as`, also in `name`, so its own name is free at once). Lookups don't see
//!   it either. Restoring and purging claim the row first (`claim`, a conditional update in one
//!   transaction), so two nodes never restore and purge the same cluster at once; a restore
//!   reserves the name it restores to in `name` while it runs. On a fleet every change to a
//!   cluster is made on its owner (`crate::cluster::on_owner`), which applies it to the loaded
//!   cluster itself: no node has to follow the catalog.
//!   A cluster has an `owner`: the signed-in person (`usr_…`) who made it, or none for the
//!   operator's and admins' (and every cluster made before owners). A branch keeps its parent's.
//! - `usage`: one row per metering interval of a *loaded* cluster: loaded seconds, the
//!   estimated resident bytes, requests. An idle cluster adds no rows.
//! - `storage`: a change log of each cluster's stored bytes. Storage accrues while a cluster
//!   sleeps too, so instead of a row per cluster per minute the log gets a row only when the
//!   size changes, and GB-months are the integral of that step function over the window.
//! - `metrics`: request metrics ([`search_server::metrics::Stats`]) in two fixed rings per
//!   cluster, a row per slot: [`MINUTE_SLOTS`] one-minute buckets (24 hours) and [`HOUR_SLOTS`]
//!   one-hour buckets (14 days). A slot is rewritten when its time comes round again, so a
//!   cluster never has more than `MINUTE_SLOTS + HOUR_SLOTS` rows, and a cluster that served no
//!   request in a minute writes nothing for it.
//! - `slowlog`: the newest [`search_es::slowlog::MAX_ENTRIES`] slow log entries per cluster, as
//!   Elasticsearch's JSON lines.
//! - `analytics`: search analytics ([`search_es::analytics::Bucket`]) of the indices that turned
//!   them on, in two fixed rings per index, a row per slot: [`ANALYTICS_HOUR_SLOTS`] one-hour
//!   buckets (7 days) and [`ANALYTICS_DAY_SLOTS`] one-day buckets (30 days), each a bounded
//!   list of top queries and of queries with no results. A slot is replaced when its time comes
//!   round again; an index whose analytics go off or that is deleted loses its rows.
//!
//! A cluster's password is sealed in its row under the deployment's key ([`Store::seal_passwords`]:
//! rolld's `ROLL_SECRET_KEY`), bound to the cluster's id, and opened as the row is read. A row
//! still holding one in the clear (written before sealing) is sealed as it's read. Without a key
//! (local development) passwords are stored as given, like every secret search keeps.
//!
//! The schema is created with `CREATE … IF NOT EXISTS` (`Db::ensure_schema`: on Postgres under
//! an advisory lock, since nodes start together); it reads the `search.db` files the earlier
//! catalog wrote.
//!
//! Calls are async for their callers; `code-sql` runs them synchronously (SQLite inline, in
//! microseconds; Postgres on a runtime of its own).

use code_sql::{Db, OptionalExtension, Row, params};
use search_es::seal::{Sealer, is_sealed};
use search_server::metrics::Stats;
use serde::{Deserialize, Serialize};

use std::path::Path;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Serialize)]
pub struct SearchCluster {
    pub id: String,
    /// The tenant's name on the node: the Basic auth user, the host label under the node's
    /// domain, `GET /` `cluster_name`, and its key in object storage. Immutable.
    pub name: String,
    pub region: String,
    /// `elasticsearch` or `opensearch`: the REST dialect the cluster speaks.
    pub flavor: String,
    #[serde(skip)]
    pub password: String,
    /// Seconds without requests before the cluster is unloaded (scale to zero).
    pub idle_timeout_s: i64,
    /// Requests running at once (as many more may queue; beyond that, 429).
    pub max_in_flight: i64,
    /// `http.max_content_length`, in MiB.
    pub max_body_mb: i64,
    /// Default of `action.destructive_requires_name`.
    pub destructive_requires_name: bool,
    pub parent_id: Option<String>,
    /// Unix seconds of the parent's state the branch started from.
    pub parent_at: Option<i64>,
    pub created_at: i64,
    pub password_rotated_at: Option<i64>,
    /// What metering last saw (it refreshes them while the cluster is loaded and once after it
    /// unloads).
    pub indices: i64,
    pub docs: i64,
    pub stored_bytes: i64,
    /// Of `stored_bytes`, what a branch's commit points reference that its parent's latest
    /// don't: what the branch adds (all of it for a cluster that isn't a branch).
    pub own_bytes: i64,
    /// On a fleet, the node it's loaded on (set as it loads, cleared as it unloads there): what
    /// lists show without asking the cluster's lease.
    #[serde(skip)]
    pub loaded_on: Option<String>,
    /// When a sleeping cluster's next background job is due (ms; its node wakes it then).
    #[serde(skip)]
    pub wake_at: Option<i64>,
    /// The signed-in person (`usr_…`) whose cluster it is; `None` for the operator's and admins'.
    /// Set on create, kept by branches, never changed.
    pub owner: Option<String>,
    /// When it was last restored after a delete (Unix seconds): its point-in-time history
    /// starts again there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_at: Option<i64>,
    /// Who may reach its data plane (Elastic Cloud's traffic filters): canonical CIDRs, each with
    /// an optional description; empty: anyone. Branches start with their parent's.
    pub ip_allowlist: Vec<IpRule>,
    /// Which web pages may call it from a browser (`http.cors.*`, checked after the
    /// allowlist); `None`: CORS off. Branches start with their parent's.
    pub cors: Option<CorsSettings>,
    /// Requests per minute all its browser keys may make together (`None`: the node's default,
    /// `search_server::browser::DEFAULT_CLUSTER_PER_MINUTE`).
    pub browser_per_minute: Option<i64>,
    /// A new branch whose node hasn't yet marked what it inherited from its parent
    /// (`search_es::branch`): set when the branch is made, cleared once a load marked it.
    /// Branches made before marking existed never have it, so their own jobs are left alone.
    #[serde(skip)]
    pub inherit_pending: bool,
}

impl std::fmt::Debug for SearchCluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchCluster")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("region", &self.region)
            .field("flavor", &self.flavor)
            .field("password", &"<redacted>")
            .field("idle_timeout_s", &self.idle_timeout_s)
            .field("max_in_flight", &self.max_in_flight)
            .field("max_body_mb", &self.max_body_mb)
            .field("destructive_requires_name", &self.destructive_requires_name)
            .field("parent_id", &self.parent_id)
            .field("parent_at", &self.parent_at)
            .field("created_at", &self.created_at)
            .field("password_rotated_at", &self.password_rotated_at)
            .field("indices", &self.indices)
            .field("docs", &self.docs)
            .field("stored_bytes", &self.stored_bytes)
            .field("own_bytes", &self.own_bytes)
            .field("loaded_on", &self.loaded_on)
            .field("wake_at", &self.wake_at)
            .field("owner", &self.owner)
            .field("restored_at", &self.restored_at)
            .field("ip_allowlist", &self.ip_allowlist)
            .field("cors", &self.cors)
            .field("browser_per_minute", &self.browser_per_minute)
            .finish()
    }
}

/// A cluster's `http.cors.*` (docs/search/README.md, "Search from the browser"): as checked by
/// `search_server::cors::Cors::new`, which keeps origins as browsers send them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsSettings {
    /// `scheme://host[:port]`, `/regex/` or `*`.
    pub allow_origin: Vec<String>,
    /// `http.cors.allow-methods` (`None`: Elasticsearch's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_methods: Option<Vec<String>>,
    /// `http.cors.allow-headers` (`None`: Elasticsearch's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_headers: Option<Vec<String>>,
    /// `http.cors.max-age`, in seconds (`None`: Elasticsearch's 1728000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<i64>,
}

/// A remote cluster's credentials: a `cross_cluster` API key of cluster `target_id` that
/// cluster `cluster_id` reaches it with as its remote `alias` (`cluster.remote.<alias>.credentials`).
#[derive(Clone, Serialize)]
pub struct RemoteLink {
    #[serde(skip)]
    pub cluster_id: String,
    pub alias: String,
    pub target_id: String,
    /// The key's id on the target (never its secret).
    pub key_id: String,
    /// The encoded key, sealed under the deployment's key (as given without one). Never returned.
    #[serde(skip)]
    pub credentials: String,
    /// roll made the key (revoked when the link goes); else the person gave it.
    pub minted: bool,
    pub created_at: i64,
}

impl std::fmt::Debug for RemoteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteLink")
            .field("cluster_id", &self.cluster_id)
            .field("alias", &self.alias)
            .field("target_id", &self.target_id)
            .field("key_id", &self.key_id)
            .field("credentials", &"<redacted>")
            .field("minted", &self.minted)
            .field("created_at", &self.created_at)
            .finish()
    }
}

const REMOTE_COLS: &str = "cluster_id, alias, target_id, key_id, credentials, minted, created_at";

fn remote_row(r: &Row) -> code_sql::Result<RemoteLink> {
    Ok(RemoteLink { cluster_id: r.get(0)?, alias: r.get(1)?, target_id: r.get(2)?, key_id: r.get(3)?, credentials: r.get(4)?, minted: r.get(5)?, created_at: r.get(6)? })
}

/// One entry of a cluster's IP allowlist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpRule {
    /// Canonical: `203.0.113.7/32`, `10.0.0.0/8`, `2001:db8::/32`.
    pub cidr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One metering interval of a loaded cluster.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSample {
    pub at: i64,
    /// Seconds the cluster was loaded during the interval.
    pub seconds: i64,
    /// Estimated resident bytes while loaded.
    pub loaded_bytes: i64,
    pub requests: i64,
    /// CPU milliseconds the node's embedding pool spent on the cluster's inputs.
    pub inference_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageTotals {
    /// Seconds the cluster was loaded (in memory).
    pub loaded_seconds: i64,
    /// Integral of the resident size over loaded time.
    pub loaded_gib_hours: f64,
    pub requests: i64,
    pub peak_loaded_bytes: i64,
    /// Integral of the stored size over the window.
    pub stored_gib_months: f64,
    /// CPU seconds the node's embedding pool spent on the cluster's inputs (E5).
    pub inference_cpu_seconds: f64,
}

/// A deleted cluster kept for restoring until `purge_at`.
#[derive(Clone, Debug, Serialize)]
pub struct DeletedCluster {
    /// As it was, under the name it had.
    #[serde(flatten)]
    pub cluster: SearchCluster,
    /// Unix seconds.
    pub deleted_at: i64,
    /// Unix seconds: when its storage is removed for good.
    pub purge_at: i64,
    /// The name its storage is kept under in object storage (never a valid cluster name).
    #[serde(skip)]
    pub stored_as: String,
    /// What the catalog's `name` holds now: `stored_as`, or the name a restore reserved.
    #[serde(skip)]
    pub reserved: String,
}

/// What [`Store::claim_deleted`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Claimed. `reserved` is what the row's `name` held before (a crashed restore's target).
    Claimed { reserved: String },
    /// No such deleted cluster (restored, purged, or never deleted).
    Gone,
    /// Another restore or purge holds it.
    Busy,
    /// A restore's name is taken by another cluster.
    Taken,
}

/// What lookups of live clusters filter on.
const LIVE: &str = "deleting = 0 AND deleted_at IS NULL";

pub struct Store {
    db: Db,
    /// Seals clusters' passwords in their rows ([`Store::seal_passwords`]); unset: as given.
    sealer: OnceLock<Arc<Sealer>>,
}

/// What a cluster's sealed password is bound to: its row, so a password can't be moved into
/// another cluster's.
fn password_kind(id: &str) -> String {
    format!("search_cluster_password:{id}")
}

/// Per-person limits a write checks in its own transaction (`0`: no cap). Only clusters with an
/// owner count, and only toward that owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// Clusters a person may own, branches included.
    pub clusters: i64,
    /// Of those, ones that never unload (`idle_timeout_s` 0).
    pub always_on: i64,
}

impl Caps {
    /// No caps: the operator's and admins' writes.
    pub const NONE: Caps = Caps { clusters: 0, always_on: 0 };
}

/// A write refused by a [`Caps`] limit: the cap, and how many the owner already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Over {
    Clusters { cap: i64, count: i64 },
    AlwaysOn { cap: i64, count: i64 },
}

/// What [`Store::insert`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
    Yes,
    /// The name (or id) is taken.
    Taken,
    Over(Over),
}

/// What [`Store::record_revoke`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Recorded {
    /// Kept with the deleted cluster, for its restore.
    Yes,
    /// The cluster is live (restored meanwhile): revoke it on the cluster.
    Live,
    /// No such cluster any more (purged, or never).
    Gone,
    /// The cluster keeps as many as it may.
    Full,
}

/// The clusters a person owns, as [`Store::list_owned`] lists them.
const OWNED: &str = "deleting = 0 AND deleted_at IS NULL AND owner = ?1";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS search_clusters (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
  password TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
  max_body_mb INTEGER NOT NULL, destructive_requires_name INTEGER NOT NULL,
  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL, password_rotated_at INTEGER,
  indices INTEGER NOT NULL DEFAULT 0, docs INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0,
  deleting INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS search_clusters_parent ON search_clusters (parent_id);
CREATE INDEX IF NOT EXISTS search_clusters_always_on ON search_clusters (created_at) WHERE idle_timeout_s = 0;
CREATE TABLE IF NOT EXISTS search_usage (
  cluster_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, loaded_bytes INTEGER NOT NULL,
  requests INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS search_usage_cluster ON search_usage (cluster_id, at);
CREATE TABLE IF NOT EXISTS search_storage (cluster_id TEXT NOT NULL, at INTEGER NOT NULL, bytes INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS search_storage_cluster ON search_storage (cluster_id, at);
CREATE TABLE IF NOT EXISTS search_metrics (
  cluster_id TEXT NOT NULL, res INTEGER NOT NULL, slot INTEGER NOT NULL, at INTEGER NOT NULL, data TEXT NOT NULL,
  PRIMARY KEY (cluster_id, res, slot)
);
CREATE TABLE IF NOT EXISTS search_slowlog (cluster_id TEXT NOT NULL, at INTEGER NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS search_slowlog_cluster ON search_slowlog (cluster_id, at);
CREATE TABLE IF NOT EXISTS search_analytics (
  cluster_id TEXT NOT NULL, idx TEXT NOT NULL, res INTEGER NOT NULL, slot INTEGER NOT NULL, at INTEGER NOT NULL, data TEXT NOT NULL,
  PRIMARY KEY (cluster_id, idx, res, slot)
);
CREATE TABLE IF NOT EXISTS search_suggestions (
  cluster_id TEXT NOT NULL, idx TEXT NOT NULL, config TEXT NOT NULL, due_at INTEGER NOT NULL,
  attempted_at INTEGER, built_at INTEGER, built_index TEXT, count INTEGER NOT NULL DEFAULT 0, took_ms INTEGER NOT NULL DEFAULT 0,
  digest TEXT, error TEXT, sample TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (cluster_id, idx)
);
CREATE INDEX IF NOT EXISTS search_suggestions_due ON search_suggestions (due_at);
"#;

const COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                    parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes, owner, ip_allowlist, cors, browser_per_minute, inherit_pending";
/// What lookups read: [`COLS`] and where the cluster is loaded.
const READ_COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                         parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes, owner, loaded_on, own_bytes, wake_at, restored_at, ip_allowlist, cors, browser_per_minute, inherit_pending";
/// What deleted clusters' reads add to [`READ_COLS`].
const DELETED_COLS: &str = "deleted_name, deleted_at, purge_at, stored_as, name";

fn row(r: &Row) -> code_sql::Result<SearchCluster> {
    Ok(SearchCluster {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        flavor: r.get(3)?,
        password: r.get(4)?,
        idle_timeout_s: r.get(5)?,
        max_in_flight: r.get(6)?,
        max_body_mb: r.get(7)?,
        destructive_requires_name: r.get(8)?,
        parent_id: r.get(9)?,
        parent_at: r.get(10)?,
        created_at: r.get(11)?,
        password_rotated_at: r.get(12)?,
        indices: r.get(13)?,
        docs: r.get(14)?,
        stored_bytes: r.get(15)?,
        owner: r.get(16)?,
        loaded_on: r.get(17)?,
        own_bytes: r.get(18)?,
        wake_at: r.get(19)?,
        restored_at: r.get(20)?,
        // Unreadable, the cluster can't be looked up (and so can't be reached), never opened up.
        ip_allowlist: serde_json::from_str(&r.get::<_, String>(21)?).map_err(|e| code_sql::Error::Conversion(format!("ip_allowlist: {e}")))?,
        cors: match r.get::<_, Option<String>>(22)? {
            Some(j) => Some(serde_json::from_str(&j).map_err(|e| code_sql::Error::Conversion(format!("cors: {e}")))?),
            None => None,
        },
        browser_per_minute: r.get(23)?,
        inherit_pending: r.get(24)?,
    })
}

fn deleted_row(r: &Row) -> code_sql::Result<DeletedCluster> {
    let mut c = row(r)?;
    c.name = r.get(25)?;
    c.loaded_on = None;
    c.wake_at = None;
    Ok(DeletedCluster { cluster: c, deleted_at: r.get(26)?, purge_at: r.get(27)?, stored_as: r.get(28)?, reserved: r.get(29)? })
}

/// What the catalog keeps of an index's query suggestions (`crate::suggestions`): its settings
/// as of the last look, when it's next due, and its last build.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SuggestionsRow {
    pub index: String,
    /// `search_es::query_suggestions::Config::to_json`.
    pub config: String,
    /// Milliseconds.
    pub due_at: i64,
    pub attempted_at: Option<i64>,
    pub built_at: Option<i64>,
    /// The concrete index the alias points at.
    pub built_index: Option<String>,
    pub count: i64,
    pub took_ms: i64,
    pub digest: Option<String>,
    /// Why the last attempt failed.
    pub error: Option<String>,
    /// The heaviest suggestions of the last build, as JSON (what the dashboard shows without
    /// waking the cluster).
    pub sample: String,
}

const SUGGESTION_COLS: &str = "idx, config, due_at, attempted_at, built_at, built_index, count, took_ms, digest, error, sample";

fn suggestions_row(r: &Row) -> code_sql::Result<SuggestionsRow> {
    suggestions_row_at(r, 0)
}

fn suggestions_row_at(r: &Row, o: usize) -> code_sql::Result<SuggestionsRow> {
    Ok(SuggestionsRow {
        index: r.get(o)?,
        config: r.get(o + 1)?,
        due_at: r.get(o + 2)?,
        attempted_at: r.get(o + 3)?,
        built_at: r.get(o + 4)?,
        built_index: r.get(o + 5)?,
        count: r.get(o + 6)?,
        took_ms: r.get(o + 7)?,
        digest: r.get(o + 8)?,
        error: r.get(o + 9)?,
        sample: r.get(o + 10)?,
    })
}

/// One-minute buckets a cluster keeps: 24 hours.
pub const MINUTE_SLOTS: i64 = 1440;
/// One-hour buckets a cluster keeps: 14 days.
pub const HOUR_SLOTS: i64 = 336;

fn cors_json(cors: &Option<CorsSettings>) -> Option<String> {
    cors.as_ref().map(|c| serde_json::to_string(c).unwrap_or_else(|_| "null".into()))
}

/// One-hour analytics buckets an index keeps: 7 days.
pub const ANALYTICS_HOUR_SLOTS: i64 = 168;
/// One-day analytics buckets an index keeps: 30 days and the day in progress.
pub const ANALYTICS_DAY_SLOTS: i64 = 31;
/// Queries (and queries with no results) an hour's bucket keeps.
pub const ANALYTICS_HOUR_TOP: (usize, usize) = (100, 50);
/// Queries (and queries with no results) a day's bucket keeps.
pub const ANALYTICS_DAY_TOP: (usize, usize) = (300, 100);

/// The list sizes a bucket of `res` seconds keeps.
pub fn analytics_caps(res: i64) -> (usize, usize) {
    if res >= 86_400 { ANALYTICS_DAY_TOP } else { ANALYTICS_HOUR_TOP }
}

fn allowlist_json(rules: &[IpRule]) -> String {
    serde_json::to_string(rules).expect("rules serialize")
}

/// Seconds in the 30-day month the GB-month figures use (AWS bills storage per 730-hour month;
/// a 30-day month keeps `days / 30` exact in the dashboard).
pub const MONTH_SECONDS: f64 = 30.0 * 86_400.0;
const GIB: f64 = (1u64 << 30) as f64;

impl Store {
    /// `search.db` at `path` (a fresh Postgres schema instead when `code-sql`'s test database
    /// variable is set, so the tests run on both).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::with_db(Db::open_or_test(&code_sql::Target::Sqlite(path.to_path_buf()), "search")?)
    }

    /// A catalog of its own in a temporary file (tests; nodes of one process share it).
    pub fn memory() -> anyhow::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("roll-search-catalog-{}-{}-{n}", std::process::id(), crate::meter::now_ms()));
        std::fs::create_dir_all(&dir)?;
        Self::open(&dir.join("search.db"))
    }

    /// The catalog every node of a fleet shares: schema `search` of the system database.
    pub fn postgres(url: &str, opts: code_sql::PgOptions) -> anyhow::Result<Self> {
        Self::with_db(Db::connect(url, "search", opts)?)
    }

    fn with_db(db: Db) -> anyhow::Result<Self> {
        db.ensure_schema(SCHEMA)?;
        db.add_column("search_clusters", "loaded_on", "TEXT")?;
        // Intervals before inference was metered had none.
        db.add_column("search_usage", "inference_ms", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("search_clusters", "own_bytes", "INTEGER NOT NULL DEFAULT 0")?;
        db.add_column("search_clusters", "wake_at", "INTEGER")?;
        // Clusters made before owners stay the operator's (none).
        db.add_column("search_clusters", "owner", "TEXT")?;
        // Clusters made before allowlists are open, as they were.
        db.add_column("search_clusters", "ip_allowlist", "TEXT NOT NULL DEFAULT '[]'")?;
        // Clusters made before browser search have CORS off, as Elasticsearch does.
        db.add_column("search_clusters", "cors", "TEXT")?;
        db.add_column("search_clusters", "browser_per_minute", "INTEGER")?;
        // Branches made before marking aren't marked (`SearchCluster::inherit_pending`).
        db.add_column("search_clusters", "inherit_pending", "INTEGER NOT NULL DEFAULT 0")?;
        db.ensure_schema("CREATE INDEX IF NOT EXISTS search_clusters_owner ON search_clusters (owner, created_at)")?;
        // Deleted clusters kept for restoring.
        db.add_column("search_clusters", "deleted_at", "INTEGER")?;
        db.add_column("search_clusters", "purge_at", "INTEGER")?;
        db.add_column("search_clusters", "deleted_name", "TEXT")?;
        db.add_column("search_clusters", "stored_as", "TEXT")?;
        db.add_column("search_clusters", "claim", "TEXT")?;
        db.add_column("search_clusters", "claimed_at", "INTEGER")?;
        db.add_column("search_clusters", "restored_at", "INTEGER")?;
        db.ensure_schema("CREATE INDEX IF NOT EXISTS search_clusters_purge ON search_clusters (purge_at) WHERE deleted_at IS NOT NULL")?;
        // Credentials revoked while their cluster was deleted, for its restore to revoke.
        db.ensure_schema("CREATE TABLE IF NOT EXISTS search_revokes (cluster_id TEXT NOT NULL, credential TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (cluster_id, credential))")?;
        // Remote clusters' credentials (`cluster.remote.<alias>.credentials`): a cross-cluster
        // API key of `target_id`, sealed, for cluster `cluster_id`'s remote `alias`.
        db.ensure_schema(
            "CREATE TABLE IF NOT EXISTS search_remotes (cluster_id TEXT NOT NULL, alias TEXT NOT NULL, target_id TEXT NOT NULL, key_id TEXT NOT NULL, credentials TEXT NOT NULL, minted INTEGER NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (cluster_id, alias))",
        )?;
        // Catalogs written before `code-sql` lack nothing but may lack the index on parents
        // (created above); their extra columns (`seq`) and tables (`search_tombstones`) are unused.
        Ok(Self { db, sealer: OnceLock::new() })
    }

    /// Seals clusters' passwords under the deployment's key from now on (rolld:
    /// `ROLL_SECRET_KEY`), each bound to its cluster's id; rows written in the clear before are
    /// sealed as they're read. The first sealer given stays.
    pub fn seal_passwords(&self, sealer: Arc<Sealer>) {
        let _ = self.sealer.set(sealer);
    }

    /// `c`'s password as its row keeps it.
    fn stored_password(&self, c: &SearchCluster) -> String {
        match self.sealer.get() {
            Some(s) => s.seal_text(&password_kind(&c.id), &c.password),
            None => c.password.clone(),
        }
    }

    /// Opens the passwords of clusters just read. A row still holding its password in the clear
    /// is sealed in one write, only where it still holds what was read (a rotation since keeps
    /// its own). A sealed password this node can't open (no key, another key) fails the read,
    /// naming the cluster, never the value.
    fn reveal<'a>(&self, clusters: impl IntoIterator<Item = &'a mut SearchCluster>) -> anyhow::Result<()> {
        let sealer = self.sealer.get();
        let mut legacy = Vec::new();
        for c in clusters {
            if is_sealed(&c.password) {
                let Some(plain) = sealer.and_then(|s| s.open_text(&password_kind(&c.id), &c.password)) else {
                    anyhow::bail!("cluster {}'s password is sealed under another key than this node's (ROLL_SECRET_KEY unset or changed)", c.id);
                };
                c.password = plain;
            } else if let Some(s) = sealer {
                legacy.push((c.id.clone(), s.seal_text(&password_kind(&c.id), &c.password), c.password.clone()));
            }
        }
        if !legacy.is_empty() {
            let r = self.db.write(|x| {
                for (id, sealed, plain) in &legacy {
                    x.execute("UPDATE search_clusters SET password = ?2 WHERE id = ?1 AND password = ?3", params![id, sealed, plain])?;
                }
                Ok::<_, code_sql::Error>(())
            });
            if let Err(e) = r {
                tracing::warn!(clusters = legacy.len(), error = %e, "sealing clusters' passwords failed: they stay in the clear until read again");
            }
        }
        Ok(())
    }

    /// Inserts a new cluster, unless its name (or id) is taken or its owner is at a cap. The
    /// owner's clusters are counted in the insert's own transaction (SQLite's one writer;
    /// Postgres `SERIALIZABLE`, which re-runs one of two racing creates), so two creates at the
    /// cap can't both pass.
    pub async fn insert(&self, c: &SearchCluster, caps: Caps) -> anyhow::Result<Inserted> {
        let password = self.stored_password(c);
        let r = self.db.write(|x| {
            if let Some(owner) = &c.owner
                && let Some(over) = over_caps(x, owner, c.idle_timeout_s, caps)?
            {
                return Ok(Inserted::Over(over));
            }
            x.execute(
                &format!("INSERT INTO search_clusters ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)"),
                params![
                    c.id,
                    c.name,
                    c.region,
                    c.flavor,
                    password,
                    c.idle_timeout_s,
                    c.max_in_flight,
                    c.max_body_mb,
                    c.destructive_requires_name,
                    c.parent_id,
                    c.parent_at,
                    c.created_at,
                    c.password_rotated_at,
                    c.indices,
                    c.docs,
                    c.stored_bytes,
                    c.owner,
                    allowlist_json(&c.ip_allowlist),
                    cors_json(&c.cors),
                    c.browser_per_minute,
                    c.inherit_pending
                ],
            )
            .map(|_| Inserted::Yes)
        });
        match r {
            Err(e) if e.is_unique_violation() => Ok(Inserted::Taken),
            r => Ok(r?),
        }
    }

    /// Whether one more cluster of `owner` (`idle_timeout_s`: its setting) is past one of `caps`,
    /// as [`Store::insert`] counts it. Only a quick refusal before work an insert would undo: the
    /// insert's own count decides.
    pub async fn check_caps(&self, owner: &str, idle_timeout_s: i64, caps: Caps) -> anyhow::Result<Option<Over>> {
        Ok(self.db.read(|x| over_caps(x, owner, idle_timeout_s, caps))?)
    }

    /// Saves the settings and the password (not the metering figures: see [`Store::set_stats`]).
    /// Making an owned cluster always-on checks `caps.always_on` in the same transaction.
    pub async fn update(&self, c: &SearchCluster, caps: Caps) -> anyhow::Result<Result<(), Over>> {
        let password = self.stored_password(c);
        Ok(self.db.write(|x| {
            if let Some(owner) = &c.owner
                && let Some(over) = over_always_on(x, owner, Some(&c.id), c.idle_timeout_s, caps)?
            {
                return Ok(Err(over));
            }
            x.execute(
                "UPDATE search_clusters SET flavor = ?2, password = ?3, idle_timeout_s = ?4, max_in_flight = ?5, max_body_mb = ?6,
                 destructive_requires_name = ?7, password_rotated_at = ?8, ip_allowlist = ?9, cors = ?10, browser_per_minute = ?11 WHERE id = ?1",
                params![
                    c.id,
                    c.flavor,
                    password,
                    c.idle_timeout_s,
                    c.max_in_flight,
                    c.max_body_mb,
                    c.destructive_requires_name,
                    c.password_rotated_at,
                    allowlist_json(&c.ip_allowlist),
                    cors_json(&c.cors),
                    c.browser_per_minute
                ],
            )?;
            Ok::<_, code_sql::Error>(Ok(()))
        })?)
    }

    /// A cluster's id from its id or name, synchronously (roll's access rules resolve resources
    /// on blocking threads). A deleted cluster kept for restoring resolves by its id only (its
    /// restore route names it).
    pub fn id_of(&self, id_or_name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.db.read(|x| {
            x.query_row(&format!("SELECT id FROM search_clusters WHERE (id = ?1 AND deleting = 0) OR (name = ?1 AND {LIVE}) ORDER BY id = ?1 DESC LIMIT 1"), params![id_or_name], |r| r.get(0)).optional()
        })?)
    }

    /// Whose cluster `id_or_name` is, synchronously (rolld's workspace owners): `None` when there
    /// is no such cluster, `Some(None)` for the operator's.
    pub fn owner_of(&self, id_or_name: &str) -> anyhow::Result<Option<Option<String>>> {
        Ok(self.db.read(|x| {
            x.query_row(&format!("SELECT owner FROM search_clusters WHERE (id = ?1 OR name = ?1) AND {LIVE} ORDER BY id = ?1 DESC LIMIT 1"), params![id_or_name], |r| r.get(0)).optional()
        })?)
    }

    async fn one(&self, filter: &str, arg: &str) -> anyhow::Result<Option<SearchCluster>> {
        let mut c = self.db.read(|x| x.query_row(&format!("SELECT {READ_COLS} FROM search_clusters WHERE {filter} = ?1 AND {LIVE}"), params![arg], row).optional())?;
        self.reveal(c.iter_mut())?;
        Ok(c)
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("id", id).await
    }

    /// The cluster named `name`, unless it is being deleted.
    pub async fn by_name(&self, name: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("name", name).await
    }

    async fn many(&self, filter: &str, arg: Option<&str>) -> anyhow::Result<Vec<SearchCluster>> {
        let sql = format!("SELECT {READ_COLS} FROM search_clusters WHERE {LIVE} {filter} ORDER BY created_at, rowid");
        let mut v = self.db.read(|x| match arg {
            Some(a) => x.query(&sql, params![a], row),
            None => x.query(&sql, params![], row),
        })?;
        self.reveal(v.iter_mut())?;
        Ok(v)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("", None).await
    }

    /// The clusters of the signed-in person `owner` (what [`Caps`] counts).
    pub async fn list_owned(&self, owner: &str) -> anyhow::Result<Vec<SearchCluster>> {
        self.many(&format!("AND {OWNED}"), Some(owner)).await
    }

    /// Clusters that never unload (`idle_timeout_s` 0), through a partial index: what every node
    /// of a fleet polls, so it stays cheap however large the catalog grows.
    pub async fn always_on(&self) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("AND idle_timeout_s = 0", None).await
    }

    /// Clusters branched from `id`.
    pub async fn children(&self, id: &str) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("AND parent_id = ?1", Some(id)).await
    }

    /// Records that cluster `name` is loaded on `node` (`Some`), or no longer there (`None`:
    /// cleared only if it still names `me`, so a late unload never erases the next owner's).
    pub async fn set_loaded_on(&self, name: &str, node: Option<&str>, me: &str) -> anyhow::Result<()> {
        self.db.write(|x| match node {
            Some(n) => x.execute("UPDATE search_clusters SET loaded_on = ?2 WHERE name = ?1", params![name, n]),
            None => x.execute("UPDATE search_clusters SET loaded_on = NULL WHERE name = ?1 AND loaded_on = ?2", params![name, me]),
        })?;
        Ok(())
    }

    /// Sets or clears [`SearchCluster::inherit_pending`].
    pub async fn set_inherit_pending(&self, name: &str, pending: bool) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET inherit_pending = ?2 WHERE name = ?1", params![name, pending]))?;
        Ok(())
    }

    /// When cluster `name` must load again to run a background job that comes due (Unix ms;
    /// `None`: no job, or it is loaded and its scheduler runs them).
    pub async fn set_wake_at(&self, name: &str, at: Option<i64>) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET wake_at = ?2 WHERE name = ?1", params![name, at]))?;
        Ok(())
    }

    /// Sleeping clusters with a background job due by `now_ms`.
    pub async fn due_wakes(&self, now_ms: i64) -> anyhow::Result<Vec<SearchCluster>> {
        let sql = format!("SELECT {READ_COLS} FROM search_clusters WHERE {LIVE} AND wake_at IS NOT NULL AND wake_at <= ?1 ORDER BY wake_at");
        let mut v = self.db.read(|x| x.query(&sql, params![now_ms], row))?;
        self.reveal(v.iter_mut())?;
        Ok(v)
    }

    /// Hides a cluster from lookups while its data is deleted (`on`), or shows it again (the
    /// delete failed).
    pub async fn set_deleting(&self, id: &str, on: bool) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET deleting = ?2 WHERE id = ?1", params![id, on]))?;
        Ok(())
    }

    /// Keeps a deleted cluster for restoring until `purge_at`: its name is freed at once (the
    /// row's `name` becomes `stored_as`, where its storage now is), and lookups stop seeing it.
    /// `false` when it isn't a live cluster any more (another delete won).
    pub async fn mark_deleted(&self, id: &str, stored_as: &str, at: i64, purge_at: i64) -> anyhow::Result<bool> {
        let n = self.db.write(|x| {
            x.execute(
                "UPDATE search_clusters SET deleted_name = name, name = ?2, stored_as = ?2, deleted_at = ?3, purge_at = ?4, deleting = 0,
                 loaded_on = NULL, wake_at = NULL, claim = NULL, claimed_at = NULL WHERE id = ?1 AND deleted_at IS NULL",
                params![id, stored_as, at, purge_at],
            )
        })?;
        Ok(n == 1)
    }

    /// Deleted clusters kept for restoring (of `owner` only, when given), newest first.
    pub async fn list_deleted(&self, owner: Option<&str>) -> anyhow::Result<Vec<DeletedCluster>> {
        let sql = format!("SELECT {READ_COLS}, {DELETED_COLS} FROM search_clusters WHERE deleted_at IS NOT NULL {} ORDER BY deleted_at DESC, rowid DESC", if owner.is_some() { "AND owner = ?1" } else { "" });
        let mut v = self.db.read(|x| match owner {
            Some(o) => x.query(&sql, params![o], deleted_row),
            None => x.query(&sql, params![], deleted_row),
        })?;
        self.reveal(v.iter_mut().map(|d| &mut d.cluster))?;
        Ok(v)
    }

    /// The deleted cluster `id`, if it is kept.
    pub async fn get_deleted(&self, id: &str) -> anyhow::Result<Option<DeletedCluster>> {
        let sql = format!("SELECT {READ_COLS}, {DELETED_COLS} FROM search_clusters WHERE id = ?1 AND deleted_at IS NOT NULL");
        let mut d = self.db.read(|x| x.query_row(&sql, params![id], deleted_row).optional())?;
        self.reveal(d.iter_mut().map(|d| &mut d.cluster))?;
        Ok(d)
    }

    /// Deleted clusters whose window ended by `now` (Unix seconds).
    pub async fn due_purges(&self, now: i64) -> anyhow::Result<Vec<DeletedCluster>> {
        let sql = format!("SELECT {READ_COLS}, {DELETED_COLS} FROM search_clusters WHERE deleted_at IS NOT NULL AND purge_at <= ?1 ORDER BY purge_at");
        let mut v = self.db.read(|x| x.query(&sql, params![now], deleted_row))?;
        self.reveal(v.iter_mut().map(|d| &mut d.cluster))?;
        Ok(v)
    }

    /// Claims deleted cluster `id` for a restore (`restore_as`: the name it comes back under,
    /// reserved in the row at once, so no create can take it meanwhile) or a purge (`None`), in
    /// one transaction: a claim another node holds since `stale_before` or later wins. A restore
    /// needs the window still open at `now`.
    pub async fn claim_deleted(&self, id: &str, claim: &str, restore_as: Option<&str>, now: i64, stale_before: i64) -> anyhow::Result<Claim> {
        let r = self.db.write(|x| {
            let cur: Option<(String, Option<i64>, i64)> = x
                .query_row(
                    "SELECT name, claimed_at, purge_at FROM search_clusters WHERE id = ?1 AND deleted_at IS NOT NULL",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((reserved, claimed_at, purge_at)) = cur else { return Ok(Claim::Gone) };
            if claimed_at.is_some_and(|t| t >= stale_before) {
                return Ok(Claim::Busy);
            }
            match restore_as {
                Some(name) => {
                    if purge_at <= now {
                        return Ok(Claim::Gone);
                    }
                    if name != reserved {
                        let taken: Option<i64> = x.query_row("SELECT 1 FROM search_clusters WHERE name = ?1", params![name], |r| r.get(0)).optional()?;
                        if taken.is_some() {
                            return Ok(Claim::Taken);
                        }
                    }
                    x.execute("UPDATE search_clusters SET claim = ?2, claimed_at = ?3, name = ?4 WHERE id = ?1", params![id, claim, now, name])?;
                }
                None => {
                    x.execute("UPDATE search_clusters SET claim = ?2, claimed_at = ?3 WHERE id = ?1", params![id, claim, now])?;
                }
            }
            Ok::<_, code_sql::Error>(Claim::Claimed { reserved })
        });
        match r {
            Ok(c) => Ok(c),
            // A create took the name between the check and the update.
            Err(e) if e.is_unique_violation() => Ok(Claim::Taken),
            Err(e) => Err(e.into()),
        }
    }

    /// Gives a claim up (the restore failed): the row's name goes back to where its storage is.
    pub async fn release_claim(&self, id: &str, claim: &str) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET claim = NULL, claimed_at = NULL, name = stored_as WHERE id = ?1 AND claim = ?2", params![id, claim]))?;
        Ok(())
    }

    /// Makes a restored cluster live again under the name its claim reserved; `false` when the
    /// claim was lost (it went stale and a purge took it).
    /// Brings a deleted cluster's row back if `claim` still holds it, unless its owner is at one
    /// of `caps` (counted in this transaction, as [`Store::insert`] counts them): `Ok(Ok(false))`
    /// when the claim was lost.
    pub async fn finish_restore(&self, id: &str, claim: &str, at: i64, caps: Caps) -> anyhow::Result<Result<bool, Over>> {
        Ok(self.db.write(|x| {
            let row: Option<(Option<String>, i64)> = x
                .query_row("SELECT owner, idle_timeout_s FROM search_clusters WHERE id = ?1 AND claim = ?2 AND deleted_at IS NOT NULL", params![id, claim], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let Some((owner, idle_timeout_s)) = row else { return Ok::<_, code_sql::Error>(Ok(false)) };
            if let Some(owner) = &owner
                && let Some(over) = over_caps(x, owner, idle_timeout_s, caps)?
            {
                return Ok(Err(over));
            }
            let n = x.execute(
                "UPDATE search_clusters SET deleted_at = NULL, purge_at = NULL, deleted_name = NULL, stored_as = NULL, claim = NULL, claimed_at = NULL,
                 restored_at = ?3 WHERE id = ?1 AND claim = ?2 AND deleted_at IS NOT NULL",
                params![id, claim, at],
            )?;
            Ok(Ok(n == 1))
        })?)
    }

    /// Removes a purged cluster's rows, if `claim` still holds it; `false` otherwise.
    pub async fn finish_purge(&self, id: &str, claim: &str) -> anyhow::Result<bool> {
        Ok(self.db.write(|x| {
            if x.execute("DELETE FROM search_clusters WHERE id = ?1 AND claim = ?2 AND deleted_at IS NOT NULL", params![id, claim])? == 0 {
                return Ok::<_, code_sql::Error>(false);
            }
            x.execute("DELETE FROM search_usage WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_storage WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_revokes WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_remotes WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_metrics WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_slowlog WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_analytics WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_suggestions WHERE cluster_id = ?1", params![id])?;
            Ok(true)
        })?)
    }

    /// Keeps `credential` (what `Control::revoke_link` takes) to revoke when deleted cluster `id`
    /// is restored, unless it already keeps `max`. Checked against the row in the same
    /// transaction, so a restore that flips the row live after this reads it
    /// ([`Store::revokes`]), and one that flipped it before makes this [`Recorded::Live`].
    pub async fn record_revoke(&self, id: &str, credential: &str, at: i64, max: i64) -> anyhow::Result<Recorded> {
        Ok(self.db.write(|x| {
            let deleted: Option<Option<i64>> = x.query_row("SELECT deleted_at FROM search_clusters WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
            match deleted {
                None => return Ok::<_, code_sql::Error>(Recorded::Gone),
                Some(None) => return Ok(Recorded::Live),
                Some(Some(_)) => {}
            }
            let kept: Option<i64> = x.query_row("SELECT 1 FROM search_revokes WHERE cluster_id = ?1 AND credential = ?2", params![id, credential], |r| r.get(0)).optional()?;
            if kept.is_some() {
                return Ok(Recorded::Yes);
            }
            let n: i64 = x.query_row("SELECT COUNT(*) FROM search_revokes WHERE cluster_id = ?1", params![id], |r| r.get(0))?;
            if n >= max {
                return Ok(Recorded::Full);
            }
            x.execute("INSERT INTO search_revokes (cluster_id, credential, at) VALUES (?1, ?2, ?3)", params![id, credential, at])?;
            Ok(Recorded::Yes)
        })?)
    }

    /// The revokes live cluster `id` still has to apply ([`Store::record_revoke`]), oldest first.
    /// Read with the row in a write transaction, so it can't miss one recorded before the row
    /// went live.
    pub async fn revokes(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.db.write(|x| {
            x.query(
                "SELECT r.credential FROM search_revokes r JOIN search_clusters c ON c.id = r.cluster_id WHERE r.cluster_id = ?1 AND c.deleting = 0 AND c.deleted_at IS NULL ORDER BY r.at, r.credential",
                params![id],
                |r| r.get(0),
            )
        })?)
    }

    /// Live clusters with revokes still to apply (a restore that couldn't apply them all).
    pub async fn clusters_with_revokes(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.db.read(|x| {
            x.query("SELECT DISTINCT r.cluster_id FROM search_revokes r JOIN search_clusters c ON c.id = r.cluster_id WHERE c.deleting = 0 AND c.deleted_at IS NULL", params![], |r| r.get(0))
        })?)
    }

    /// A revoke applied: forgotten.
    pub async fn forget_revoke(&self, id: &str, credential: &str) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("DELETE FROM search_revokes WHERE cluster_id = ?1 AND credential = ?2", params![id, credential]))?;
        Ok(())
    }

    /// Deletes a cluster's rows.
    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute("DELETE FROM search_clusters WHERE id = ?1", params![id])?;
            x.execute("DELETE FROM search_usage WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_metrics WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_slowlog WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_analytics WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_suggestions WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_storage WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_remotes WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_revokes WHERE cluster_id = ?1", params![id])
        })?;
        Ok(())
    }

    /// Keeps (or replaces) cluster `r.cluster_id`'s credentials for its remote `r.alias`;
    /// returns what it replaced.
    pub async fn put_remote(&self, r: &RemoteLink) -> anyhow::Result<Option<RemoteLink>> {
        Ok(self.db.write(|x| {
            let old = x.query_row(&format!("SELECT {REMOTE_COLS} FROM search_remotes WHERE cluster_id = ?1 AND alias = ?2"), params![r.cluster_id, r.alias], remote_row).optional()?;
            x.execute(
                "INSERT INTO search_remotes (cluster_id, alias, target_id, key_id, credentials, minted, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (cluster_id, alias) DO UPDATE SET target_id = excluded.target_id, key_id = excluded.key_id, credentials = excluded.credentials, minted = excluded.minted, created_at = excluded.created_at",
                params![r.cluster_id, r.alias, r.target_id, r.key_id, r.credentials, r.minted, r.created_at],
            )?;
            Ok::<_, code_sql::Error>(old)
        })?)
    }

    /// Cluster `cluster_id`'s credentials for its remote `alias`.
    pub async fn remote(&self, cluster_id: &str, alias: &str) -> anyhow::Result<Option<RemoteLink>> {
        Ok(self.db.read(|x| x.query_row(&format!("SELECT {REMOTE_COLS} FROM search_remotes WHERE cluster_id = ?1 AND alias = ?2"), params![cluster_id, alias], remote_row).optional())?)
    }

    /// Every remote cluster `cluster_id` holds credentials for, by alias.
    pub async fn remotes(&self, cluster_id: &str) -> anyhow::Result<Vec<RemoteLink>> {
        Ok(self.db.read(|x| x.query(&format!("SELECT {REMOTE_COLS} FROM search_remotes WHERE cluster_id = ?1 ORDER BY alias"), params![cluster_id], remote_row))?)
    }

    /// Forgets cluster `cluster_id`'s credentials for remote `alias`; returns them.
    pub async fn delete_remote(&self, cluster_id: &str, alias: &str) -> anyhow::Result<Option<RemoteLink>> {
        Ok(self.db.write(|x| {
            let old = x.query_row(&format!("SELECT {REMOTE_COLS} FROM search_remotes WHERE cluster_id = ?1 AND alias = ?2"), params![cluster_id, alias], remote_row).optional()?;
            if old.is_some() {
                x.execute("DELETE FROM search_remotes WHERE cluster_id = ?1 AND alias = ?2", params![cluster_id, alias])?;
            }
            Ok::<_, code_sql::Error>(old)
        })?)
    }

    /// Stores what a node handed on of a cluster's search analytics, in one transaction: the
    /// rows of `cleared` indices go (and, when `enabled` lists the indices with analytics on,
    /// those of every other index), then each (index, unix hour, bucket) adds up in the index's
    /// hour and day rings. A slot holding the same bucket adds up; one holding an older bucket is
    /// replaced; one holding a newer one (a node whose clock is behind) is kept.
    pub async fn record_analytics(&self, id: &str, cleared: &[String], enabled: Option<&[String]>, hours: &[(String, u64, search_es::analytics::Bucket)]) -> anyhow::Result<()> {
        if cleared.is_empty() && enabled.is_none() && hours.is_empty() {
            return Ok(());
        }
        self.db.write(|x| {
            // A suggestions row holds query text too (its sample): it goes with the analytics.
            for i in cleared {
                x.execute("DELETE FROM search_analytics WHERE cluster_id = ?1 AND idx = ?2", params![id, i])?;
                x.execute("DELETE FROM search_suggestions WHERE cluster_id = ?1 AND idx = ?2", params![id, i])?;
            }
            if let Some(on) = enabled {
                let mut kept: Vec<String> = x.query("SELECT DISTINCT idx FROM search_analytics WHERE cluster_id = ?1", params![id], |r| r.get(0))?;
                let suggested: Vec<String> = x.query("SELECT idx FROM search_suggestions WHERE cluster_id = ?1", params![id], |r| r.get(0))?;
                kept.extend(suggested);
                for i in kept.iter().filter(|i| !on.contains(i)) {
                    x.execute("DELETE FROM search_analytics WHERE cluster_id = ?1 AND idx = ?2", params![id, i])?;
                    x.execute("DELETE FROM search_suggestions WHERE cluster_id = ?1 AND idx = ?2", params![id, i])?;
                }
            }
            for (idx, hour, bucket) in hours {
                let t = (*hour).min(i64::MAX as u64 / 3600) as i64 * 3600;
                for (res, slots) in [(3600, ANALYTICS_HOUR_SLOTS), (86_400, ANALYTICS_DAY_SLOTS)] {
                    let at = t - t.rem_euclid(res);
                    let slot = (at / res).rem_euclid(slots);
                    let (top, empty) = analytics_caps(res);
                    let kept: Option<(i64, String)> = x
                        .query_row("SELECT at, data FROM search_analytics WHERE cluster_id = ?1 AND idx = ?2 AND res = ?3 AND slot = ?4", params![id, idx, res, slot], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    let mut merged = match kept {
                        Some((a, data)) if a == at => search_es::analytics::Bucket::decode(&data, top, empty),
                        Some((a, _)) if a > at => continue,
                        _ => search_es::analytics::Bucket::new(top, empty),
                    };
                    merged.merge(bucket);
                    x.execute(
                        "INSERT INTO search_analytics (cluster_id, idx, res, slot, at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT (cluster_id, idx, res, slot) DO UPDATE SET at = excluded.at, data = excluded.data",
                        params![id, idx, res, slot, at, merged.encode()],
                    )?;
                }
            }
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    /// A cluster's analytics buckets of `res` seconds (3600 or 86400) from `since` (unix
    /// seconds), of index `idx` or of every index: (index, start, bucket), oldest first.
    pub async fn analytics_since(&self, id: &str, idx: Option<&str>, res: i64, since: i64) -> anyhow::Result<Vec<(String, i64, search_es::analytics::Bucket)>> {
        let rows: Vec<(String, i64, String)> = self.db.read(|x| match idx {
            Some(i) => x.query(
                "SELECT idx, at, data FROM search_analytics WHERE cluster_id = ?1 AND idx = ?2 AND res = ?3 AND at >= ?4 ORDER BY at",
                params![id, i, res, since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ),
            None => x.query("SELECT idx, at, data FROM search_analytics WHERE cluster_id = ?1 AND res = ?2 AND at >= ?3 ORDER BY at", params![id, res, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))),
        })?;
        let (top, empty) = analytics_caps(res);
        Ok(rows.into_iter().map(|(i, at, d)| (i, at, search_es::analytics::Bucket::decode(&d, top, empty))).collect())
    }

    /// The indices a cluster keeps analytics of.
    pub async fn analytics_indices(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.db.read(|x| x.query("SELECT DISTINCT idx FROM search_analytics WHERE cluster_id = ?1 ORDER BY idx", params![id], |r| r.get(0)))?)
    }

    /// A cluster's query suggestion rows (`crate::suggestions`), by index.
    pub async fn suggestions(&self, id: &str) -> anyhow::Result<Vec<SuggestionsRow>> {
        let sql = format!("SELECT {SUGGESTION_COLS} FROM search_suggestions WHERE cluster_id = ?1 ORDER BY idx");
        Ok(self.db.read(|x| x.query(&sql, params![id], suggestions_row))?)
    }

    /// Index `idx`'s query suggestion row.
    pub async fn suggestion(&self, id: &str, idx: &str) -> anyhow::Result<Option<SuggestionsRow>> {
        let sql = format!("SELECT {SUGGESTION_COLS} FROM search_suggestions WHERE cluster_id = ?1 AND idx = ?2");
        Ok(self.db.read(|x| x.query_row(&sql, params![id, idx], suggestions_row).optional())?)
    }

    /// Writes a query suggestion row whole.
    pub async fn put_suggestion(&self, id: &str, r: &SuggestionsRow) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute(
                "INSERT INTO search_suggestions (cluster_id, idx, config, due_at, attempted_at, built_at, built_index, count, took_ms, digest, error, sample)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (cluster_id, idx) DO UPDATE SET config = excluded.config, due_at = excluded.due_at, attempted_at = excluded.attempted_at,
                   built_at = excluded.built_at, built_index = excluded.built_index, count = excluded.count, took_ms = excluded.took_ms,
                   digest = excluded.digest, error = excluded.error, sample = excluded.sample",
                params![id, r.index, r.config, r.due_at, r.attempted_at, r.built_at, r.built_index, r.count, r.took_ms, r.digest, r.error, r.sample],
            )
        })?;
        Ok(())
    }

    /// Forgets index `idx`'s query suggestions (turned off, or the index is gone).
    pub async fn delete_suggestion(&self, id: &str, idx: &str) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("DELETE FROM search_suggestions WHERE cluster_id = ?1 AND idx = ?2", params![id, idx]))?;
        Ok(())
    }

    /// Sets when index `idx`'s suggestions are next due (ms), and when they were last looked
    /// at when `attempted_at` is given (a check that found nothing to change).
    pub async fn set_suggestion_due(&self, id: &str, idx: &str, due_at: i64, attempted_at: Option<i64>) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute(
                "UPDATE search_suggestions SET due_at = ?3, attempted_at = COALESCE(?4, attempted_at) WHERE cluster_id = ?1 AND idx = ?2",
                params![id, idx, due_at, attempted_at],
            )
        })?;
        Ok(())
    }

    /// Suggestion rows of live clusters due by `now_ms`, soonest first: (cluster id, row).
    pub async fn due_suggestions(&self, now_ms: i64) -> anyhow::Result<Vec<(String, SuggestionsRow)>> {
        let cols: String = SUGGESTION_COLS.split(", ").map(|c| format!("s.{c}")).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT s.cluster_id, {cols} FROM search_suggestions s JOIN search_clusters c ON c.id = s.cluster_id
             WHERE c.deleting = 0 AND c.deleted_at IS NULL AND s.due_at <= ?1 ORDER BY s.due_at LIMIT 100"
        );
        Ok(self.db.read(|x| {
            x.query(&sql, params![now_ms], |r| {
                let id: String = r.get(0)?;
                Ok((id, suggestions_row_at(r, 1)?))
            })
        })?)
    }

    /// The latest figures metering saw. A change of `stored_bytes` is logged at `at` for the
    /// storage integral (one row per change: the update locks the cluster's row first, so nodes
    /// metering it at once during a handoff take turns).
    pub async fn set_stats(&self, id: &str, at: i64, indices: i64, docs: i64, stored_bytes: i64) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute("UPDATE search_clusters SET indices = ?2, docs = ?3, stored_bytes = ?4 WHERE id = ?1", params![id, indices, docs, stored_bytes])?;
            let last: Option<i64> =
                x.query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC LIMIT 1", params![id], |r| r.get(0)).optional()?;
            if last != Some(stored_bytes) {
                x.execute("INSERT INTO search_storage (cluster_id, at, bytes) VALUES (?1, ?2, ?3)", params![id, at, stored_bytes])?;
            }
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    /// [`SearchCluster::own_bytes`], as metering last saw it.
    pub async fn set_own_bytes(&self, id: &str, own_bytes: i64) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET own_bytes = ?2 WHERE id = ?1", params![id, own_bytes]).map(drop))?;
        Ok(())
    }

    pub async fn record_usage(&self, id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute(
                "INSERT INTO search_usage (cluster_id, at, seconds, loaded_bytes, requests, inference_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, u.at, u.seconds, u.loaded_bytes, u.requests, u.inference_ms],
            )
        })?;
        Ok(())
    }

    /// Adds minutes of request metrics (unix minute, what it saw) to the cluster's minute and
    /// hour rings, in one transaction: a slot holding the same bucket adds up, one holding an
    /// older bucket (a day or two weeks ago) is replaced.
    pub async fn record_metrics(&self, id: &str, minutes: &[(u64, Stats)]) -> anyhow::Result<()> {
        if minutes.is_empty() {
            return Ok(());
        }
        self.db.write(|x| {
            for (minute, stats) in minutes {
                let t = (*minute).min(i64::MAX as u64 / 60) as i64 * 60;
                for (res, slots) in [(60, MINUTE_SLOTS), (3600, HOUR_SLOTS)] {
                    let at = t - t.rem_euclid(res);
                    let slot = (at / res).rem_euclid(slots);
                    let kept: Option<(i64, String)> = x
                        .query_row("SELECT at, data FROM search_metrics WHERE cluster_id = ?1 AND res = ?2 AND slot = ?3", params![id, res, slot], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    let mut merged = match kept {
                        Some((a, data)) if a == at => Stats::decode(&data),
                        // An older bucket's slot (or a newer one's, from a node whose clock is
                        // ahead: the newer is kept).
                        Some((a, _)) if a > at => continue,
                        _ => Stats::default(),
                    };
                    merged.merge(stats);
                    x.execute(
                        "INSERT INTO search_metrics (cluster_id, res, slot, at, data) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT (cluster_id, res, slot) DO UPDATE SET at = excluded.at, data = excluded.data",
                        params![id, res, slot, at, merged.encode()],
                    )?;
                }
            }
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    /// The cluster's buckets of `res` seconds (60 or 3600) from `since` (unix seconds), oldest
    /// first.
    pub async fn metrics_since(&self, id: &str, res: i64, since: i64) -> anyhow::Result<Vec<(i64, Stats)>> {
        let rows: Vec<(i64, String)> = self.db.read(|x| {
            x.query("SELECT at, data FROM search_metrics WHERE cluster_id = ?1 AND res = ?2 AND at >= ?3 ORDER BY at", params![id, res, since], |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
        Ok(rows.into_iter().map(|(at, d)| (at, Stats::decode(&d))).collect())
    }

    /// Appends slow log entries (unix ms, the entry as JSON), oldest first, and keeps the
    /// cluster's newest [`search_es::slowlog::MAX_ENTRIES`].
    pub async fn record_slowlog(&self, id: &str, entries: &[(i64, String)]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let keep = search_es::slowlog::MAX_ENTRIES as i64;
        self.db.write(|x| {
            for (at, data) in entries {
                x.execute("INSERT INTO search_slowlog (cluster_id, at, data) VALUES (?1, ?2, ?3)", params![id, at, data])?;
            }
            // The oldest past the newest `keep`.
            let edge: Option<(i64, i64)> = x
                .query_row(
                    "SELECT at, rowid FROM search_slowlog WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC LIMIT 1 OFFSET ?2",
                    params![id, keep - 1],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((at, rowid)) = edge {
                x.execute("DELETE FROM search_slowlog WHERE cluster_id = ?1 AND (at < ?2 OR (at = ?2 AND rowid < ?3))", params![id, at, rowid])?;
            }
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    /// The cluster's slow log entries, newest first.
    pub async fn slowlog(&self, id: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.db.read(|x| x.query("SELECT data FROM search_slowlog WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC", params![id], |r| r.get(0)))?)
    }

    /// Totals over `[since, now]`.
    pub async fn usage_since(&self, id: &str, since: i64, now: i64) -> anyhow::Result<UsageTotals> {
        let (seconds, byte_seconds, requests, peak, inference_ms): (i64, f64, i64, i64, i64) = self.db.read(|x| {
            x.query_row(
                "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(CAST(seconds AS REAL) * loaded_bytes), 0.0), COALESCE(SUM(requests), 0),
                        COALESCE(MAX(loaded_bytes), 0), COALESCE(SUM(inference_ms), 0)
                 FROM search_usage WHERE cluster_id = ?1 AND at >= ?2",
                params![id, since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })?;
        Ok(UsageTotals {
            loaded_seconds: seconds,
            loaded_gib_hours: byte_seconds / GIB / 3600.0,
            requests,
            peak_loaded_bytes: peak,
            inference_cpu_seconds: inference_ms as f64 / 1000.0,
            stored_gib_months: self.storage_byte_seconds(id, since, now).await? / GIB / MONTH_SECONDS,
        })
    }

    /// The integral of stored bytes over `[since, now]`: the size in effect at `since` (the last
    /// change before it) and every change after.
    pub async fn storage_byte_seconds(&self, id: &str, since: i64, now: i64) -> anyhow::Result<f64> {
        let points: Vec<(i64, i64)> = self.db.read(|x| {
            let before: Option<i64> = x
                .query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 AND at < ?2 ORDER BY at DESC, rowid DESC LIMIT 1", params![id, since], |r| r.get(0))
                .optional()?;
            let mut v: Vec<(i64, i64)> = before.map(|b| (since, b)).into_iter().collect();
            v.extend(x.query("SELECT at, bytes FROM search_storage WHERE cluster_id = ?1 AND at >= ?2 AND at <= ?3 ORDER BY at, rowid", params![id, since, now], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?);
            Ok::<_, code_sql::Error>(v)
        })?;
        Ok(integrate(&points, now))
    }
}

/// Whether `owner` may have one more always-on cluster: `None` unless `idle_timeout_s` is 0, the
/// cap is set, and (for an existing cluster `id`) it isn't always-on already, so a cluster that
/// already is keeps its other settings changeable when the cap is lowered.
/// Whether one more cluster of `owner` (with `idle_timeout_s`) is past one of `caps`.
fn over_caps(x: &code_sql::Conn<'_>, owner: &str, idle_timeout_s: i64, caps: Caps) -> code_sql::Result<Option<Over>> {
    if caps.clusters > 0 {
        let count: i64 = x.query_row(&format!("SELECT COUNT(*) FROM search_clusters WHERE {OWNED}"), params![owner], |r| r.get(0))?;
        if count >= caps.clusters {
            return Ok(Some(Over::Clusters { cap: caps.clusters, count }));
        }
    }
    over_always_on(x, owner, None, idle_timeout_s, caps)
}

fn over_always_on(x: &code_sql::Conn<'_>, owner: &str, id: Option<&str>, idle_timeout_s: i64, caps: Caps) -> code_sql::Result<Option<Over>> {
    if idle_timeout_s != 0 || caps.always_on <= 0 {
        return Ok(None);
    }
    if let Some(id) = id {
        let was: Option<i64> = x.query_row("SELECT idle_timeout_s FROM search_clusters WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
        if was == Some(0) {
            return Ok(None);
        }
    }
    let count: i64 = x.query_row(&format!("SELECT COUNT(*) FROM search_clusters WHERE {OWNED} AND idle_timeout_s = 0"), params![owner], |r| r.get(0))?;
    Ok((count >= caps.always_on).then_some(Over::AlwaysOn { cap: caps.always_on, count }))
}

/// The integral of a step function given as `(from, value)` points (sorted by time) up to `end`.
pub fn integrate(points: &[(i64, i64)], end: i64) -> f64 {
    let mut total = 0.0;
    for (i, &(at, v)) in points.iter().enumerate() {
        let until = points.get(i + 1).map_or(end, |p| p.0).min(end);
        if until > at {
            total += (until - at) as f64 * v as f64;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn cluster(id: &str, name: &str) -> SearchCluster {
        SearchCluster {
            id: id.into(),
            name: name.into(),
            region: "local".into(),
            flavor: "elasticsearch".into(),
            password: "pw".into(),
            idle_timeout_s: 300,
            max_in_flight: 64,
            max_body_mb: 100,
            destructive_requires_name: true,
            parent_id: None,
            parent_at: None,
            created_at: 1,
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
        }
    }

    async fn crud(s: &Store) {
        let c = cluster("srch_a", "app");
        assert_eq!(s.insert(&c, Caps::NONE).await.unwrap(), Inserted::Yes);
        assert_eq!(s.insert(&c, Caps::NONE).await.unwrap(), Inserted::Taken, "ids are unique");
        assert_eq!(s.insert(&cluster("srch_b", "app"), Caps::NONE).await.unwrap(), Inserted::Taken, "names are unique");
        assert_eq!(s.by_name("app").await.unwrap().unwrap().id, "srch_a");
        let mut child = cluster("srch_c", "app-dev");
        child.parent_id = Some("srch_a".into());
        child.parent_at = Some(5);
        child.created_at = 2;
        assert_eq!(s.insert(&child, Caps::NONE).await.unwrap(), Inserted::Yes);
        assert_eq!(s.children("srch_a").await.unwrap().len(), 1);
        let mut mine = cluster("srch_m", "mine");
        mine.owner = Some("usr_a".into());
        mine.created_at = 3;
        assert_eq!(s.insert(&mine, Caps::NONE).await.unwrap(), Inserted::Yes);
        assert_eq!(s.list_owned("usr_a").await.unwrap().iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["srch_m"]);
        assert!(s.list_owned("usr_b").await.unwrap().is_empty());
        assert_eq!(s.owner_of("mine").unwrap(), Some(Some("usr_a".into())));
        assert_eq!(s.owner_of("srch_a").unwrap(), Some(None));
        assert_eq!(s.owner_of("nope").unwrap(), None);
        s.delete("srch_m").await.unwrap();
        let mut c = s.get("srch_a").await.unwrap().unwrap();
        c.flavor = "opensearch".into();
        c.password = "new".into();
        c.max_in_flight = 8;
        s.update(&c, Caps::NONE).await.unwrap().unwrap();
        let back = s.get("srch_a").await.unwrap().unwrap();
        assert_eq!((back.flavor.as_str(), back.password.as_str(), back.max_in_flight), ("opensearch", "new", 8));
        assert_eq!(s.list().await.unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["app", "app-dev"]);
        assert!(s.always_on().await.unwrap().is_empty());
        c.idle_timeout_s = 0;
        s.update(&c, Caps::NONE).await.unwrap().unwrap();
        assert_eq!(s.always_on().await.unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["app"]);
        s.set_deleting("srch_a", true).await.unwrap();
        assert!(s.always_on().await.unwrap().is_empty(), "a cluster being deleted is never loaded");
        s.set_deleting("srch_a", false).await.unwrap();
        s.delete("srch_a").await.unwrap();
        assert!(s.get("srch_a").await.unwrap().is_none());
        s.delete("srch_c").await.unwrap();
    }

    async fn usage_totals(s: &Store) {
        assert_eq!(s.insert(&cluster("srch_u", "usage"), Caps::NONE).await.unwrap(), Inserted::Yes);
        s.record_usage("srch_u", &UsageSample { at: 100, seconds: 60, loaded_bytes: 1 << 30, requests: 10, inference_ms: 1500 }).await.unwrap();
        s.record_usage("srch_u", &UsageSample { at: 160, seconds: 30, loaded_bytes: 2 << 30, requests: 5, inference_ms: 0 }).await.unwrap();
        let t = s.usage_since("srch_u", 0, 200).await.unwrap();
        assert_eq!((t.loaded_seconds, t.requests, t.peak_loaded_bytes, t.inference_cpu_seconds), (90, 15, 2 << 30, 1.5));
        assert!((t.loaded_gib_hours - (60.0 + 60.0) / 3600.0).abs() < 1e-12);
        assert_eq!(s.usage_since("srch_u", 150, 200).await.unwrap().requests, 5);
        s.delete("srch_u").await.unwrap();
        assert_eq!(s.usage_since("srch_u", 0, 200).await.unwrap().requests, 0);
    }

    async fn storage_is_a_step_function(s: &Store) {
        assert_eq!(s.insert(&cluster("srch_s", "storage"), Caps::NONE).await.unwrap(), Inserted::Yes);
        s.set_stats("srch_s", 100, 1, 10, 1000).await.unwrap();
        // An unchanged size logs nothing.
        s.set_stats("srch_s", 150, 1, 12, 1000).await.unwrap();
        s.set_stats("srch_s", 200, 2, 20, 3000).await.unwrap();
        assert_eq!(s.get("srch_s").await.unwrap().unwrap().docs, 20);
        // 100..200 at 1000, 200..300 at 3000.
        assert_eq!(s.storage_byte_seconds("srch_s", 0, 300).await.unwrap(), 100.0 * 1000.0 + 100.0 * 3000.0);
        // A window starting mid-step uses the size in effect then.
        assert_eq!(s.storage_byte_seconds("srch_s", 150, 300).await.unwrap(), 50.0 * 1000.0 + 100.0 * 3000.0);
        assert_eq!(s.storage_byte_seconds("srch_s", 250, 300).await.unwrap(), 50.0 * 3000.0);
        let months = s.usage_since("srch_s", 0, 300).await.unwrap().stored_gib_months;
        assert!((months - 400_000.0 / GIB / MONTH_SECONDS).abs() < 1e-18);
        s.delete("srch_s").await.unwrap();
    }

    /// Restore and purge exclude each other through the claim, whichever node runs them.
    async fn deleted_claims(s: &Store) {
        assert_eq!(s.insert(&cluster("srch_d", "gone"), Caps::NONE).await.unwrap(), Inserted::Yes);
        assert!(s.mark_deleted("srch_d", "deleted_srch_d_x", 100, 200).await.unwrap());
        assert!(!s.mark_deleted("srch_d", "deleted_srch_d_y", 100, 200).await.unwrap(), "deleted once");
        assert!(s.by_name("gone").await.unwrap().is_none() && s.get("srch_d").await.unwrap().is_none());
        assert_eq!(s.id_of("srch_d").unwrap().as_deref(), Some("srch_d"), "its restore route resolves");
        assert_eq!(s.id_of("gone").unwrap(), None);
        assert_eq!(s.insert(&cluster("srch_n", "gone"), Caps::NONE).await.unwrap(), Inserted::Yes, "the name is free");
        assert_eq!(s.list_deleted(None).await.unwrap()[0].cluster.name, "gone");
        assert_eq!(s.claim_deleted("srch_d", "r1", Some("gone"), 150, 0).await.unwrap(), Claim::Taken);
        assert_eq!(
            s.claim_deleted("srch_d", "r1", Some("back"), 150, 0).await.unwrap(),
            Claim::Claimed { reserved: "deleted_srch_d_x".into() }
        );
        // The restore holds it: a purge (or another restore) waits; the reserved name is taken.
        assert_eq!(s.claim_deleted("srch_d", "p1", None, 250, 100).await.unwrap(), Claim::Busy);
        assert_eq!(s.insert(&cluster("srch_o", "back"), Caps::NONE).await.unwrap(), Inserted::Taken);
        assert!(!s.finish_purge("srch_d", "p1").await.unwrap());
        // A claim older than `stale_before` lapses: the purge takes it and the restore loses.
        assert_eq!(s.claim_deleted("srch_d", "p1", None, 250, 151).await.unwrap(), Claim::Claimed { reserved: "back".into() });
        assert!(!s.finish_restore("srch_d", "r1", 260, Caps::NONE).await.unwrap().unwrap());
        assert!(s.finish_purge("srch_d", "p1").await.unwrap());
        assert_eq!(s.claim_deleted("srch_d", "r2", Some("back"), 150, 0).await.unwrap(), Claim::Gone);
        // A restore after the window is too late; within it, it finishes.
        assert!(s.mark_deleted("srch_n", "deleted_srch_n_x", 100, 200).await.unwrap());
        assert_eq!(s.claim_deleted("srch_n", "r", Some("gone"), 200, 0).await.unwrap(), Claim::Gone);
        assert!(matches!(s.claim_deleted("srch_n", "r", Some("gone"), 199, 0).await.unwrap(), Claim::Claimed { .. }));
        assert!(s.finish_restore("srch_n", "r", 199, Caps::NONE).await.unwrap().unwrap());
        assert_eq!(s.by_name("gone").await.unwrap().unwrap().restored_at, Some(199));
        s.delete("srch_n").await.unwrap();
    }

    async fn metrics_and_slowlog_rings(s: &Store) {
        use search_server::metrics::{Kind, Recorder};
        assert_eq!(s.insert(&cluster("srch_r", "rings"), Caps::NONE).await.unwrap(), Inserted::Yes);
        let minute = |m: u64, n: u64| {
            let r = Recorder::default();
            for _ in 0..n {
                r.record(std::time::UNIX_EPOCH + std::time::Duration::from_secs(m * 60), Kind::Search, 200, Some(std::time::Duration::from_millis(2)));
            }
            r.take().pop().map(|(m, st)| (m, *st)).unwrap()
        };
        // Day 0, minute 10, twice (two passes): the slot adds up; the hour too.
        let m10 = 1_000_000_000 / 60 / 1440 * 1440 + 10;
        s.record_metrics("srch_r", &[minute(m10, 2)]).await.unwrap();
        s.record_metrics("srch_r", &[minute(m10, 3), minute(m10 + 1, 1)]).await.unwrap();
        let mins = s.metrics_since("srch_r", 60, 0).await.unwrap();
        assert_eq!(mins.iter().map(|(at, st)| (*at, st.all().requests())).collect::<Vec<_>>(), [(m10 as i64 * 60, 5), (m10 as i64 * 60 + 60, 1)]);
        let hours = s.metrics_since("srch_r", 3600, 0).await.unwrap();
        assert_eq!(hours.iter().map(|(_, st)| st.all().requests()).sum::<u64>(), 6);
        // A day later the same minute slot is replaced, not added to: the ring stays fixed.
        s.record_metrics("srch_r", &[minute(m10 + 1440, 7)]).await.unwrap();
        let mins = s.metrics_since("srch_r", 60, 0).await.unwrap();
        assert_eq!(mins.iter().map(|(at, st)| (*at, st.all().requests())).collect::<Vec<_>>(), [(m10 as i64 * 60 + 60, 1), ((m10 + 1440) as i64 * 60, 7)]);
        // A late write for a bucket older than the slot's is dropped.
        s.record_metrics("srch_r", &[minute(m10, 9)]).await.unwrap();
        assert_eq!(s.metrics_since("srch_r", 60, 0).await.unwrap().len(), 2);
        // Slow log: the newest MAX_ENTRIES stay, newest first.
        let max = search_es::slowlog::MAX_ENTRIES as i64;
        let entries: Vec<(i64, String)> = (0..max + 30).map(|i| (1000 + i / 2, format!("e{i}"))).collect();
        s.record_slowlog("srch_r", &entries[..40]).await.unwrap();
        s.record_slowlog("srch_r", &entries[40..]).await.unwrap();
        let kept = s.slowlog("srch_r").await.unwrap();
        assert_eq!(kept.len(), max as usize);
        assert_eq!((kept[0].as_str(), kept[kept.len() - 1].as_str()), (&*format!("e{}", max + 29), "e30"));
        s.delete("srch_r").await.unwrap();
        assert!(s.metrics_since("srch_r", 60, 0).await.unwrap().is_empty() && s.slowlog("srch_r").await.unwrap().is_empty());
    }

    async fn analytics_rings(s: &Store) {
        use search_es::analytics::Bucket;
        assert_eq!(s.insert(&cluster("srch_a", "analytics"), Caps::NONE).await.unwrap(), Inserted::Yes);
        let hour = |n: u64| {
            let mut b = Bucket::new(4, 4);
            b.searches = n;
            b
        };
        let h0 = 1_000_000_000 / 86_400 * 24;
        s.record_analytics("srch_a", &[], None, &[("a".into(), h0, hour(2)), ("b".into(), h0, hour(1))]).await.unwrap();
        s.record_analytics("srch_a", &[], None, &[("a".into(), h0, hour(3)), ("a".into(), h0 + 1, hour(1))]).await.unwrap();
        let hours = s.analytics_since("srch_a", Some("a"), 3600, 0).await.unwrap();
        assert_eq!(hours.iter().map(|(_, at, b)| (*at, b.searches)).collect::<Vec<_>>(), [(h0 as i64 * 3600, 5), (h0 as i64 * 3600 + 3600, 1)]);
        let days = s.analytics_since("srch_a", None, 86_400, 0).await.unwrap();
        assert_eq!(days.iter().map(|(_, _, b)| b.searches).sum::<u64>(), 7);
        // A week later the hour's slot is replaced; the day ring still has the day.
        s.record_analytics("srch_a", &[], None, &[("a".into(), h0 + 168, hour(9))]).await.unwrap();
        assert_eq!(s.analytics_since("srch_a", Some("a"), 3600, 0).await.unwrap().iter().map(|(_, _, b)| b.searches).collect::<Vec<_>>(), [1, 9]);
        // A clear drops the index's rows; an enabled list drops every other index's.
        s.record_analytics("srch_a", &["a".into()], None, &[]).await.unwrap();
        assert_eq!(s.analytics_indices("srch_a").await.unwrap(), ["b"]);
        s.record_analytics("srch_a", &[], Some(&[]), &[]).await.unwrap();
        assert!(s.analytics_indices("srch_a").await.unwrap().is_empty());
        s.record_analytics("srch_a", &[], None, &[("c".into(), h0, hour(1))]).await.unwrap();
        s.delete("srch_a").await.unwrap();
        assert!(s.analytics_indices("srch_a").await.unwrap().is_empty());
    }

    async fn all(s: &Store) {
        crud(s).await;
        deleted_claims(s).await;
        usage_totals(s).await;
        storage_is_a_step_function(s).await;
        metrics_and_slowlog_rings(s).await;
        analytics_rings(s).await;
    }

    #[tokio::test]
    async fn catalog() {
        // SQLite, or Postgres when code-sql's test database variable names one.
        all(&Store::memory().unwrap()).await;
    }

    /// A `search.db` the earlier catalog wrote (its own migrations, change counters) opens and
    /// keeps its clusters.
    #[tokio::test]
    async fn opens_an_earlier_catalog() {
        let dir = std::env::temp_dir().join(format!("search-control-store-{}-{}", std::process::id(), crate::meter::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE search_clusters (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
                  password TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
                  max_body_mb INTEGER NOT NULL, destructive_requires_name INTEGER NOT NULL,
                  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL, password_rotated_at INTEGER,
                  indices INTEGER NOT NULL DEFAULT 0, docs INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0,
                  seq INTEGER NOT NULL DEFAULT 0, deleting INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE search_tombstones (name TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO search_clusters (id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                 parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes) \
                 VALUES ('srch_o', 'old', 'local', 'elasticsearch', 'pw', 300, 64, 100, 1, NULL, NULL, 1, NULL, 0, 0, 0)",
                [],
            )
            .unwrap();
        }
        if std::env::var(code_sql::TEST_DATABASE_ENV).is_ok_and(|v| !v.is_empty()) {
            return;
        }
        let s = Store::open(&path).unwrap();
        let old = s.by_name("old").await.unwrap().unwrap();
        assert_eq!((old.id.as_str(), old.owner.as_deref()), ("srch_o", None), "an earlier cluster is the operator's");
        s.delete("srch_o").await.unwrap();
        all(&s).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a key, a password is sealed in its row and bound to it; a row written in the clear
    /// is sealed as it's read.
    #[tokio::test]
    async fn passwords_are_sealed_in_their_rows() {
        let s = Store::memory().unwrap();
        let stored = |id: &str| s.db.read(|x| x.query_row("SELECT password FROM search_clusters WHERE id = ?1", params![id], |r| r.get::<_, String>(0))).unwrap();
        // Written before the node had a key: in the clear.
        s.insert(&cluster("srch_l", "legacy"), Caps::NONE).await.unwrap();
        assert_eq!(stored("srch_l"), "pw");
        s.seal_passwords(Sealer::from_secret("0123456789abcdef0123456789abcdef").unwrap());
        let mut c = cluster("srch_s", "sealed");
        c.password = "hunter2-hunter2".into();
        s.insert(&c, Caps::NONE).await.unwrap();
        assert!(is_sealed(&stored("srch_s")) && !stored("srch_s").contains("hunter2"));
        assert_eq!(s.get("srch_s").await.unwrap().unwrap().password, "hunter2-hunter2");

        assert_eq!(s.by_name("legacy").await.unwrap().unwrap().password, "pw");
        let migrated = stored("srch_l");
        assert!(is_sealed(&migrated), "sealed on its first read");
        assert_eq!(s.list().await.unwrap().iter().map(|c| c.password.as_str()).collect::<Vec<_>>(), ["pw", "hunter2-hunter2"]);
        assert_eq!(stored("srch_l"), migrated, "sealed once");

        // Another cluster's sealed password doesn't open in this row.
        let moved = stored("srch_s");
        s.db.write(|x| x.execute("UPDATE search_clusters SET password = ?2 WHERE id = ?1", params!["srch_l", moved])).unwrap();
        let e = s.get("srch_l").await.unwrap_err().to_string();
        assert!(e.contains("srch_l") && !e.contains("hunter2"), "{e}");
    }

    #[test]
    fn debug_hides_the_password() {
        let mut c = cluster("srch_d", "debug");
        c.password = "hunter2-hunter2".into();
        let printed = format!("{c:?}");
        assert!(printed.contains("srch_d") && !printed.contains("hunter2"), "{printed}");
    }

    #[test]
    fn integration_edges() {
        assert_eq!(integrate(&[], 10), 0.0);
        assert_eq!(integrate(&[(0, 5)], 10), 50.0);
        assert_eq!(integrate(&[(0, 5), (4, 0)], 10), 20.0);
        // Points past the end don't count.
        assert_eq!(integrate(&[(0, 1), (20, 100)], 10), 10.0);
        assert_eq!(integrate(&[(5, 1)], 3), 0.0);
    }
}
